//! Replay playback machinery, ported from tango's SIO playback engine:
//! the linearly-driven link, its snapshot stores, the background
//! prefetch body, and the async seek coordination.
//!
//! A gbaroll replay is the boot configuration plus one continuous run of
//! confirmed player-indexed key rows. The link is deterministic, so
//! playback is a linear re-sim — and *any* recorded tick can be reached
//! by loading the nearest link [`Snapshot`] at or before it and stepping
//! forward. The prefetch worker races its own link through the whole
//! stream filling a keyframe [`SnapshotStore`]; a [`RewindRing`] keeps
//! every tick of the last ~1.5s so short backward steps land on exact
//! snapshots; and seeks are asynchronous — requests land on a
//! [`SeekController`] and a dedicated worker chases the newest target,
//! so the UI never blocks on catch-up emulation.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// Floor on the keyframe interval — never snapshot more than once a
/// second of game time.
pub const KEYFRAME_INTERVAL_MIN: u32 = 60;

/// Rough keyframe memory budget, in whole-link snapshots normalized to
/// two cores (a 4-player snapshot counts double). Long recordings widen
/// the interval instead of blowing the budget.
const KEYFRAME_BUDGET_2P: u32 = 400;

/// Depth of the [`RewindRing`]'s per-tick window behind the playhead.
pub const REWIND_FRAMES: u32 = 90;

/// Hard cap on rewind-ring entries for a 2-player link; scaled down for
/// wider links (their snapshots are proportionally bigger).
const REWIND_MAX_ENTRIES_2P: usize = 192;

/// A whole-link snapshot poised at `tick` (= input rows consumed),
/// carrying every core's rendered frame so previews and view-switch
/// blits never need emulation.
pub struct Snapshot {
    pub tick: u32,
    pub state: mgba_siolink::Snapshot,
    /// Every core's framebuffer (native BGR555), indexed by player.
    pub framebuffers: Vec<Vec<u8>>,
}

/// Sparse keyframe store covering the whole replay, shared between the
/// prefetch worker, the drive loop, and the seek chase.
#[derive(Clone)]
pub struct SnapshotStore {
    entries: Arc<Mutex<BTreeMap<u32, Arc<Snapshot>>>>,
    interval: u32,
}

impl SnapshotStore {
    /// Budget the keyframe interval by recording length and player
    /// count so memory stays bounded.
    pub fn new(total_ticks: u32, num_players: usize) -> Self {
        let budget = (KEYFRAME_BUDGET_2P * 2 / num_players.max(2) as u32).max(16);
        let interval = (total_ticks / budget).max(KEYFRAME_INTERVAL_MIN);
        Self {
            entries: Arc::new(Mutex::new(BTreeMap::new())),
            interval,
        }
    }

    /// True if no keyframe exists within the interval at or before
    /// `tick` — capturing here fills a gap.
    pub fn snapshot_needed(&self, tick: u32) -> bool {
        let lo = tick.saturating_sub(self.interval);
        self.entries
            .lock()
            .unwrap()
            .range((std::ops::Bound::Excluded(lo), std::ops::Bound::Included(tick)))
            .next()
            .is_none()
    }

    pub fn push(&self, snap: Arc<Snapshot>) {
        self.entries.lock().unwrap().insert(snap.tick, snap);
    }

    /// Largest keyframe with `tick <= target`, if any.
    pub fn best_at_or_before(&self, target: u32) -> Option<Arc<Snapshot>> {
        self.entries
            .lock()
            .unwrap()
            .range(..=target)
            .next_back()
            .map(|(_, s)| s.clone())
    }

    /// Largest keyframe with `lo_exclusive < tick <= hi_inclusive`.
    pub fn best_in_range(&self, lo_exclusive: u32, hi_inclusive: u32) -> Option<Arc<Snapshot>> {
        self.entries
            .lock()
            .unwrap()
            .range((
                std::ops::Bound::Excluded(lo_exclusive),
                std::ops::Bound::Included(hi_inclusive),
            ))
            .next_back()
            .map(|(_, s)| s.clone())
    }

    /// Keyframe closest to `target` on either side, if any.
    pub fn nearest(&self, target: u32) -> Option<Arc<Snapshot>> {
        let entries = self.entries.lock().unwrap();
        let below = entries.range(..=target).next_back();
        let above = entries
            .range((std::ops::Bound::Excluded(target), std::ops::Bound::Unbounded))
            .next();
        [below, above]
            .into_iter()
            .flatten()
            .min_by_key(|(k, _)| k.abs_diff(target))
            .map(|(_, s)| s.clone())
    }
}

/// Rolling per-tick snapshot window trailing the playhead: every tick
/// the playback link runs (normal playback and seek chases alike) is
/// captured, so short backward steps land on exact snapshots.
#[derive(Clone)]
pub struct RewindRing(Arc<RewindRingInner>);

struct RewindRingInner {
    entries: Mutex<BTreeMap<u32, Arc<Snapshot>>>,
    anchor: AtomicU32,
    keyframe_interval: u32,
    max_entries: usize,
}

impl RewindRing {
    pub fn new(num_players: usize, keyframe_interval: u32) -> Self {
        Self(Arc::new(RewindRingInner {
            entries: Mutex::new(BTreeMap::new()),
            anchor: AtomicU32::new(0),
            keyframe_interval,
            max_entries: (REWIND_MAX_ENTRIES_2P * 2 / num_players.max(2)).max(32),
        }))
    }

    /// Re-anchor the window at `tick` (each seek chase's target); normal
    /// playback captures only ever raise it.
    pub fn set_anchor(&self, tick: u32) {
        self.0.anchor.store(tick, Ordering::Release);
    }

    pub fn insert(&self, snap: Arc<Snapshot>) {
        // Forward playback drags the anchor along; captures below it
        // (seek catch-up runs) leave it where the chase put it.
        self.0.anchor.fetch_max(snap.tick, Ordering::AcqRel);
        let anchor = self.0.anchor.load(Ordering::Acquire);
        let mut entries = self.0.entries.lock().unwrap();
        entries.insert(snap.tick, snap);
        let keep_from = anchor.saturating_sub(REWIND_FRAMES + self.0.keyframe_interval + 1);
        while let Some((&lo, _)) = entries.first_key_value() {
            if lo < keep_from {
                entries.pop_first();
            } else {
                break;
            }
        }
        while entries.len() > self.0.max_entries {
            let (&lo, _) = entries.first_key_value().unwrap();
            let (&hi, _) = entries.last_key_value().unwrap();
            if lo.abs_diff(anchor) >= hi.abs_diff(anchor) {
                entries.pop_first();
            } else {
                entries.pop_last();
            }
        }
    }

    pub fn best_at_or_before(&self, target: u32) -> Option<Arc<Snapshot>> {
        self.0
            .entries
            .lock()
            .unwrap()
            .range(..=target)
            .next_back()
            .map(|(_, s)| s.clone())
    }

    pub fn best_in_range(&self, lo_exclusive: u32, hi_inclusive: u32) -> Option<Arc<Snapshot>> {
        self.0
            .entries
            .lock()
            .unwrap()
            .range((
                std::ops::Bound::Excluded(lo_exclusive),
                std::ops::Bound::Included(hi_inclusive),
            ))
            .next_back()
            .map(|(_, s)| s.clone())
    }

    pub fn nearest(&self, target: u32) -> Option<Arc<Snapshot>> {
        let entries = self.0.entries.lock().unwrap();
        let below = entries.range(..=target).next_back();
        let above = entries
            .range((std::ops::Bound::Excluded(target), std::ops::Bound::Unbounded))
            .next();
        [below, above]
            .into_iter()
            .flatten()
            .min_by_key(|(k, _)| k.abs_diff(target))
            .map(|(_, s)| s.clone())
    }
}

/// Everything needed to boot a playback link, in player order.
#[derive(Clone)]
pub struct BootConfig {
    pub roms: Vec<Vec<u8>>,
    /// Per player, the encoded boot payload the recording started from
    /// (the replay's `PlayerMeta::boot`). All-or-nothing: either every
    /// side has one (the session was a plugged-in cable) or none does (a
    /// power-on boot, e.g. synthesized test replays).
    pub boots: Vec<Option<Vec<u8>>>,
    pub rtc_unix_micros: Option<u64>,
}

impl BootConfig {
    pub fn num_players(&self) -> usize {
        self.roms.len()
    }

    /// Boot a link poised at tick 0: the recorded plug-in captures when
    /// the replay carries them, a hard reset otherwise. Either way the
    /// recording's tick 0 is exactly this state — no priming.
    pub fn boot(&self) -> anyhow::Result<mgba_siolink::Link> {
        let rtc = Some(
            self.rtc_unix_micros
                .map(|us| std::time::UNIX_EPOCH + std::time::Duration::from_micros(us))
                .unwrap_or_else(|| std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000)),
        );
        let mut link = if self.boots.iter().all(|b| b.is_some()) {
            let sides = self
                .roms
                .iter()
                .cloned()
                .zip(self.boots.iter())
                .map(|(rom, boot)| {
                    let blob = crate::net::protocol::BootBlob::decode(boot.as_deref().unwrap())?;
                    anyhow::Ok(mgba_siolink::BootSide {
                        rom,
                        save: blob.save,
                        state: blob.state,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            mgba_siolink::Link::from_states(sides, rtc)?
        } else {
            anyhow::ensure!(
                self.boots.iter().all(|b| b.is_none()),
                "replay mixes captured and power-on sides"
            );
            mgba_siolink::Link::with_options(mgba_siolink::LinkOptions {
                sides: self
                    .roms
                    .iter()
                    .cloned()
                    .map(|rom| mgba_siolink::SideOptions { rom, save: None })
                    .collect(),
                rtc,
            })?
        };
        crate::session::prepare_audio_buffers(&mut link);
        Ok(link)
    }
}

/// The playback link plus the recorded input stream and a cursor. The
/// host wraps it in a mutex — the drive loop, the seek chase, and the
/// audio pull interleave on that lock.
pub struct Playback {
    link: mgba_siolink::Link,
    inputs: Arc<Vec<Vec<u32>>>,
    cursor: u32,
}

pub type SharedPlayback = Arc<Mutex<Option<Playback>>>;

impl Playback {
    pub fn new(config: &BootConfig, inputs: Arc<Vec<Vec<u32>>>) -> anyhow::Result<Self> {
        Ok(Self {
            link: config.boot()?,
            inputs,
            cursor: 0,
        })
    }

    /// Input rows consumed so far = the playhead tick.
    pub fn cursor(&self) -> u32 {
        self.cursor
    }

    pub fn total(&self) -> u32 {
        self.inputs.len() as u32
    }

    pub fn at_end(&self) -> bool {
        self.cursor >= self.total()
    }

    /// Feed the next recorded input row. Returns false at end-of-stream.
    pub fn step(&mut self) -> bool {
        let Some(keys) = self.inputs.get(self.cursor as usize) else {
            return false;
        };
        self.link.tick(keys);
        self.cursor += 1;
        true
    }

    /// Capture a whole-link snapshot (with every framebuffer) at the
    /// current cursor.
    pub fn capture(&mut self) -> anyhow::Result<Arc<Snapshot>> {
        Ok(Arc::new(capture(&mut self.link, self.cursor)))
    }

    /// Restore the link to `snap` and move the cursor there.
    pub fn load(&mut self, snap: &Snapshot) -> anyhow::Result<()> {
        self.link.load(&snap.state)?;
        self.cursor = snap.tick;
        Ok(())
    }

    /// Direct link access, for video/audio readout.
    pub fn link_mut(&mut self) -> &mut mgba_siolink::Link {
        &mut self.link
    }
}

fn capture(link: &mut mgba_siolink::Link, tick: u32) -> Snapshot {
    let state = link.save().expect("link snapshot");
    let framebuffers = (0..link.num_players())
        .map(|i| link.video_buffer(i).map(|b| b.to_vec()).unwrap_or_default())
        .collect();
    Snapshot {
        tick,
        state,
        framebuffers,
    }
}

/// Body of the seek worker thread. Sleeps until a [`SeekController`]
/// request lands, then chases the newest target on the playback link:
/// load the best snapshot at or before it (rewind ring ∪ keyframe
/// store), step forward feeding the recorded inputs, capturing every
/// tick on the way (the ring backfills itself), and publish the landing
/// frame. Newer requests supersede an in-flight chase at the next tick
/// boundary.
///
/// The link mutex is held for a chase's duration: the drive loop just
/// waits its turn (it re-paces on wake), and the audio pull uses
/// `try_lock` so it plays silence rather than stalling. Backward seeks
/// with no snapshot at or before the target fall back to tick 0 (the
/// prefetcher stores a boot keyframe immediately, so this is rare).
pub fn run_seek_worker(
    ctrl: &SeekController,
    playback: &Mutex<Option<Playback>>,
    store: &SnapshotStore,
    rewind: &RewindRing,
    on_progress: &mut dyn FnMut(u32),
    publish_landing: &mut dyn FnMut(&Snapshot),
    on_resume: &mut dyn FnMut(),
) {
    while ctrl.wait_for_request() {
        ctrl.begin_pass();
        'plan: loop {
            let target = ctrl.take_target();
            rewind.set_anchor(target);

            let mut guard = playback.lock().unwrap();
            let Some(pb) = guard.as_mut() else {
                // Still booting — drop the request; the user can seek
                // again once the link is up.
                break 'plan;
            };

            let cur = pb.cursor();
            let start = if target < cur {
                let best = [rewind.best_at_or_before(target), store.best_at_or_before(target)]
                    .into_iter()
                    .flatten()
                    .max_by_key(|s| s.tick);
                match best {
                    Some(snap) => Some(snap),
                    None => break 'plan,
                }
            } else {
                [
                    rewind.best_in_range(cur, target.max(cur)),
                    store.best_in_range(cur, target.max(cur)),
                ]
                .into_iter()
                .flatten()
                .max_by_key(|s| s.tick)
            };

            if let Some(snap) = &start {
                rewind.insert(snap.clone());
                if let Err(e) = pb.load(snap) {
                    log::error!("seek: snapshot load failed: {e:?}");
                    break 'plan;
                }
                on_progress(pb.cursor());
                if snap.tick >= target {
                    publish_landing(snap);
                    break 'plan;
                }
            }

            let mut landing: Option<Arc<Snapshot>> = None;
            while pb.cursor() < target {
                if ctrl.is_cancelled() {
                    ctrl.end_pass();
                    return;
                }
                if ctrl.is_dirty() {
                    drop(guard);
                    continue 'plan;
                }
                if !pb.step() {
                    break;
                }
                on_progress(pb.cursor());
                match pb.capture() {
                    Ok(snap) => {
                        if store.snapshot_needed(snap.tick) {
                            store.push(snap.clone());
                        }
                        rewind.insert(snap.clone());
                        landing = Some(snap);
                    }
                    Err(e) => log::warn!("seek: capture failed: {e:?}"),
                }
            }
            // The catch-up run pushed fast-forward audio into the cores'
            // buffers; purge it so the callback doesn't play a garbled
            // burst.
            let link = pb.link_mut();
            for i in 0..link.num_players() {
                link.core_mut(i).audio_buffer().clear();
            }
            if let Some(snap) = landing {
                publish_landing(&snap);
            }
            break 'plan;
        }
        ctrl.end_pass();

        if !ctrl.is_dirty() && ctrl.take_resume() {
            on_resume();
        }
    }
}

/// Body of the background prefetch worker: boots its own link and runs
/// the whole recorded stream as fast as the host allows, capturing a
/// keyframe at the store's interval and publishing the playhead-scale
/// progress. Rendering stays on so keyframes carry framebuffers for
/// scrub previews.
pub fn run_prefetch(
    config: &BootConfig,
    inputs: &[Vec<u32>],
    store: SnapshotStore,
    progress: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let mut link = config.boot()?;

    // Keyframe at tick 0: the boot state every backward seek bottoms
    // out on.
    store.push(Arc::new(capture(&mut link, 0)));

    for (i, keys) in inputs.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let tick = i as u32 + 1;
        link.tick(keys);
        if store.snapshot_needed(tick) {
            store.push(Arc::new(capture(&mut link, tick)));
        }
        progress.store(tick, Ordering::Relaxed);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Seek coordination (host-facing half of the seek machinery).

/// Coordination state between seek requesters (the UI thread) and the
/// seek worker thread. Requests coalesce: only the most recent target
/// matters, and an in-flight chase retargets mid-loop instead of
/// finishing stale work.
pub struct SeekController {
    /// Latest requested absolute tick.
    target: AtomicU32,
    /// `target` holds a request no chase has consumed yet.
    dirty: AtomicBool,
    /// A chase is currently running on the playback link.
    chasing: AtomicBool,
    /// Unpause the playback thread once the chase lands (set by seeks
    /// that paused playback for the duration, e.g. a scrub drag).
    resume: AtomicBool,
    /// Tells the worker and any in-flight chase to exit.
    cancel: AtomicBool,
    wake_mutex: Mutex<()>,
    wake_cv: Condvar,
}

impl Default for SeekController {
    fn default() -> Self {
        Self::new()
    }
}

impl SeekController {
    pub fn new() -> Self {
        Self {
            target: AtomicU32::new(0),
            dirty: AtomicBool::new(false),
            chasing: AtomicBool::new(false),
            resume: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            wake_mutex: Mutex::new(()),
            wake_cv: Condvar::new(),
        }
    }

    /// Record `target` as the newest seek request and wake the worker.
    /// Supersedes any not-yet-landed request. Never blocks on the core.
    pub fn request(&self, target: u32, resume_after: bool) {
        self.target.store(target, Ordering::Release);
        self.resume.store(resume_after, Ordering::Release);
        self.dirty.store(true, Ordering::Release);
        // Hold the wake mutex across notify so the signal can't slip
        // between the worker's dirty check and its wait.
        let _guard = self.wake_mutex.lock().unwrap();
        self.wake_cv.notify_one();
    }

    /// Permanently stop the worker (and abort any in-flight chase).
    pub fn shutdown(&self) {
        self.cancel.store(true, Ordering::Release);
        let _guard = self.wake_mutex.lock().unwrap();
        self.wake_cv.notify_one();
    }

    /// Target of the not-yet-landed seek, if any. Lets the UI draw the
    /// playhead where it's headed instead of where the core still is.
    pub fn pending_target(&self) -> Option<u32> {
        (self.dirty.load(Ordering::Acquire) || self.chasing.load(Ordering::Acquire))
            .then(|| self.target.load(Ordering::Acquire))
    }

    /// True while a not-yet-landed seek will unpause playback when it
    /// lands — the UI should keep displaying the playing state.
    pub fn resume_pending(&self) -> bool {
        (self.dirty.load(Ordering::Acquire) || self.chasing.load(Ordering::Acquire))
            && self.resume.load(Ordering::Acquire)
    }

    /// Withdraw a pending resume: the seek still lands, but playback
    /// stays paused afterwards.
    pub fn clear_resume(&self) {
        self.resume.store(false, Ordering::Release);
    }

    /// Block until a request lands ([`Self::request`]) or the controller
    /// shuts down. Returns false on shutdown.
    pub fn wait_for_request(&self) -> bool {
        let mut guard = self.wake_mutex.lock().unwrap();
        loop {
            if self.cancel.load(Ordering::Acquire) {
                return false;
            }
            if self.dirty.load(Ordering::Acquire) {
                return true;
            }
            guard = self.wake_cv.wait(guard).unwrap();
        }
    }

    /// Mark a chase pass running — [`Self::pending_target`] keeps
    /// reporting until [`Self::end_pass`].
    pub fn begin_pass(&self) {
        self.chasing.store(true, Ordering::Release);
    }

    pub fn end_pass(&self) {
        self.chasing.store(false, Ordering::Release);
    }

    /// Consume the pending request: clears dirty and returns the target.
    /// Order matters — dirty clears before the read, so a request racing
    /// in re-flags for the next pass instead of being lost.
    pub fn take_target(&self) -> u32 {
        self.dirty.store(false, Ordering::Release);
        self.target.load(Ordering::Acquire)
    }

    /// A newer request landed mid-pass — abandon the current chase.
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    /// Consume a pending resume-on-landing, if one was requested.
    pub fn take_resume(&self) -> bool {
        self.resume.swap(false, Ordering::AcqRel)
    }
}
