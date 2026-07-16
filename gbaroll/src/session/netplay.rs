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

    // futures' unbounded channel rather than std's: the senders live in
    // async pumps and the receiver drains non-blockingly from the tick
    // body, and it works the same whether the pumps run on tokio (here)
    // or as browser tasks (the wasm build).
    let (event_tx, event_rx) = futures::channel::mpsc::unbounded::<NetEvent>();

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
        rom_crc32: Some(bundle.players[local_player].rom_crc32),
    };

    // The link boots on the drive thread (tango does the same); it hands
    // back a LinkHandle for the audio stream once up.
    let (handle_tx, handle_rx) = std::sync::mpsc::channel();
    let drive = {
        let boot_args = BootArgs {
            shared: shared.clone(),
            local_player,
            roms: args.roms,
            boots: bundle.boots,
            nicks: bundle.players.iter().map(|p| p.nick.clone()).collect(),
            rom_meta: args.rom_meta,
            clock_unix_micros: bundle.clock_unix_micros,
            room_code: bundle.room_code.clone(),
            replays_dir: args.replays_dir,
            event_rx,
            peers,
            ctl_txs,
            connections,
        };
        std::thread::Builder::new()
            .name("gbaroll-netplay-drive".to_owned())
            .spawn(move || drive(boot_args, handle_tx))?
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
    event_tx: futures::channel::mpsc::UnboundedSender<NetEvent>,
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
                    let _ = event_tx.unbounded_send(NetEvent::Gone {
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
                .unbounded_send(NetEvent::Input {
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
    let _ = event_tx.unbounded_send(NetEvent::Gone {
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
    event_tx: futures::channel::mpsc::UnboundedSender<NetEvent>,
) {
    while let Some(bytes) = ctl_rx.receive().await {
        match bincode::deserialize::<PeerControl>(&bytes) {
            Ok(PeerControl::Quit) => {
                let _ = event_tx.unbounded_send(NetEvent::Gone {
                    player,
                    reason: GoneReason::Quit,
                });
                return;
            }
            Ok(_) => {}
            Err(e) => log::debug!("bad control message from player {player}: {e}"),
        }
    }
    let _ = event_tx.unbounded_send(NetEvent::Gone {
        player,
        reason: GoneReason::Disconnected,
    });
}

/// Everything the netplay session needs to boot: the exchanged
/// captures, the transport ends, and the channel the pumps feed.
struct BootArgs {
    shared: Arc<SharedSession>,
    local_player: usize,
    roms: Vec<Vec<u8>>,
    boots: Vec<Vec<u8>>,
    nicks: Vec<String>,
    rom_meta: Vec<(u32, String, String)>,
    clock_unix_micros: u64,
    room_code: String,
    replays_dir: std::path::PathBuf,
    event_rx: futures::channel::mpsc::UnboundedReceiver<NetEvent>,
    peers: Vec<PeerCtx>,
    ctl_txs: Vec<datachannel_wrapper::DataChannelSender>,
    connections: Vec<datachannel_wrapper::PeerConnection>,
}

/// The netplay session's boot, per-tick body, and teardown, extracted
/// from the drive loop so a host without threads (the wasm main loop)
/// can call them directly. The caller owns pacing; `tick` assumes it is
/// only called when the session should attempt to advance one frame.
struct NetplayDriver {
    shared: Arc<SharedSession>,
    local_player: usize,
    local_rom: Vec<u8>,
    clock_unix_micros: u64,
    session: Session,
    throttler: Throttler,
    event_rx: futures::channel::mpsc::UnboundedReceiver<NetEvent>,
    peers: Vec<PeerCtx>,
    ctl_txs: Vec<datachannel_wrapper::DataChannelSender>,
    connections: Vec<datachannel_wrapper::PeerConnection>,
    /// Per peer, when we last heard an input from them — consulted only
    /// when the queue watchdog trips, to name the peer that went silent.
    last_heard: Vec<Instant>,
    /// A rolling one-second window of advance times, for the measured
    /// TPS the telemetry panel charts against fps_target.
    tick_times: VecDeque<Instant>,
    replay_writer: Option<gbaroll_replay::Writer<std::io::BufWriter<std::fs::File>>>,
}

impl NetplayDriver {
    /// Rebuild the link from the exchanged captures and start the
    /// rollback session. On failure the end is already published to
    /// `shared`; the caller just returns.
    fn boot(args: BootArgs) -> Option<NetplayDriver> {
        let rtc = std::time::UNIX_EPOCH + std::time::Duration::from_micros(args.clock_unix_micros);
        let local_rom = args.roms[args.local_player].clone();
        // The cable plugs in: every peer rebuilds the identical link from
        // the exchanged captures (the local side included — our own
        // machine loads from its serialized capture too, so everyone
        // reconstructs the same bytes).
        let link = (|| {
            let sides = args
                .roms
                .into_iter()
                .zip(args.boots.iter())
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
                args.shared.finish(SessionEnd::Error(format!("failed to boot link: {e}")));
                return None;
            }
        };
        prepare_audio_buffers(&mut link);

        let session = match Session::new(
            link,
            args.local_player,
            args.shared.present_delay.load(Ordering::Relaxed),
        ) {
            Ok(s) => s,
            Err(e) => {
                args.shared
                    .finish(SessionEnd::Error(format!("failed to start session: {e}")));
                return None;
            }
        };

        // Open the replay. Recording failure downgrades to "no replay",
        // not a dead session.
        let replay_writer = open_replay(
            &args.replays_dir,
            &args.room_code,
            args.local_player,
            args.clock_unix_micros,
            &args.nicks,
            &args.rom_meta,
            &args.boots,
        )
        .map_err(|e| log::error!("replay recording disabled: {e:#}"))
        .ok();

        let last_heard = args.peers.iter().map(|_| Instant::now()).collect();
        Some(NetplayDriver {
            shared: args.shared,
            local_player: args.local_player,
            local_rom,
            clock_unix_micros: args.clock_unix_micros,
            session,
            throttler: Throttler::new(),
            event_rx: args.event_rx,
            peers: args.peers,
            ctl_txs: args.ctl_txs,
            connections: args.connections,
            last_heard,
            tick_times: VecDeque::new(),
            replay_writer,
        })
    }

    fn link_handle(&mut self) -> mgba_siolink::session::LinkHandle {
        self.session.link_handle()
    }

    /// Attempt to advance one frame. Returns `Some(end)` when the
    /// session is over; the caller must then run [`finish`].
    fn tick(&mut self) -> Option<SessionEnd> {
        if self.shared.quit.load(Ordering::Relaxed) {
            return Some(SessionEnd::LocalQuit);
        }
        if self.shared.unplug.load(Ordering::Relaxed) {
            return Some(SessionEnd::Unplugged);
        }

        let pd = self.shared.present_delay.load(Ordering::Relaxed);
        if pd != self.session.present_delay() {
            self.session.set_present_delay(pd);
        }

        // Drain the network before advancing. A closed channel reads as
        // empty: the pumps announce their own death as a Gone event
        // before dropping their sender.
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                NetEvent::Input {
                    player,
                    keys,
                    tick_advantage,
                } => {
                    if let Some(slot) = self.peers.iter().position(|p| p.player == player) {
                        self.last_heard[slot] = Instant::now();
                    }
                    self.session.add_remote_input(player, keys as u32, tick_advantage)
                }
                NetEvent::Gone { player, reason } => {
                    return Some(match reason {
                        GoneReason::Quit => SessionEnd::PeerQuit { player },
                        GoneReason::Disconnected | GoneReason::FellBehind => {
                            SessionEnd::PeerDisconnected { player }
                        }
                    });
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
        let queue_len = self.session.local_queue_length();
        if queue_len >= UNPLUG_QUEUE_LENGTH {
            let slot = (0..self.peers.len())
                .max_by_key(|&i| self.last_heard[i].elapsed())
                .unwrap_or(0);
            return Some(SessionEnd::PeerDisconnected {
                player: self.peers[slot].player,
            });
        }

        // Read skew BEFORE advance enqueues this tick's local input.
        let skew = self.session.skew();

        let keys = self.shared.joyflags.load(Ordering::Relaxed) & 0x3ff;
        let (outgoing, report) = match self.session.advance(keys) {
            Ok(v) => v,
            Err(e) => return Some(SessionEnd::Error(format!("emulation error: {e}"))),
        };

        // Broadcast this tick to every peer: push onto each per-peer
        // out-stream and ship its whole redundancy window.
        let checkpoint = self.session.checkpoint().unwrap_or((0, 0));
        let meta = Meta {
            tick_advantage: outgoing.tick_advantage,
            checkpoint_tick: checkpoint.0,
            checkpoint_digest: checkpoint.1,
        };
        for peer in &self.peers {
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
        for peer in &self.peers {
            if let Some((tick, digest)) = peer.checkpoint.lock().unwrap().take() {
                if let Some(mine) = self.session.digest_at(tick) {
                    if mine != digest {
                        log::error!(
                            "desync at settled tick {tick}: local digest {mine:08x}, player {} digest {digest:08x}",
                            peer.player + 1
                        );
                        return Some(SessionEnd::Desync { tick });
                    }
                }
            }
        }

        // Record newly-confirmed ticks.
        if let Some(writer) = self.replay_writer.as_mut() {
            for (_tick, row) in self.session.drain_confirmed() {
                if let Err(e) = writer.push(&row) {
                    log::error!("replay write failed, stopping recording: {e}");
                    self.replay_writer = None;
                    break;
                }
            }
        }

        // Present the local side.
        let (shared, local_player) = (&self.shared, self.local_player);
        self.session.with_link(|link| {
            if let Some(buf) = link.video_buffer(local_player) {
                shared.publish_video(buf);
            }
        });

        // Clock sync: shave fps by the throttler's slowdown.
        let slowdown = self.throttler.step(skew, self.session.speculation_balance());
        let fps_target = EXPECTED_FPS - slowdown;
        self.shared.set_fps_target(fps_target);

        // Measured TPS: advances in the trailing second.
        let now = Instant::now();
        self.tick_times.push_back(now);
        while self
            .tick_times
            .front()
            .is_some_and(|t| now.duration_since(*t) > std::time::Duration::from_secs(1))
        {
            self.tick_times.pop_front();
        }

        {
            let mut stats = self.shared.stats.lock().unwrap();
            stats.queue_len = queue_len as u32;
            stats.skew = skew;
            stats.rolled_back = report.rolled_back;
            stats.confirmed = report.confirmed;
            stats.frontier = report.frontier;
            stats.tps = self.tick_times.len() as f32;
            stats.fps_target = fps_target;
            stats.peers = self
                .peers
                .iter()
                .map(|p| crate::session::PeerStat {
                    player: p.player,
                    nick: p.nick.clone(),
                    rtt_ms: p.rtt.lock().unwrap().map(|d| d.as_secs_f32() * 1000.0),
                })
                .collect();
        }

        None
    }

    /// Teardown: announce a deliberate end to the peers, finalize the
    /// replay, capture the unplug handoff, and publish the end.
    fn finish(mut self, end: SessionEnd) {
        // A deliberate local quit or unplug announces itself so peers end
        // at once instead of waiting out a transport EOF.
        if matches!(end, SessionEnd::LocalQuit | SessionEnd::Unplugged) {
            if let Ok(quit) = bincode::serialize(&PeerControl::Quit) {
                for ctl in self.ctl_txs.iter_mut() {
                    let _ = crate::runtime().block_on(async {
                        tokio::time::timeout(std::time::Duration::from_millis(500), ctl.send(&quit)).await
                    });
                }
            }
        }

        // Finalize the replay: flush any confirmed ticks the loop's exit
        // skipped (an unplug/disconnect breaks at the top of the
        // iteration, before that pass's drain), then write the
        // end-of-replay sentinel so it reads back complete rather than
        // truncated.
        if let Some(mut writer) = self.replay_writer.take() {
            for (_tick, row) in self.session.drain_confirmed() {
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
        // newest simulated tick — what the player was just looking at) so
        // the game continues solo. The dead peers' unconfirmed inputs
        // stay whatever we predicted, which is exactly the static a real
        // yank leaves on the wire.
        if end.unplugs() {
            let local_player = self.local_player;
            let captured = self.session.with_link(|link| {
                let state = link.capture_boot_state(local_player)?;
                Ok::<_, mgba::Error>((state, link.export_save(local_player)))
            });
            match captured {
                Ok((state, save)) => {
                    *self.shared.handoff.lock().unwrap() = Some(Handoff {
                        rom: self.local_rom,
                        state,
                        save,
                        rtc_unix_micros: self.clock_unix_micros,
                    });
                }
                Err(e) => log::error!("couldn't capture the unplug handoff: {e}"),
            }
        }

        self.shared.finish(end);
        drop(self.connections);
    }
}

fn drive(args: BootArgs, handle_tx: std::sync::mpsc::Sender<mgba_siolink::session::LinkHandle>) {
    let shared = args.shared.clone();
    let Some(mut driver) = NetplayDriver::boot(args) else {
        return;
    };
    let _ = handle_tx.send(driver.link_handle());

    let mut pacer = Pacer::new();
    let end = loop {
        if let Some(end) = driver.tick() {
            break end;
        }
        pacer.pace(f32::from_bits(shared.fps_target.load(Ordering::Relaxed)));
    };
    driver.finish(end);
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
