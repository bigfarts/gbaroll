//! The netplay session: a rollback `mgba_siolink::session::Session`
//! paced on a dedicated drive thread, with per-peer rennet streams over
//! the mesh's unreliable datachannels. The drive loop mirrors tango's:
//! drain the network, queue watchdog, read skew *before* advance,
//! advance, broadcast the redundancy window, feed the throttler into
//! the pacer.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use mgba_siolink::session::Session;
use mgba_siolink::throttler::Throttler;
use mgba_siolink::{BootSide, Link};

use crate::net::lobby::SessionBundle;
use crate::net::protocol::{BootBlob, Frame, InStream, Input, Meta, OutStream, PeerControl, HORIZON};
use crate::session::{
    prepare_audio_buffers, Handoff, LinkAccess, Pacer, SessionDescriptor, SessionEnd, SessionKind, SessionRuntime,
    SharedSession, EXPECTED_FPS, UNPLUG_QUEUE_LENGTH,
};

pub struct NetplayArgs {
    pub bundle: SessionBundle,
    /// Per player, the ROM bytes their side boots (resolved from the
    /// local library by CRC32).
    pub roms: Vec<Vec<u8>>,
    /// Per player, (crc32, header title, header code) for the replay.
    pub rom_meta: Vec<(u32, String, String)>,
    pub replays_dir: std::path::PathBuf,
    pub present_delay: u32,
}

/// Cadence of the per-peer heartbeat: resend the redundancy window on
/// any interval where the emulator sent nothing, so acks/recovery keep
/// flowing while a peer catches up.
const HEARTBEAT: std::time::Duration = std::time::Duration::from_millis(16);

/// How many (seq, send time) samples to keep for ack-derived RTT.
const MAX_RTT_SAMPLES: usize = 256;

struct Streams {
    out: OutStream,
    inn: InStream,
    sent_times: VecDeque<(u32, Instant)>,
}

/// Per-peer shared state between the drive thread and the pumps.
struct PeerCtx {
    player: usize,
    nick: String,
    streams: Arc<Mutex<Streams>>,
    dgram_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    rtt: Arc<Mutex<Option<std::time::Duration>>>,
    /// Freshest checkpoint the peer reported, taken by the drive thread.
    checkpoint: Arc<Mutex<Option<(u32, u32)>>>,
}

enum GoneReason {
    Quit,
    Disconnected,
    FellBehind,
}

enum NetEvent {
    Input {
        player: usize,
        keys: u16,
        tick_advantage: i16,
    },
    Gone {
        player: usize,
        reason: GoneReason,
    },
}

pub fn start(
    args: NetplayArgs,
    audio_binder: &crate::platform::audio::LateBinder,
    frame_notify: Arc<tokio::sync::Notify>,
) -> anyhow::Result<SessionRuntime> {
    let num_players = args.bundle.players.len();
    let local_player = args.bundle.local_player;
    assert_eq!(args.roms.len(), num_players);

    let shared = SharedSession::new(args.present_delay, frame_notify);
    shared.view_player.store(local_player, Ordering::Relaxed);

    let (event_tx, event_rx) = std::sync::mpsc::channel::<NetEvent>();

    // Split each mesh edge into pump tasks + the drive thread's context.
    let mut peers = Vec::new();
    let mut ctl_txs = Vec::new();
    let mut connections = Vec::new();
    let mut bundle = args.bundle;
    for peer in bundle.peers.drain(..) {
        let player = peer.player;
        let nick = bundle.players[player].nick.clone();
        let streams = Arc::new(Mutex::new(Streams {
            out: OutStream::new(HORIZON),
            inn: InStream::new(HORIZON),
            sent_times: VecDeque::new(),
        }));
        let rtt = Arc::new(Mutex::new(None));
        let checkpoint = Arc::new(Mutex::new(None));
        let (dgram_tx, dgram_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

        crate::runtime().spawn(recv_pump(
            player,
            peer.data_rx,
            streams.clone(),
            rtt.clone(),
            checkpoint.clone(),
            event_tx.clone(),
        ));
        crate::runtime().spawn(send_pump(peer.data_tx, dgram_rx, streams.clone()));
        crate::runtime().spawn(ctl_pump(player, peer.ctl_rx, event_tx.clone()));

        ctl_txs.push(peer.ctl_tx);
        connections.push(peer.pc);
        peers.push(PeerCtx {
            player,
            nick,
            streams,
            dgram_tx,
            rtt,
            checkpoint,
        });
    }

    let descriptor = SessionDescriptor {
        kind: SessionKind::Netplay,
        num_players,
        local_player,
        nicks: bundle.players.iter().map(|p| p.nick.clone()).collect(),
        room_code: Some(bundle.room_code.clone()),
        replay_path: None,
        rom_crc32: Some(bundle.players[local_player].rom_crc32),
    };

    // The link boots on the drive thread (tango does the same); it hands
    // back a LinkHandle for the audio stream once up.
    let (handle_tx, handle_rx) = std::sync::mpsc::channel();
    let drive = {
        let shared = shared.clone();
        let boots = bundle.boots;
        let nicks: Vec<String> = bundle.players.iter().map(|p| p.nick.clone()).collect();
        let room_code = bundle.room_code.clone();
        let clock = bundle.clock_unix_micros;
        let roms = args.roms;
        let rom_meta = args.rom_meta;
        let replays_dir = args.replays_dir;
        std::thread::Builder::new()
            .name("gbaroll-netplay-drive".to_owned())
            .spawn(move || {
                drive(
                    shared,
                    local_player,
                    roms,
                    boots,
                    nicks,
                    rom_meta,
                    clock,
                    room_code,
                    replays_dir,
                    event_rx,
                    peers,
                    ctl_txs,
                    connections,
                    handle_tx,
                )
            })?
    };

    // Wait for boot so the audio stream can bind to the live link.
    let link_handle = handle_rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .map_err(|_| anyhow::anyhow!("emulator failed to boot (see log)"))?;

    let audio = audio_binder
        .bind(Some(Box::new(crate::platform::audio::LinkStream::new(
            LinkAccess::Handle(link_handle),
            shared.clone(),
            audio_binder.sample_rate(),
        ))))
        .ok();

    Ok(SessionRuntime {
        shared,
        descriptor,
        link: None,
        playback: None,
        _audio: audio,
        pre_join: None,
        threads: vec![drive],
    })
}

async fn recv_pump(
    player: usize,
    mut data_rx: datachannel_wrapper::DataChannelReceiver,
    streams: Arc<Mutex<Streams>>,
    rtt: Arc<Mutex<Option<std::time::Duration>>>,
    checkpoint: Arc<Mutex<Option<(u32, u32)>>>,
    event_tx: std::sync::mpsc::Sender<NetEvent>,
) {
    while let Some(dgram) = data_rx.receive().await {
        let frame = match Frame::decode(&mut &dgram[..]) {
            Ok(f) => f,
            Err(e) => {
                log::debug!("bad datagram from player {player}: {e}");
                continue;
            }
        };
        let delivered = {
            let mut s = streams.lock().unwrap();
            s.out.apply_ack(frame.ack());
            // Ack-derived RTT: when the peer's frontier passes a
            // timestamped seq, the freshest just-confirmed one dates
            // the round trip.
            let frontier = s.out.peer_ack_base();
            let mut newest = None;
            while s.sent_times.front().is_some_and(|(seq, _)| *seq < frontier) {
                newest = s.sent_times.pop_front();
            }
            if let Some((_, at)) = newest {
                *rtt.lock().unwrap() = Some(at.elapsed());
            }
            match s.inn.accept(&frame) {
                Ok(window) => window,
                Err(rennet::HorizonExceeded) => {
                    let _ = event_tx.send(NetEvent::Gone {
                        player,
                        reason: GoneReason::FellBehind,
                    });
                    return;
                }
            }
        };
        if delivered.meta.checkpoint_tick > 0 {
            *checkpoint.lock().unwrap() = Some((delivered.meta.checkpoint_tick, delivered.meta.checkpoint_digest));
        }
        for element in delivered.entries {
            if event_tx
                .send(NetEvent::Input {
                    player,
                    keys: element.0,
                    tick_advantage: delivered.meta.tick_advantage,
                })
                .is_err()
            {
                return;
            }
        }
    }
    let _ = event_tx.send(NetEvent::Gone {
        player,
        reason: GoneReason::Disconnected,
    });
}

async fn send_pump(
    mut data_tx: datachannel_wrapper::DataChannelSender,
    mut dgram_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    streams: Arc<Mutex<Streams>>,
) {
    loop {
        match tokio::time::timeout(HEARTBEAT, dgram_rx.recv()).await {
            Ok(Some(bytes)) => {
                if data_tx.send(&bytes).await.is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(_) => {
                // Nothing sent this interval (stall/pause): resend the
                // window so acks and loss recovery keep flowing.
                let bytes = {
                    let s = streams.lock().unwrap();
                    let w = s.out.window();
                    Frame::new(w.base, s.inn.ack(), w.meta, w.entries).to_vec()
                };
                if data_tx.send(&bytes).await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn ctl_pump(
    player: usize,
    mut ctl_rx: datachannel_wrapper::DataChannelReceiver,
    event_tx: std::sync::mpsc::Sender<NetEvent>,
) {
    while let Some(bytes) = ctl_rx.receive().await {
        match bincode::deserialize::<PeerControl>(&bytes) {
            Ok(PeerControl::Quit) => {
                let _ = event_tx.send(NetEvent::Gone {
                    player,
                    reason: GoneReason::Quit,
                });
                return;
            }
            Ok(_) => {}
            Err(e) => log::debug!("bad control message from player {player}: {e}"),
        }
    }
    let _ = event_tx.send(NetEvent::Gone {
        player,
        reason: GoneReason::Disconnected,
    });
}

#[allow(clippy::too_many_arguments)]
fn drive(
    shared: Arc<SharedSession>,
    local_player: usize,
    roms: Vec<Vec<u8>>,
    boots: Vec<Vec<u8>>,
    nicks: Vec<String>,
    rom_meta: Vec<(u32, String, String)>,
    clock_unix_micros: u64,
    room_code: String,
    replays_dir: std::path::PathBuf,
    event_rx: std::sync::mpsc::Receiver<NetEvent>,
    peers: Vec<PeerCtx>,
    mut ctl_txs: Vec<datachannel_wrapper::DataChannelSender>,
    connections: Vec<datachannel_wrapper::PeerConnection>,
    handle_tx: std::sync::mpsc::Sender<mgba_siolink::session::LinkHandle>,
) {
    let rtc = std::time::UNIX_EPOCH + std::time::Duration::from_micros(clock_unix_micros);
    let local_rom = roms[local_player].clone();
    // The cable plugs in: every peer rebuilds the identical link from the
    // exchanged captures (the local side included — our own machine loads
    // from its serialized capture too, so everyone reconstructs the same
    // bytes).
    let link = (|| {
        let sides = roms
            .into_iter()
            .zip(boots.iter())
            .map(|(rom, boot)| {
                let blob = BootBlob::decode(boot)?;
                anyhow::Ok(BootSide {
                    rom,
                    save: blob.save,
                    state: blob.state,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        anyhow::Ok(Link::from_states(sides, Some(rtc))?)
    })();
    let mut link = match link {
        Ok(link) => link,
        Err(e) => {
            shared.finish(SessionEnd::Error(format!("failed to boot link: {e}")));
            return;
        }
    };
    prepare_audio_buffers(&mut link);

    let mut session = match Session::new(link, local_player, shared.present_delay.load(Ordering::Relaxed)) {
        Ok(s) => s,
        Err(e) => {
            shared.finish(SessionEnd::Error(format!("failed to start session: {e}")));
            return;
        }
    };
    let _ = handle_tx.send(session.link_handle());

    // Open the replay. Recording failure downgrades to "no replay", not
    // a dead session.
    let mut replay_writer = open_replay(
        &replays_dir,
        &room_code,
        local_player,
        clock_unix_micros,
        &nicks,
        &rom_meta,
        &boots,
    )
    .map_err(|e| log::error!("replay recording disabled: {e:#}"))
    .ok();

    let mut throttler = Throttler::new();
    let mut pacer = Pacer::new();
    // Per peer, when we last heard an input from them — consulted only
    // when the queue watchdog trips, to name the peer that went silent.
    let mut last_heard: Vec<Instant> = peers.iter().map(|_| Instant::now()).collect();
    // A rolling one-second window of advance times, for the measured TPS
    // the telemetry panel charts against fps_target.
    let mut tick_times: VecDeque<Instant> = VecDeque::new();

    let end = 'main: loop {
        if shared.quit.load(Ordering::Relaxed) {
            break 'main SessionEnd::LocalQuit;
        }
        if shared.unplug.load(Ordering::Relaxed) {
            break 'main SessionEnd::Unplugged;
        }

        let pd = shared.present_delay.load(Ordering::Relaxed);
        if pd != session.present_delay() {
            session.set_present_delay(pd);
        }

        // Drain the network before advancing.
        for event in event_rx.try_iter() {
            match event {
                NetEvent::Input {
                    player,
                    keys,
                    tick_advantage,
                } => {
                    if let Some(slot) = peers.iter().position(|p| p.player == player) {
                        last_heard[slot] = Instant::now();
                    }
                    session.add_remote_input(player, keys as u32, tick_advantage)
                }
                NetEvent::Gone { player, reason } => {
                    break 'main match reason {
                        GoneReason::Quit => SessionEnd::PeerQuit { player },
                        GoneReason::Disconnected | GoneReason::FellBehind => {
                            SessionEnd::PeerDisconnected { player }
                        }
                    };
                }
            }
        }

        // The queue watchdog (tango's scheme): a dead link can't stop the
        // throttled sim from committing local inputs, so the unmatched
        // queue climbs steadily toward the trip depth — reaching it IS
        // the disconnect signal, measured in the resource that would
        // overflow the horizon rather than a time proxy for it. The
        // transport's own close events usually beat this; the watchdog
        // catches the hard drops the transport is slow to notice. Blame
        // whoever has been silent longest.
        let queue_len = session.local_queue_length();
        if queue_len >= UNPLUG_QUEUE_LENGTH {
            let slot = (0..peers.len()).max_by_key(|&i| last_heard[i].elapsed()).unwrap_or(0);
            break 'main SessionEnd::PeerDisconnected {
                player: peers[slot].player,
            };
        }

        // Read skew BEFORE advance enqueues this tick's local input.
        let skew = session.skew();

        let keys = shared.joyflags.load(Ordering::Relaxed) & 0x3ff;
        let (outgoing, report) = match session.advance(keys) {
            Ok(v) => v,
            Err(e) => break 'main SessionEnd::Error(format!("emulation error: {e}")),
        };

        // Broadcast this tick to every peer: push onto each per-peer
        // out-stream and ship its whole redundancy window.
        let checkpoint = session.checkpoint().unwrap_or((0, 0));
        let meta = Meta {
            tick_advantage: outgoing.tick_advantage,
            checkpoint_tick: checkpoint.0,
            checkpoint_digest: checkpoint.1,
        };
        for peer in &peers {
            let bytes = {
                let mut s = peer.streams.lock().unwrap();
                let seq = s.out.push_with_meta(Input(outgoing.keys as u16), meta);
                s.sent_times.push_back((seq, Instant::now()));
                if s.sent_times.len() > MAX_RTT_SAMPLES {
                    s.sent_times.pop_front();
                }
                let w = s.out.window();
                Frame::new(w.base, s.inn.ack(), w.meta, w.entries).to_vec()
            };
            let _ = peer.dgram_tx.send(bytes);
        }

        // Cross-peer desync check: compare each peer's newest reported
        // settled digest against our own at that tick.
        for peer in &peers {
            if let Some((tick, digest)) = peer.checkpoint.lock().unwrap().take() {
                if let Some(mine) = session.digest_at(tick) {
                    if mine != digest {
                        log::error!(
                            "desync at settled tick {tick}: local digest {mine:08x}, player {} digest {digest:08x}",
                            peer.player + 1
                        );
                        break 'main SessionEnd::Desync { tick };
                    }
                }
            }
        }

        // Record newly-confirmed ticks.
        if let Some(writer) = replay_writer.as_mut() {
            for (_tick, row) in session.drain_confirmed() {
                if let Err(e) = writer.push(&row) {
                    log::error!("replay write failed, stopping recording: {e}");
                    replay_writer = None;
                    break;
                }
            }
        }

        // Present the local side.
        session.with_link(|link| {
            if let Some(buf) = link.video_buffer(local_player) {
                shared.publish_video(buf);
            }
        });

        // Clock sync: shave fps by the throttler's slowdown.
        let slowdown = throttler.step(skew, session.speculation_balance());
        let fps_target = EXPECTED_FPS - slowdown;
        shared.set_fps_target(fps_target);

        // Measured TPS: advances in the trailing second.
        let now = Instant::now();
        tick_times.push_back(now);
        while tick_times.front().is_some_and(|t| now.duration_since(*t) > std::time::Duration::from_secs(1)) {
            tick_times.pop_front();
        }

        {
            let mut stats = shared.stats.lock().unwrap();
            stats.queue_len = queue_len as u32;
            stats.skew = skew;
            stats.rolled_back = report.rolled_back;
            stats.confirmed = report.confirmed;
            stats.frontier = report.frontier;
            stats.tps = tick_times.len() as f32;
            stats.fps_target = fps_target;
            stats.peers = peers
                .iter()
                .map(|p| crate::session::PeerStat {
                    player: p.player,
                    nick: p.nick.clone(),
                    rtt_ms: p.rtt.lock().unwrap().map(|d| d.as_secs_f32() * 1000.0),
                })
                .collect();
        }

        pacer.pace(fps_target);
    };

    // A deliberate local quit or unplug announces itself so peers end at
    // once instead of waiting out a transport EOF.
    if matches!(end, SessionEnd::LocalQuit | SessionEnd::Unplugged) {
        if let Ok(quit) = bincode::serialize(&PeerControl::Quit) {
            for ctl in ctl_txs.iter_mut() {
                let _ = crate::runtime().block_on(async {
                    tokio::time::timeout(std::time::Duration::from_millis(500), ctl.send(&quit)).await
                });
            }
        }
    }

    // Finalize the replay: flush any confirmed ticks the loop's exit
    // skipped (an unplug/disconnect breaks at the top of the iteration,
    // before that pass's drain), then write the end-of-replay sentinel so
    // it reads back complete rather than truncated.
    if let Some(mut writer) = replay_writer.take() {
        for (_tick, row) in session.drain_confirmed() {
            if let Err(e) = writer.push(&row) {
                log::error!("replay write failed while finalizing: {e}");
                break;
            }
        }
        if let Err(e) = writer.finish() {
            log::error!("failed to finalize replay: {e}");
        }
    }

    // The cable unplugs: capture the local machine as it stands (the
    // newest simulated tick — what the player was just looking at) so the
    // game continues solo. The dead peers' unconfirmed inputs stay
    // whatever we predicted, which is exactly the static a real yank
    // leaves on the wire.
    if end.unplugs() {
        let captured = session.with_link(|link| {
            let state = link.capture_boot_state(local_player)?;
            Ok::<_, mgba::Error>((state, link.export_save(local_player)))
        });
        match captured {
            Ok((state, save)) => {
                *shared.handoff.lock().unwrap() = Some(Handoff {
                    rom: local_rom,
                    state,
                    save,
                    rtc_unix_micros: clock_unix_micros,
                });
            }
            Err(e) => log::error!("couldn't capture the unplug handoff: {e}"),
        }
    }

    shared.finish(end);
    drop(connections);
}

fn open_replay(
    replays_dir: &std::path::Path,
    room_code: &str,
    local_player: usize,
    clock_unix_micros: u64,
    nicks: &[String],
    rom_meta: &[(u32, String, String)],
    boots: &[Vec<u8>],
) -> anyhow::Result<gbaroll_replay::Writer<std::io::BufWriter<std::fs::File>>> {
    std::fs::create_dir_all(replays_dir)?;
    let stamp = chrono::Local::now().format("%Y%m%d%H%M%S");
    let path = replays_dir.join(format!(
        "{stamp}-{}-p{}.{}",
        room_code.to_ascii_lowercase(),
        local_player + 1,
        gbaroll_replay::FILE_EXTENSION
    ));
    let file = std::fs::File::create(&path)?;
    let metadata = gbaroll_replay::Metadata {
        local_player: local_player as u8,
        started_at_unix_micros: Some(clock_unix_micros),
        rtc_unix_micros: Some(clock_unix_micros),
        players: nicks
            .iter()
            .zip(rom_meta.iter())
            .zip(boots.iter())
            .map(|((nick, (crc, title, code)), boot)| gbaroll_replay::PlayerMeta {
                nick: nick.clone(),
                rom_crc32: *crc,
                rom_title: title.clone(),
                rom_code: code.clone(),
                // The exchanged payload verbatim (already compressed):
                // playback rebuilds the same plugged-in link from it.
                boot: Some(boot.clone()),
            })
            .collect(),
    };
    log::info!("recording replay to {}", path.display());
    Ok(gbaroll_replay::Writer::new(std::io::BufWriter::new(file), &metadata)?)
}
