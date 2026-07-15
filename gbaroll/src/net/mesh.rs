//! Full-mesh WebRTC bring-up. After the signaling server broadcasts
//! `Starting`, every peer builds one `PeerConnection` per other player:
//! the lower player index makes the offer, descriptions and trickled ICE
//! candidates relay through the server as opaque [`PeerSignal`]s, and
//! each edge finishes with a version handshake over its reliable control
//! channel (whose first send doubles as the wait-for-open barrier).

use std::collections::HashMap;

use anyhow::Context as _;
use datachannel_wrapper::{
    DataChannelInit, DataChannelReceiver, DataChannelSender, IceCandidate, PeerConnection, PeerConnectionEvent,
    Reliability, RtcConfig, SdpType, SessionDescription,
};
use futures::{SinkExt, StreamExt};
use gbaroll_signaling::{ClientMessage, IceServer, ServerMessage};
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::net::protocol::{PeerControl, PeerSignal, BOOT_CHUNK, MAX_BOOT_SIZE, NET_VERSION};

/// One connected mesh edge, ready for a session.
pub struct PeerLink {
    pub player: usize,
    /// Keeps the connection alive; dropping it tears the edge down.
    pub pc: PeerConnection,
    pub ctl_tx: DataChannelSender,
    pub ctl_rx: DataChannelReceiver,
    pub data_tx: DataChannelSender,
    pub data_rx: DataChannelReceiver,
}

/// How long the whole mesh has to come up before we give up.
const MESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a still-pending edge survives its peer's signaling socket
/// going away. A peer that finished *its* mesh legitimately leaves the
/// server while our handshake reply is still in flight — the transport
/// is already up, so the handshake completes without signaling; only a
/// peer that actually died leaves the edge dangling past this.
const DEPARTED_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

struct Pending {
    pc: PeerConnection,
    remote_description_set: bool,
    queued_candidates: Vec<String>,
}

enum PumpEvent {
    Signal(PeerSignal),
    Failed,
}

fn sdp_type_name(t: SdpType) -> &'static str {
    match t {
        SdpType::Offer => "offer",
        SdpType::Answer => "answer",
        SdpType::Pranswer => "pranswer",
        SdpType::Rollback => "rollback",
    }
}

fn sdp_type_from_name(s: &str) -> anyhow::Result<SdpType> {
    Ok(match s {
        "offer" => SdpType::Offer,
        "answer" => SdpType::Answer,
        "pranswer" => SdpType::Pranswer,
        "rollback" => SdpType::Rollback,
        other => anyhow::bail!("unknown sdp type {other:?}"),
    })
}

/// Build the mesh. `sink`/`stream` are the signaling websocket halves,
/// still connected to the started room.
/// Reformat the server-provided ICE list to libdatachannel's URL form
/// (`proto:user:pass@host:port`). TURN-over-TCP entries are dropped —
/// libdatachannel's parser rejects the `?transport=tcp` suffix.
fn to_libdatachannel_urls(servers: &[IceServer]) -> Vec<String> {
    let mut out = Vec::new();
    for server in servers {
        for url in &server.urls {
            let Some((scheme, rest)) = url.split_once(':') else { continue };
            match scheme {
                "stun" | "stuns" => out.push(url.clone()),
                "turn" | "turns" => {
                    if rest.contains("transport=tcp") {
                        continue;
                    }
                    match (&server.username, &server.credential) {
                        (Some(user), Some(pass)) => out.push(format!("{scheme}:{user}:{pass}@{rest}")),
                        _ => out.push(url.clone()),
                    }
                }
                _ => log::warn!("ignoring unknown ICE server url {url:?}"),
            }
        }
    }
    out
}

pub async fn build<Sink, Stream>(
    sink: &mut Sink,
    stream: &mut Stream,
    local_player: usize,
    num_players: usize,
    ice_servers: &[IceServer],
) -> anyhow::Result<Vec<PeerLink>>
where
    Sink: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    Stream: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    tokio::time::timeout(MESH_TIMEOUT, build_inner(sink, stream, local_player, num_players, ice_servers))
        .await
        .map_err(|_| anyhow::anyhow!("timed out connecting to peers"))?
}

async fn build_inner<Sink, Stream>(
    sink: &mut Sink,
    stream: &mut Stream,
    local_player: usize,
    num_players: usize,
    ice_servers: &[IceServer],
) -> anyhow::Result<Vec<PeerLink>>
where
    Sink: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    Stream: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let mut pendings: HashMap<usize, Pending> = HashMap::new();
    let (pump_tx, mut pump_rx) = tokio::sync::mpsc::unbounded_channel::<(usize, PumpEvent)>();
    let mut handshakes = tokio::task::JoinSet::new();

    let ice_urls = to_libdatachannel_urls(ice_servers);
    log::info!("mesh ICE servers: {ice_urls:?}");
    for player in (0..num_players).filter(|&p| p != local_player) {
        let mut config = RtcConfig::new(&ice_urls);
        config.disable_auto_negotiation = true;
        let (mut pc, mut event_rx) = PeerConnection::new(config).context("create peer connection")?;

        // Both channels are negotiated on fixed stream ids, so both
        // sides just create them — no in-band open handshake.
        let ctl = pc
            .create_data_channel("gbaroll-ctl", DataChannelInit::default().negotiated().manual_stream().stream(0))
            .context("create control channel")?;
        let data = pc
            .create_data_channel(
                "gbaroll-data",
                DataChannelInit::default()
                    .reliability(Reliability {
                        unordered: true,
                        unreliable: true,
                        max_packet_life_time: 0,
                        max_retransmits: 0,
                    })
                    .negotiated()
                    .manual_stream()
                    .stream(1),
            )
            .context("create data channel")?;

        // Deterministic roles: the lower index offers.
        if local_player < player {
            pc.set_local_description(SdpType::Offer, None).context("set offer")?;
            let desc = pc.local_description().context("local description missing after offer")?;
            send_signal(
                sink,
                player,
                &PeerSignal::Description {
                    sdp_type: sdp_type_name(desc.sdp_type).to_string(),
                    sdp: desc.sdp,
                },
            )
            .await?;
        }

        // Forward trickled candidates + connection failures out of the
        // event stream into the select loop below.
        let pump_tx = pump_tx.clone();
        crate::runtime().spawn(async move {
            while let Some(event) = event_rx.recv().await {
                match event {
                    PeerConnectionEvent::IceCandidate(candidate) => {
                        if pump_tx
                            .send((player, PumpEvent::Signal(PeerSignal::Candidate { candidate: candidate.candidate })))
                            .is_err()
                        {
                            break;
                        }
                    }
                    PeerConnectionEvent::ConnectionStateChange(
                        datachannel_wrapper::ConnectionState::Failed | datachannel_wrapper::ConnectionState::Closed,
                    ) => {
                        let _ = pump_tx.send((player, PumpEvent::Failed));
                        break;
                    }
                    _ => {}
                }
            }
        });

        // The control channel's first send blocks until the channel
        // opens, so Hello doubles as the wait-for-open barrier.
        let (mut ctl_tx, mut ctl_rx) = ctl.split();
        let (data_tx, data_rx) = data.split();
        handshakes.spawn(async move {
            let hello = bincode::serialize(&PeerControl::Hello {
                net_version: NET_VERSION,
            })?;
            ctl_tx.send(&hello).await.context("send hello")?;
            let reply = ctl_rx.receive().await.context("control channel closed during handshake")?;
            match bincode::deserialize::<PeerControl>(&reply).context("bad handshake message")? {
                PeerControl::Hello { net_version } if net_version == NET_VERSION => {}
                PeerControl::Hello { net_version } => {
                    anyhow::bail!("peer runs incompatible net protocol {net_version} (we run {NET_VERSION})")
                }
                other => anyhow::bail!("expected hello, got {other:?}"),
            }
            anyhow::Ok((player, ctl_tx, ctl_rx, data_tx, data_rx))
        });

        pendings.insert(
            player,
            Pending {
                pc,
                remote_description_set: false,
                queued_candidates: Vec::new(),
            },
        );
    }

    let mut done: HashMap<usize, PeerLink> = HashMap::new();
    // Peers whose signaling socket closed while their edge was still
    // pending, and when the grace clock started.
    let mut departed: HashMap<usize, std::time::Instant> = HashMap::new();
    while done.len() < num_players - 1 {
        tokio::select! {
            msg = stream.next() => {
                let msg = msg.context("signaling server connection lost")??;
                let Message::Binary(bytes) = msg else { continue };
                let msg: ServerMessage = match gbaroll_signaling::decode(&bytes) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                match msg {
                    ServerMessage::Signal { from, payload } => {
                        let from = from as usize;
                        let Some(pending) = pendings.get_mut(&from) else { continue };
                        let signal: PeerSignal = match bincode::deserialize(&payload) {
                            Ok(s) => s,
                            Err(e) => {
                                log::warn!("undecodable signal from player {from}: {e}");
                                continue;
                            }
                        };
                        match signal {
                            PeerSignal::Description { sdp_type, sdp } => {
                                let sdp_type = sdp_type_from_name(&sdp_type)?;
                                let is_offer = matches!(sdp_type, SdpType::Offer);
                                pending
                                    .pc
                                    .set_remote_description(SessionDescription { sdp_type, sdp })
                                    .context("set remote description")?;
                                pending.remote_description_set = true;
                                for candidate in pending.queued_candidates.drain(..) {
                                    if let Err(e) = pending.pc.add_remote_candidate(IceCandidate { candidate }) {
                                        log::warn!("add queued candidate: {e}");
                                    }
                                }
                                // Answering side: the offer just landed;
                                // produce and return our answer.
                                if is_offer {
                                    pending.pc.set_local_description(SdpType::Answer, None).context("set answer")?;
                                    let desc = pending
                                        .pc
                                        .local_description()
                                        .context("local description missing after answer")?;
                                    send_signal(
                                        sink,
                                        from,
                                        &PeerSignal::Description {
                                            sdp_type: sdp_type_name(desc.sdp_type).to_string(),
                                            sdp: desc.sdp,
                                        },
                                    )
                                    .await?;
                                }
                            }
                            PeerSignal::Candidate { candidate } => {
                                if pending.remote_description_set {
                                    if let Err(e) = pending.pc.add_remote_candidate(IceCandidate { candidate }) {
                                        log::warn!("add candidate: {e}");
                                    }
                                } else {
                                    pending.queued_candidates.push(candidate);
                                }
                            }
                        }
                    }
                    ServerMessage::PeerLeft { player_idx } => {
                        let player = player_idx as usize;
                        // Harmless if that edge is already up (the peer
                        // finished its mesh and left the server); only
                        // an edge that stays pending past the grace
                        // window means the peer really died.
                        if !done.contains_key(&player) {
                            departed.entry(player).or_insert_with(std::time::Instant::now);
                        }
                    }
                    _ => {}
                }
            }
            event = pump_rx.recv() => {
                let Some((player, event)) = event else { continue };
                match event {
                    PumpEvent::Signal(signal) => send_signal(sink, player, &signal).await?,
                    PumpEvent::Failed => anyhow::bail!("connection to player {} failed", player + 1),
                }
            }
            joined = handshakes.join_next() => {
                let Some(joined) = joined else { continue };
                let (player, ctl_tx, ctl_rx, data_tx, data_rx) = joined.context("handshake task died")??;
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
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                if let Some((&player, _)) = departed.iter().find(|(_, at)| at.elapsed() > DEPARTED_GRACE) {
                    anyhow::bail!("player {} left during connection setup", player + 1);
                }
            }
        }
    }

    let mut links: Vec<PeerLink> = done.into_values().collect();
    links.sort_by_key(|l| l.player);
    Ok(links)
}

async fn send_signal<Sink>(sink: &mut Sink, to: usize, signal: &PeerSignal) -> anyhow::Result<()>
where
    Sink: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let msg = ClientMessage::Signal {
        to: to as u8,
        payload: bincode::serialize(signal)?,
    };
    sink.send(Message::Binary(gbaroll_signaling::encode(&msg)?))
        .await
        .context("send signal")?;
    Ok(())
}

/// How long the boot-payload exchange has after the mesh is up. Nothing
/// here waits on a human — every client captures and sends as soon as its
/// UI notices the start — so this only catches dead peers.
const EXCHANGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

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
                ctl_tx.send(&announce).await.context("announce boot payload")?;
                for chunk in blob.chunks(BOOT_CHUNK) {
                    let msg = bincode::serialize(&PeerControl::BootChunk(chunk.to_vec()))?;
                    ctl_tx.send(&msg).await.context("send boot chunk")?;
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

    let received = tokio::time::timeout(EXCHANGE_TIMEOUT, futures::future::try_join_all(swaps))
        .await
        .map_err(|_| anyhow::anyhow!("timed out exchanging boot payloads"))??;

    let mut boots = vec![Vec::new(); num_players];
    boots[local_player] = blob;
    for (player, bytes) in received {
        boots[player] = bytes;
    }
    Ok(boots)
}
