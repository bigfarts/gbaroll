//! Full-mesh WebRTC bring-up. After the signaling server broadcasts
//! `Starting`, every peer builds one `PeerConnection` per other player:
//! the lower player index makes the offer, descriptions and trickled ICE
//! candidates relay through the server as opaque [`PeerSignal`]s, and
//! each edge finishes with a version handshake over its reliable control
//! channel (opened-barrier first — the web has no blocking first send).

use std::collections::HashMap;

use anyhow::Context as _;
use futures::stream::{FuturesUnordered, SelectAll};
use futures::{FutureExt, StreamExt};
use gbaroll_signaling::{server_message, ClientMessage, IceServer, ServerMessage};
use gloo_timers::future::TimeoutFuture;

use crate::net::protocol::{PeerControl, PeerSignal, BOOT_CHUNK, MAX_BOOT_SIZE, NET_VERSION};
use crate::net::webrtc::{self, ChannelReceiver, ChannelSender, PeerEvent};
use crate::net::ws::SignalSocket;

/// One connected mesh edge, ready for a session.
pub struct PeerLink {
    pub player: usize,
    /// Keeps the connection alive; dropping it tears the edge down.
    pub pc: webrtc::PeerConnection,
    pub ctl_tx: ChannelSender,
    pub ctl_rx: ChannelReceiver,
    pub data_tx: ChannelSender,
    pub data_rx: ChannelReceiver,
}

/// How long the whole mesh has to come up before we give up.
const MESH_TIMEOUT_MS: u32 = 60_000;

/// How long a still-pending edge survives its peer's signaling socket
/// going away. A peer that finished *its* mesh legitimately leaves the
/// server while our handshake reply is still in flight — the transport
/// is already up, so the handshake completes without signaling; only a
/// peer that actually died leaves the edge dangling past this.
const DEPARTED_GRACE_MS: f64 = 10_000.0;

struct Pending {
    pc: webrtc::PeerConnection,
    remote_description_set: bool,
    queued_candidates: Vec<String>,
}

/// Build the mesh over the signaling socket still connected to the
/// started room.
pub async fn build(
    socket: &mut SignalSocket,
    local_player: usize,
    num_players: usize,
    ice_servers: &[IceServer],
) -> anyhow::Result<Vec<PeerLink>> {
    futures::select! {
        links = build_inner(socket, local_player, num_players, ice_servers).fuse() => links,
        _ = TimeoutFuture::new(MESH_TIMEOUT_MS).fuse() => {
            anyhow::bail!("timed out connecting to peers")
        }
    }
}

async fn build_inner(
    socket: &mut SignalSocket,
    local_player: usize,
    num_players: usize,
    ice_servers: &[IceServer],
) -> anyhow::Result<Vec<PeerLink>> {
    let mut pendings: HashMap<usize, Pending> = HashMap::new();
    // Per-peer connection events (trickled candidates, failures), merged.
    let mut events: SelectAll<_> = SelectAll::new();
    let mut handshakes = FuturesUnordered::new();

    log::info!("mesh ICE servers: {ice_servers:?}");
    for player in (0..num_players).filter(|&p| p != local_player) {
        let parts = webrtc::new(ice_servers).context("create peer connection")?;
        events.push(parts.events.map(move |ev| (player, ev)));

        // Deterministic roles: the lower index offers.
        if local_player < player {
            let sdp = parts.pc.create_offer().await?;
            send_signal(
                socket,
                player,
                &PeerSignal::Description {
                    sdp_type: "offer".to_string(),
                    sdp,
                },
            )?;
        }

        // The version handshake, gated on the control channel opening.
        let ctl_tx = parts.ctl_tx;
        let mut ctl_rx = parts.ctl_rx;
        let ctl_open = parts.ctl_open;
        let data_tx = parts.data_tx;
        let data_rx = parts.data_rx;
        handshakes.push(
            async move {
                ctl_open
                    .await
                    .map_err(|_| anyhow::anyhow!("control channel closed before opening"))?;
                let hello = bincode::serialize(&PeerControl::Hello {
                    net_version: NET_VERSION,
                })?;
                ctl_tx.send(&hello).context("send hello")?;
                let reply = ctl_rx
                    .receive()
                    .await
                    .context("control channel closed during handshake")?;
                match bincode::deserialize::<PeerControl>(&reply).context("bad handshake message")? {
                    PeerControl::Hello { net_version } if net_version == NET_VERSION => {}
                    PeerControl::Hello { net_version } => {
                        anyhow::bail!(
                            "peer runs incompatible net protocol {net_version} (we run {NET_VERSION})"
                        )
                    }
                    other => anyhow::bail!("expected hello, got {other:?}"),
                }
                anyhow::Ok((player, ctl_tx, ctl_rx, data_tx, data_rx))
            }
            .boxed_local(),
        );

        pendings.insert(
            player,
            Pending {
                pc: parts.pc,
                remote_description_set: false,
                queued_candidates: Vec::new(),
            },
        );
    }

    let mut done: HashMap<usize, PeerLink> = HashMap::new();
    // Peers whose signaling socket closed while their edge was still
    // pending, and when (ms) the grace clock started.
    let mut departed: HashMap<usize, f64> = HashMap::new();
    while done.len() < num_players - 1 {
        futures::select! {
            msg = socket.next().fuse() => {
                let bytes = msg.context("signaling server connection lost")?;
                let msg = match gbaroll_signaling::decode::<ServerMessage>(&bytes) {
                    Ok(ServerMessage { msg: Some(m) }) => m,
                    _ => continue,
                };
                match msg {
                    server_message::Msg::Signal(signal) => {
                        let from = signal.peer as usize;
                        let Some(pending) = pendings.get_mut(&from) else { continue };
                        let signal: PeerSignal = match bincode::deserialize(&signal.payload) {
                            Ok(s) => s,
                            Err(e) => {
                                log::warn!("undecodable signal from player {from}: {e}");
                                continue;
                            }
                        };
                        match signal {
                            PeerSignal::Description { sdp_type, sdp } => {
                                match sdp_type.as_str() {
                                    // Answering side: the offer just landed;
                                    // produce and return our answer.
                                    "offer" => {
                                        let answer = pending.pc.accept_offer(&sdp).await?;
                                        pending.remote_description_set = true;
                                        send_signal(
                                            socket,
                                            from,
                                            &PeerSignal::Description {
                                                sdp_type: "answer".to_string(),
                                                sdp: answer,
                                            },
                                        )?;
                                    }
                                    "answer" => {
                                        pending.pc.accept_answer(&sdp).await?;
                                        pending.remote_description_set = true;
                                    }
                                    other => anyhow::bail!("unknown sdp type {other:?}"),
                                }
                                for candidate in pending.queued_candidates.drain(..) {
                                    if let Err(e) = pending.pc.add_remote_candidate(&candidate).await {
                                        log::warn!("add queued candidate: {e:#}");
                                    }
                                }
                            }
                            PeerSignal::Candidate { candidate } => {
                                if pending.remote_description_set {
                                    if let Err(e) = pending.pc.add_remote_candidate(&candidate).await {
                                        log::warn!("add candidate: {e:#}");
                                    }
                                } else {
                                    pending.queued_candidates.push(candidate);
                                }
                            }
                        }
                    }
                    server_message::Msg::PeerLeft(left) => {
                        let player = left.player_idx as usize;
                        // Harmless if that edge is already up (the peer
                        // finished its mesh and left the server); only
                        // an edge that stays pending past the grace
                        // window means the peer really died.
                        if !done.contains_key(&player) {
                            departed.entry(player).or_insert_with(js_sys::Date::now);
                        }
                    }
                    _ => {}
                }
            }
            event = events.next() => {
                let Some((player, event)) = event else { continue };
                match event {
                    PeerEvent::Candidate(candidate) => {
                        send_signal(socket, player, &PeerSignal::Candidate { candidate })?
                    }
                    PeerEvent::Failed => anyhow::bail!("connection to player {} failed", player + 1),
                }
            }
            joined = handshakes.next() => {
                let Some(joined) = joined else { continue };
                let (player, ctl_tx, ctl_rx, data_tx, data_rx) = joined?;
                let pending = pendings.remove(&player).context("handshake for unknown peer")?;
                departed.remove(&player);
                done.insert(player, PeerLink {
                    player,
                    pc: pending.pc,
                    ctl_tx,
                    ctl_rx,
                    data_tx,
                    data_rx,
                });
            }
            _ = TimeoutFuture::new(500).fuse() => {
                let now = js_sys::Date::now();
                if let Some((&player, _)) = departed.iter().find(|(_, at)| now - **at > DEPARTED_GRACE_MS) {
                    anyhow::bail!("player {} left during connection setup", player + 1);
                }
            }
        }
    }

    let mut links: Vec<PeerLink> = done.into_values().collect();
    links.sort_by_key(|l| l.player);
    Ok(links)
}

fn send_signal(socket: &SignalSocket, to: usize, signal: &PeerSignal) -> anyhow::Result<()> {
    let msg = ClientMessage::signal(to as u32, bincode::serialize(signal)?);
    socket
        .send(&gbaroll_signaling::encode(&msg))
        .context("send signal")
}

/// How long the boot-payload exchange has after the mesh is up. Nothing
/// here waits on a human — every client captures and sends as soon as its
/// UI notices the start — so this only catches dead peers.
const EXCHANGE_TIMEOUT_MS: u32 = 60_000;

/// The plug-in exchange: ship this side's encoded boot payload to every
/// peer and collect theirs, over the reliable control channels (chunked —
/// a payload is several hundred KiB and the datachannel message cap is
/// 256 KiB). Returns the payloads in player order, the local slot filled
/// with `blob` itself.
pub async fn exchange_boots(
    links: &mut [PeerLink],
    local_player: usize,
    num_players: usize,
    blob: Vec<u8>,
) -> anyhow::Result<Vec<Vec<u8>>> {
    let swaps = links.iter_mut().map(|link| {
        let PeerLink {
            player,
            ctl_tx,
            ctl_rx,
            ..
        } = link;
        let blob = &blob;
        async move {
            let send = async {
                let announce = bincode::serialize(&PeerControl::Boot {
                    len: blob.len() as u32,
                })?;
                ctl_tx.send(&announce).context("announce boot payload")?;
                for chunk in blob.chunks(BOOT_CHUNK) {
                    let msg = bincode::serialize(&PeerControl::BootChunk(chunk.to_vec()))?;
                    ctl_tx.send(&msg).context("send boot chunk")?;
                }
                anyhow::Ok(())
            };
            let recv = async {
                let msg = ctl_rx.receive().await.context("control channel closed before boot payload")?;
                let len = match bincode::deserialize::<PeerControl>(&msg).context("bad boot announcement")? {
                    PeerControl::Boot { len } => len as usize,
                    PeerControl::Quit => anyhow::bail!("player {} left before the session", *player + 1),
                    other => anyhow::bail!("expected a boot payload, got {other:?}"),
                };
                anyhow::ensure!(len <= MAX_BOOT_SIZE, "boot payload implausibly large ({len} bytes)");
                let mut buf = Vec::with_capacity(len);
                while buf.len() < len {
                    let msg = ctl_rx.receive().await.context("control channel closed mid boot payload")?;
                    match bincode::deserialize::<PeerControl>(&msg).context("bad boot chunk")? {
                        PeerControl::BootChunk(bytes) => buf.extend_from_slice(&bytes),
                        other => anyhow::bail!("expected a boot chunk, got {other:?}"),
                    }
                }
                anyhow::ensure!(buf.len() == len, "boot payload overran its announced length");
                anyhow::Ok((*player, buf))
            };
            let ((), received) = futures::future::try_join(send, recv).await?;
            anyhow::Ok(received)
        }
    });

    let received = futures::select! {
        received = futures::future::try_join_all(swaps).fuse() => received?,
        _ = TimeoutFuture::new(EXCHANGE_TIMEOUT_MS).fuse() => {
            anyhow::bail!("timed out exchanging boot payloads")
        }
    };

    let mut boots = vec![Vec::new(); num_players];
    boots[local_player] = blob;
    for (player, bytes) in received {
        boots[player] = bytes;
    }
    Ok(boots)
}
