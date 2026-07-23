//! Session state: what a running emulator shares with the UI and the
//! audio pump, plus the per-kind drivers the runtime ticks. Two kinds —
//! netplay (rollback via `mgba_rollback::session::Session`) and local (a
//! plain link on one machine) — publish the same [`SharedSession`] so
//! the session view renders them uniformly.
//!
//! `playback/` (replay support, kept but not yet exposed on web) is
//! present in-tree but not compiled until its port lands.

pub mod local;
pub mod netplay;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// GBA cycles per second / cycles per frame — the exact tick rate.
pub const EXPECTED_FPS: f32 = 16777216.0 / 280896.0;

/// The netplay queue watchdog's trip depth: local inputs buffered with
/// nothing from a peer to match them before the link is declared dead
/// and the cable unplugs. A dead link keeps the throttled sim committing
/// ~one local input per displayed frame (the throttler caps its
/// slowdown, so it never fully stalls), so the local queue climbs
/// steadily; watching the queue itself — the resource that would
/// overflow — rather than a silence duration means the trip always fires
/// a fixed margin below the transport horizon (`HORIZON` = 600) no
/// matter how fast the throttled sim actually grows it. One delivered
/// datagram carries the peer's whole redundancy window, so anything
/// short of ~3s of total silence refills the queue instantly and never
/// trips. 180 frames ≈ 3s of play.
#[allow(dead_code)] // netplay (M5)
pub const UNPLUG_QUEUE_LENGTH: usize = 180;

/// Uniform access to a live link for audio readout, for sessions driven
/// through the rollback engine (which owns its link) and ones we drive
/// directly.
#[derive(Clone)]
pub enum LinkAccess {
    #[allow(dead_code)] // netplay (M5)
    Handle(mgba_rollback::session::LinkHandle),
    Shared(Arc<Mutex<mgba_rollback::Link>>),
}

impl LinkAccess {
    /// Run `f` against the live link.
    pub fn with_link<R>(&self, f: impl FnOnce(&mut mgba_rollback::Link) -> R) -> Option<R> {
        match self {
            LinkAccess::Handle(h) => Some(h.with_link(f)),
            LinkAccess::Shared(l) => Some(f(&mut l.lock().unwrap())),
        }
    }
}

#[allow(dead_code)] // netplay ends (M5)
#[derive(Debug, Clone)]
pub enum SessionEnd {
    LocalQuit,
    /// The local player pulled the cable: the netplay session ends but the
    /// local machine continues solo (see [`SharedSession::handoff`]).
    Unplugged,
    PeerQuit {
        player: usize,
    },
    PeerDisconnected {
        player: usize,
    },
    Desync {
        tick: u32,
    },
    Error(String),
}

impl SessionEnd {
    /// Whether the local machine survives this end — netplay teardown is a
    /// cable unplug, not a power-off, so anything short of a local quit or
    /// a dead emulator leaves a machine to keep playing.
    #[allow(dead_code)] // netplay (M5)
    pub fn unplugs(&self) -> bool {
        matches!(
            self,
            SessionEnd::Unplugged
                | SessionEnd::PeerQuit { .. }
                | SessionEnd::PeerDisconnected { .. }
                | SessionEnd::Desync { .. }
        )
    }
}

/// Which link peripheral a session's machines are wired to. The room
/// creator's choice; carried through boot payloads so every peer builds
/// the same kind of link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum LinkKind {
    #[default]
    Cable,
    Wireless,
}

impl LinkKind {
    pub fn peripheral(self) -> mgba_rollback::Peripheral {
        match self {
            LinkKind::Cable => mgba_rollback::Peripheral::Cable,
            LinkKind::Wireless => mgba_rollback::Peripheral::Wireless,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LinkKind::Cable => "Link cable",
            LinkKind::Wireless => "Wireless adapter",
        }
    }
}

/// The local side's continuation material when a netplay session ends:
/// everything a solo session needs to keep the machine running (the cable
/// unplugs, the game goes on).
pub struct Handoff {
    pub rom: Vec<u8>,
    /// Serialized core state (`Link::capture_boot_state`).
    pub state: Vec<u8>,
    /// SRAM/flash image at teardown.
    pub save: Option<Vec<u8>>,
    /// The session's pinned cart clock, carried into the continuation so
    /// RTC games don't see time jump.
    pub rtc_unix_micros: u64,
    /// The session's link peripheral: the continuation keeps the same
    /// hardware on the port (a wireless game keeps its adapter).
    pub link: LinkKind,
    /// The live adapter session on a wireless link: the continuation
    /// resumes it, and the departed peers simply fall out of range.
    pub adapter: Option<Vec<u8>>,
}

#[allow(dead_code)] // telemetry panel (M5)
#[derive(Debug, Clone, Default)]
pub struct PeerStat {
    #[allow(dead_code)]
    pub player: usize,
    pub nick: String,
    pub rtt_ms: Option<f32>,
}

/// Samples retained per metric (~3 s at the GBA tick rate), matching
/// tango's window.
pub const HISTORY_LEN: usize = 180;

/// One per-frame snapshot, kept in a ring buffer so each telemetry
/// metric can draw a sparkline. `pings` is indexed by peer slot (same
/// order as [`Stats::peers`]).
#[derive(Clone)]
pub struct MetricSample {
    pub tps: f32,
    pub fps_target: f32,
    pub skew: i32,
    pub lead: i32,
    pub depth: u32,
    pub pings: Vec<Option<f32>>,
}

impl MetricSample {
    pub fn capture(stats: &Stats) -> Self {
        Self {
            tps: stats.tps,
            fps_target: stats.fps_target,
            skew: stats.skew,
            lead: stats.queue_len as i32,
            depth: stats.rolled_back,
            pings: stats.peers.iter().map(|p| p.rtt_ms).collect(),
        }
    }
}

// Written by the drivers; read by the session view and telemetry panel.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub queue_len: u32,
    pub skew: i32,
    pub rolled_back: u32,
    pub confirmed: u32,
    pub frontier: u32,
    /// Peak sio run_loop slices a single simulated tick took inside the
    /// newest advance — the lockstep-livelock early-warning (normal is a
    /// few hundred to a few thousand; a sustained climb means the link is
    /// grinding toward the slice cap).
    pub slices_peak: u32,
    /// Actually achieved simulated ticks per second (measured over a
    /// rolling one-second window), as opposed to `fps_target` (the pace
    /// the throttler is currently asking for).
    pub tps: f32,
    pub fps_target: f32,
    pub peers: Vec<PeerStat>,
}

/// State shared between the driver, the audio pump, and the UI. One
/// instance per session, regardless of kind. On the web build all of it
/// lives on one thread, but the atomics/mutexes are kept — they're free
/// uncontended and the types stay `Send` for the engine's sake.
#[allow(dead_code)] // present_delay/unplug/handoff are the netplay surface (M5)
pub struct SharedSession {
    /// Latest presented frame: mGBA's raw little-endian BGR555,
    /// 240x160x2 bytes.
    pub vbuf: Mutex<Vec<u8>>,
    /// Bumped whenever `vbuf` changes, so the presenter knows to
    /// re-upload.
    pub vbuf_rev: AtomicU64,
    /// The pace the simulation is currently targeting, as f32 bits; the
    /// audio servo keys its faux clock off it. 0.0 = paused/silent.
    pub fps_target: AtomicU32,
    /// The local joypad, written by the runtime pump every tick.
    pub joyflags: AtomicU32,
    /// Which player's screen (and audio) to present. For netplay this is
    /// pinned to the local player; local sessions can switch.
    pub view_player: AtomicUsize,
    /// Netplay: present delay, adjustable live.
    pub present_delay: AtomicU32,
    /// Local: pause flag.
    pub paused: AtomicBool,
    /// Local: resume must also discard the old pacing deadline.
    /// This is separate from `paused` because a short pause can begin and
    /// end between two pumps; in that case the pump never observes
    /// `paused == true` and cannot reset its clock on its own.
    pace_reset_requested: AtomicBool,
    /// Local: speed percent (100 = 1x), for hold-to-fast-forward.
    pub speed: AtomicU32,
    /// UI → driver: end the session.
    pub quit: AtomicBool,
    /// UI → netplay driver: pull the cable (end the session, but leave a
    /// handoff for the solo continuation).
    pub unplug: AtomicBool,
    /// Driver → UI: why the session ended.
    pub end: Mutex<Option<SessionEnd>>,
    /// Netplay driver → UI: the local machine's continuation, captured at
    /// teardown whenever the end [`unplugs`](SessionEnd::unplugs).
    pub handoff: Mutex<Option<Handoff>>,
    pub stats: Mutex<Stats>,
}

impl SharedSession {
    pub fn new(present_delay: u32) -> Arc<SharedSession> {
        Arc::new(SharedSession {
            vbuf: Mutex::new(vec![0; crate::platform::video::SCREEN_BYTES]),
            vbuf_rev: AtomicU64::new(0),
            fps_target: AtomicU32::new(0f32.to_bits()),
            joyflags: AtomicU32::new(0),
            view_player: AtomicUsize::new(0),
            present_delay: AtomicU32::new(present_delay),
            paused: AtomicBool::new(false),
            pace_reset_requested: AtomicBool::new(false),
            speed: AtomicU32::new(100),
            quit: AtomicBool::new(false),
            unplug: AtomicBool::new(false),
            end: Mutex::new(None),
            handoff: Mutex::new(None),
            stats: Mutex::new(Stats::default()),
        })
    }

    pub fn set_fps_target(&self, fps: f32) {
        self.fps_target.store(fps.to_bits(), Ordering::Relaxed);
    }

    /// Resume a locally paced session without trying to make up time spent
    /// paused. The reset request is published before the pause flag clears,
    /// so a pump that observes the resume also observes the reset.
    pub fn resume(&self) {
        self.pace_reset_requested.store(true, Ordering::Relaxed);
        self.paused.store(false, Ordering::Release);
    }

    pub(crate) fn take_pace_reset(&self) -> bool {
        self.pace_reset_requested.swap(false, Ordering::Relaxed)
    }

    /// Publish the presented core's raw BGR555 frame.
    pub fn publish_video(&self, bgr555: &[u8]) {
        let mut vbuf = self.vbuf.lock().unwrap();
        if vbuf.len() != bgr555.len() {
            vbuf.resize(bgr555.len(), 0);
        }
        vbuf.copy_from_slice(bgr555);
        drop(vbuf);
        self.vbuf_rev.fetch_add(1, Ordering::Release);
    }

    pub fn finish(&self, end: SessionEnd) {
        let mut slot = self.end.lock().unwrap();
        if slot.is_none() {
            *slot = Some(end);
        }
        drop(slot);
        self.set_fps_target(0.0);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    #[allow(dead_code)] // netplay (M5)
    Netplay,
    Local,
}

/// What the session view needs to label things.
#[allow(dead_code)] // read by the session view as its port lands (M4/M5)
pub struct SessionDescriptor {
    pub kind: SessionKind,
    pub local_player: usize,
    pub nicks: Vec<String>,
    pub room_code: Option<String>,
    /// The local player's ROM identity, for opening a lobby from a
    /// running session.
    pub rom_crc32: Option<u32>,
    /// The link peripheral on this session's machines.
    pub link: LinkKind,
}

/// Deepen every core's audio buffer past mgba's 2048 default so servo
/// regulation has room, and drop anything buffered during boot.
pub fn prepare_audio_buffers(link: &mut mgba_rollback::Link) {
    for i in 0..link.num_players() {
        let mut core = link.core_mut(i);
        core.set_audio_buffer_size(16384);
        core.audio_buffer().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_requests_one_pacer_reset() {
        let shared = SharedSession::new(0);
        shared.paused.store(true, Ordering::Relaxed);

        shared.resume();

        assert!(!shared.paused.load(Ordering::Acquire));
        assert!(shared.take_pace_reset());
        assert!(!shared.take_pace_reset());
    }
}
