//! Session runtimes: the emulator drive thread plus the state it shares
//! with the UI and audio threads. Three kinds — netplay (rollback via
//! `mgba_siolink::session::Session`), local (a plain link on one
//! machine), and replay playback — all publish the same [`SharedSession`]
//! so the session view renders them uniformly.

pub mod local;
pub mod netplay;
pub mod playback;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// GBA cycles per second / cycles per frame — the exact tick rate.
pub const EXPECTED_FPS: f32 = 16777216.0 / 280896.0;

/// Local inputs buffered without confirmation before the drive loop
/// stops advancing (the peer is stalled or gone; keep the horizon
/// recoverable).
pub const STALL_QUEUE_LENGTH: usize = 180;

/// Sleep quantum while stalled or paused.
pub const PAUSED_TICK: std::time::Duration = std::time::Duration::from_millis(10);

/// Uniform access to the live link for off-thread readout (audio), for
/// sessions driven through the rollback engine (which owns its link) and
/// ones we drive directly.
#[derive(Clone)]
pub enum LinkAccess {
    Handle(mgba_siolink::session::LinkHandle),
    Shared(Arc<Mutex<mgba_siolink::Link>>),
    /// Playback's link, behind a try-lock: a seek chase can hold the
    /// mutex for a while, and the audio callback would rather play
    /// silence than stall.
    Playback(playback::engine::SharedPlayback),
}

impl LinkAccess {
    /// Run `f` against the live link. `None` means the link is
    /// unavailable right now (still booting, or contended by a seek
    /// chase) — callers should treat it as silence/skip.
    pub fn with_link<R>(&self, f: impl FnOnce(&mut mgba_siolink::Link) -> R) -> Option<R> {
        match self {
            LinkAccess::Handle(h) => Some(h.with_link(f)),
            LinkAccess::Shared(l) => Some(f(&mut l.lock().unwrap())),
            LinkAccess::Playback(p) => match p.try_lock() {
                Ok(mut guard) => guard.as_mut().map(|pb| f(pb.link_mut())),
                Err(_) => None,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub enum SessionEnd {
    LocalQuit,
    PeerQuit { player: usize },
    PeerDisconnected { player: usize },
    Desync { tick: u32 },
    Error(String),
}

#[derive(Debug, Clone, Default)]
pub struct PeerStat {
    #[allow(dead_code)]
    pub player: usize,
    pub nick: String,
    pub rtt_ms: Option<f32>,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub queue_len: u32,
    pub skew: i32,
    pub rolled_back: u32,
    pub confirmed: u32,
    pub frontier: u32,
    pub fps_target: f32,
    pub peers: Vec<PeerStat>,
}

/// State shared between the drive thread, the audio callback, and the
/// UI. One instance per session, regardless of kind.
pub struct SharedSession {
    /// Latest presented frame, converted to RGBA8 (240x160x4).
    pub vbuf: Mutex<Vec<u8>>,
    /// Bumped whenever `vbuf` changes, so the UI knows to re-upload.
    pub vbuf_rev: AtomicU64,
    /// The pace the simulation is currently targeting, as f32 bits; the
    /// audio servo keys its faux clock off it. 0.0 = paused/silent.
    pub fps_target: AtomicU32,
    /// The local joypad, written by the UI thread every frame.
    pub joyflags: AtomicU32,
    /// Which player's screen (and audio) to present. For netplay this is
    /// pinned to the local player; local/playback can switch.
    pub view_player: AtomicUsize,
    /// Netplay: present delay, adjustable live.
    pub present_delay: AtomicU32,
    /// Local/playback: pause flag.
    pub paused: AtomicBool,
    /// Playback: speed percent (100 = 1x).
    pub speed: AtomicU32,
    /// Playback: current tick / total ticks.
    pub position: AtomicU32,
    pub total_ticks: AtomicU32,
    /// UI → drive: end the session.
    pub quit: AtomicBool,
    /// Drive → UI: why the session ended.
    pub end: Mutex<Option<SessionEnd>>,
    pub stats: Mutex<Stats>,
    /// Signaled once per presented frame; the UI subscription awaits it
    /// to redraw in lockstep with the emulator instead of on a timer.
    /// App-lifetime (shared across sessions) so the iced subscription's
    /// identity stays stable.
    pub frame_notify: Arc<tokio::sync::Notify>,
}

impl SharedSession {
    pub fn new(present_delay: u32, frame_notify: Arc<tokio::sync::Notify>) -> Arc<SharedSession> {
        Arc::new(SharedSession {
            vbuf: Mutex::new(vec![0; crate::platform::video::SCREEN_BYTES]),
            vbuf_rev: AtomicU64::new(0),
            fps_target: AtomicU32::new(0f32.to_bits()),
            joyflags: AtomicU32::new(0),
            view_player: AtomicUsize::new(0),
            present_delay: AtomicU32::new(present_delay),
            paused: AtomicBool::new(false),
            speed: AtomicU32::new(100),
            position: AtomicU32::new(0),
            total_ticks: AtomicU32::new(0),
            quit: AtomicBool::new(false),
            end: Mutex::new(None),
            stats: Mutex::new(Stats::default()),
            frame_notify,
        })
    }

    pub fn set_fps_target(&self, fps: f32) {
        self.fps_target.store(fps.to_bits(), Ordering::Relaxed);
    }

    /// Publish the presented core's raw BGR555 frame and wake the UI.
    pub fn publish_video(&self, bgr555: &[u8]) {
        let mut vbuf = self.vbuf.lock().unwrap();
        if vbuf.len() != bgr555.len() {
            vbuf.resize(bgr555.len(), 0);
        }
        vbuf.copy_from_slice(bgr555);
        drop(vbuf);
        self.vbuf_rev.fetch_add(1, Ordering::Release);
        self.frame_notify.notify_one();
    }

    pub fn finish(&self, end: SessionEnd) {
        let mut slot = self.end.lock().unwrap();
        if slot.is_none() {
            *slot = Some(end);
        }
        drop(slot);
        self.set_fps_target(0.0);
        self.frame_notify.notify_one();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Netplay,
    Local,
    Playback,
}

/// What the session view needs to label things.
pub struct SessionDescriptor {
    pub kind: SessionKind,
    pub num_players: usize,
    #[allow(dead_code)]
    pub local_player: usize,
    pub nicks: Vec<String>,
    pub room_code: Option<String>,
    pub replay_path: Option<std::path::PathBuf>,
}

pub struct SessionRuntime {
    pub shared: Arc<SharedSession>,
    pub descriptor: SessionDescriptor,
    /// Playback-only: the seek controller + snapshot stores the scrub
    /// UI drives.
    pub playback: Option<playback::PlaybackHandles>,
    /// Keeps the session's audio source bound into the host stream;
    /// dropping it returns the output to silence.
    _audio: Option<crate::platform::audio::Binding>,
    /// Extra teardown to run before joining (e.g. waking the seek
    /// worker so it can exit).
    pre_join: Option<Box<dyn FnOnce() + Send>>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Drop for SessionRuntime {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Relaxed);
        if let Some(pre_join) = self.pre_join.take() {
            pre_join();
        }
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

/// Fixed-timestep pacer: accumulate the target period, sleep the
/// remainder, and resync (rather than sprint) after a long stall.
pub struct Pacer {
    next_tick: std::time::Instant,
}

impl Pacer {
    pub fn new() -> Self {
        Pacer {
            next_tick: std::time::Instant::now(),
        }
    }

    /// The loop skipped this frame (paused/stalled): drop the cadence.
    pub fn reset(&mut self) {
        self.next_tick = std::time::Instant::now();
    }

    pub fn pace(&mut self, target_fps: f32) {
        let target_fps = target_fps.max(1.0);
        self.next_tick += std::time::Duration::from_secs_f64(1.0 / target_fps as f64);
        let now = std::time::Instant::now();
        if self.next_tick > now {
            std::thread::sleep(self.next_tick - now);
        } else if now - self.next_tick > std::time::Duration::from_millis(250) {
            self.next_tick = now;
        }
    }
}

/// Point every core's frameskip at `view` (rendering is invisible to the
/// simulation, so this is rollback-safe).
pub fn apply_view_frameskip(link: &mut mgba_siolink::Link, view: usize) {
    for i in 0..link.num_players() {
        link.set_frameskip(i, if i == view { 0 } else { i32::MAX });
    }
}

/// Deepen every core's audio buffer past mgba's 2048 default so servo
/// regulation has room, and drop anything buffered during boot.
pub fn prepare_audio_buffers(link: &mut mgba_siolink::Link) {
    for i in 0..link.num_players() {
        let mut core = link.core_mut(i);
        core.set_audio_buffer_size(16384);
        core.audio_buffer().clear();
    }
}
