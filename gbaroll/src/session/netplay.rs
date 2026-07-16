//! The netplay session: a rollback `mgba_siolink::session::Session`
//! ticked by the runtime pump, with per-peer rennet streams over the
//! mesh's unreliable datachannels. The tick body mirrors tango's:
//! drain the network, queue watchdog, read skew *before* advance,
//! advance, broadcast the redundancy window, feed the throttler into
//! the clock. Datachannel sends are synchronous on the web, so there is
//! no send pump — the tick sends directly and a per-peer heartbeat task
//! covers resend-on-idle.

use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use futures::channel::mpsc;
use gloo_timers::future::TimeoutFuture;
use mgba_siolink::session::Session;
use mgba_siolink::throttler::Throttler;
use mgba_siolink::{BootSide, Link};
use web_time::Instant;

use crate::net::lobby::SessionBundle;
use crate::net::protocol::{
    BootBlob, Frame, InStream, Input, Meta, OutStream, PeerControl, HORIZON,
};
use crate::net::webrtc::{ChannelReceiver, ChannelSender, PeerConnection};
use crate::session::{
    prepare_audio_buffers, Handoff, LinkAccess, SessionDescriptor, SessionEnd, SessionKind,
    SharedSession, EXPECTED_FPS, UNPLUG_QUEUE_LENGTH,
};

pub struct NetplayArgs {
    pub bundle: SessionBundle,
    /// Per player, the ROM bytes their side boots (resolved from the
    /// local library by CRC32).
    pub roms: Vec<Vec<u8>>,
    pub present_delay: u32,
}

/// Cadence of the per-peer heartbeat: resend the redundancy window on
/// any interval where the emulator sent nothing, so acks/recovery keep
/// flowing while a peer catches up. (Hidden-tab timer clamping slows
/// this to ~1Hz, which still carries the whole window — and the audio
/// pump keeps real sends flowing at full rate anyway.)
const HEARTBEAT_MS: u32 = 16;

/// How many (seq, send time) samples to keep for ack-derived RTT.
const MAX_RTT_SAMPLES: usize = 256;

struct Streams {
    out: OutStream,
    inn: InStream,
    sent_times: VecDeque<(u32, Instant)>,
}

/// Per-peer shared state between the tick body and the pumps.
struct PeerCtx {
    player: usize,
    nick: String,
    streams: Rc<Mutex<Streams>>,
    data_tx: ChannelSender,
    /// Set by the tick on every real send; the heartbeat task clears it
    /// and only resends on an interval that stayed false.
    sent: Rc<Cell<bool>>,
    rtt: Rc<Mutex<Option<web_time::Duration>>>,
    /// Freshest checkpoint the peer reported, taken by the tick.
    checkpoint: Rc<Mutex<Option<(u32, u32)>>>,
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

/// A booted netplay session: the driver the runtime pump ticks, plus
/// the shared state and link access, shaped like `LocalSession`.
pub struct NetplaySession {
    pub driver: NetplayDriver,
    pub shared: Arc<SharedSession>,
    pub link: LinkAccess,
    pub descriptor: SessionDescriptor,
}

/// Boot the link from the exchanged captures (synchronously — one
/// ~100-400ms stall inside the plug-in pump, absorbed by the primed
/// audio sink) and spawn the per-peer transport pumps.
pub fn start(args: NetplayArgs) -> anyhow::Result<NetplaySession> {
    let num_players = args.bundle.players.len();
    let local_player = args.bundle.local_player;
    assert_eq!(args.roms.len(), num_players);

    let shared = SharedSession::new(args.present_delay);
    shared.view_player.store(local_player, Ordering::Relaxed);

    let (event_tx, event_rx) = mpsc::unbounded::<NetEvent>();

    // Split each mesh edge into pump tasks + the tick body's context.
    let mut peers = Vec::new();
    let mut ctl_txs = Vec::new();
    let mut connections = Vec::new();
    let mut bundle = args.bundle;
    // Stops the heartbeat tasks at teardown.
    let stop = Rc::new(Cell::new(false));
    for peer in bundle.peers.drain(..) {
        let player = peer.player;
        let nick = bundle.players[player].nick.clone();
        let streams = Rc::new(Mutex::new(Streams {
            out: OutStream::new(HORIZON),
            inn: InStream::new(HORIZON),
            sent_times: VecDeque::new(),
        }));
        let rtt = Rc::new(Mutex::new(None));
        let checkpoint = Rc::new(Mutex::new(None));
        let sent = Rc::new(Cell::new(false));

        wasm_bindgen_futures::spawn_local(recv_pump(
            player,
            peer.data_rx,
            streams.clone(),
            rtt.clone(),
            checkpoint.clone(),
            event_tx.clone(),
        ));
        wasm_bindgen_futures::spawn_local(ctl_pump(player, peer.ctl_rx, event_tx.clone()));
        wasm_bindgen_futures::spawn_local(heartbeat(
            peer.data_tx.clone(),
            streams.clone(),
            sent.clone(),
            stop.clone(),
        ));

        ctl_txs.push(peer.ctl_tx);
        connections.push(peer.pc);
        peers.push(PeerCtx {
            player,
            nick,
            streams,
            data_tx: peer.data_tx,
            sent,
            rtt,
            checkpoint,
        });
    }

    let descriptor = SessionDescriptor {
        kind: SessionKind::Netplay,
        local_player,
        nicks: bundle.players.iter().map(|p| p.nick.clone()).collect(),
        room_code: Some(bundle.room_code.clone()),
        rom_crc32: Some(bundle.players[local_player].rom_crc32),
    };

    let rtc = std::time::UNIX_EPOCH + std::time::Duration::from_micros(bundle.clock_unix_micros);
    let local_rom = args.roms[local_player].clone();
    // The cable plugs in: every peer rebuilds the identical link from the
    // exchanged captures (the local side included — our own machine loads
    // from its serialized capture too, so everyone reconstructs the same
    // bytes).
    let sides = args
        .roms
        .into_iter()
        .zip(bundle.boots.iter())
        .map(|(rom, boot)| {
            let blob = BootBlob::decode(boot)?;
            anyhow::Ok(BootSide {
                rom,
                save: blob.save,
                state: blob.state,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut link = Link::from_states(sides, Some(rtc))?;
    prepare_audio_buffers(&mut link);

    let session = Session::new(link, local_player, args.present_delay)?;
    let link_handle = session.link_handle();

    let mut driver = NetplayDriver {
        shared: shared.clone(),
        local_player,
        local_rom,
        clock_unix_micros: bundle.clock_unix_micros,
        session,
        throttler: Throttler::new(),
        event_rx,
        peers,
        ctl_txs: Some(ctl_txs),
        connections: Some(connections),
        stop,
        last_heard: Vec::new(),
        tick_times: VecDeque::new(),
    };
    driver.last_heard = driver.peers.iter().map(|_| Instant::now()).collect();

    Ok(NetplaySession {
        driver,
        shared,
        link: LinkAccess::Handle(link_handle),
        descriptor,
    })
}

async fn recv_pump(
    player: usize,
    mut data_rx: ChannelReceiver,
    streams: Rc<Mutex<Streams>>,
    rtt: Rc<Mutex<Option<web_time::Duration>>>,
    checkpoint: Rc<Mutex<Option<(u32, u32)>>>,
    event_tx: mpsc::UnboundedSender<NetEvent>,
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
            *checkpoint.lock().unwrap() =
                Some((delivered.meta.checkpoint_tick, delivered.meta.checkpoint_digest));
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

async fn ctl_pump(
    player: usize,
    mut ctl_rx: ChannelReceiver,
    event_tx: mpsc::UnboundedSender<NetEvent>,
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

/// Resend the redundancy window on intervals where the tick sent
/// nothing (stall/pause), so acks and loss recovery keep flowing.
async fn heartbeat(
    data_tx: ChannelSender,
    streams: Rc<Mutex<Streams>>,
    sent: Rc<Cell<bool>>,
    stop: Rc<Cell<bool>>,
) {
    loop {
        TimeoutFuture::new(HEARTBEAT_MS).await;
        if stop.get() {
            return;
        }
        if sent.replace(false) {
            continue;
        }
        let bytes = {
            let s = streams.lock().unwrap();
            let w = s.out.window();
            Frame::new(w.base, s.inn.ack(), w.meta, w.entries).to_vec()
        };
        if data_tx.send(&bytes).is_err() {
            return;
        }
    }
}

/// The netplay session's per-tick body and teardown. The pump owns
/// pacing; `tick` assumes it is only called when the session should
/// attempt to advance one frame.
pub struct NetplayDriver {
    shared: Arc<SharedSession>,
    local_player: usize,
    local_rom: Vec<u8>,
    clock_unix_micros: u64,
    session: Session,
    throttler: Throttler,
    event_rx: mpsc::UnboundedReceiver<NetEvent>,
    peers: Vec<PeerCtx>,
    ctl_txs: Option<Vec<ChannelSender>>,
    connections: Option<Vec<PeerConnection>>,
    stop: Rc<Cell<bool>>,
    /// Per peer, when we last heard an input from them — consulted only
    /// when the queue watchdog trips, to name the peer that went silent.
    last_heard: Vec<Instant>,
    /// A rolling one-second window of advance times, for the measured
    /// TPS the telemetry panel charts against fps_target.
    tick_times: VecDeque<Instant>,
}

impl NetplayDriver {
    /// Attempt to advance one frame. Returns `false` once the session
    /// is over; [`finish`](Self::finish) has already run by then.
    pub fn tick(&mut self) -> bool {
        if let Some(end) = self.tick_inner() {
            self.finish(end);
            return false;
        }
        true
    }

    fn tick_inner(&mut self) -> Option<SessionEnd> {
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
        // out-stream and ship its whole redundancy window. Sends are
        // synchronous; the heartbeat task covers idle intervals.
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
            let _ = peer.data_tx.send(&bytes);
            peer.sent.set(true);
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

        // Discard newly-confirmed ticks: recording isn't wired up on web
        // yet, but the session buffers them until drained, so skipping
        // this would grow that buffer for the life of the session.
        self.session.drain_confirmed();

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
            .is_some_and(|t| now.duration_since(*t) > web_time::Duration::from_secs(1))
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

    /// Teardown: capture the unplug handoff BEFORE anything drops,
    /// announce a deliberate end to the peers, publish the end, and
    /// close the transports once their buffers drain.
    fn finish(&mut self, end: SessionEnd) {
        self.stop.set(true);

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
                        rom: std::mem::take(&mut self.local_rom),
                        state,
                        save,
                        rtc_unix_micros: self.clock_unix_micros,
                    });
                }
                Err(e) => log::error!("couldn't capture the unplug handoff: {e}"),
            }
        }

        // A deliberate local quit or unplug announces itself so peers end
        // at once instead of waiting out a transport EOF. Sends are sync;
        // the detached task keeps the connections alive until the control
        // channels drain (bounded), then closes them.
        let deliberate = matches!(end, SessionEnd::LocalQuit | SessionEnd::Unplugged);
        let ctl_txs = self.ctl_txs.take().unwrap_or_default();
        let connections = self.connections.take().unwrap_or_default();
        if deliberate {
            if let Ok(quit) = bincode::serialize(&PeerControl::Quit) {
                for ctl in &ctl_txs {
                    let _ = ctl.send(&quit);
                }
            }
        }
        wasm_bindgen_futures::spawn_local(async move {
            for _ in 0..10 {
                if ctl_txs.iter().all(|c| c.buffered_amount() == 0) {
                    break;
                }
                TimeoutFuture::new(50).await;
            }
            for pc in &connections {
                pc.close();
            }
            drop(connections);
        });

        self.shared.finish(end);
    }
}
