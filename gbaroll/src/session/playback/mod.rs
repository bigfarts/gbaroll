//! Replay playback session: a linearly-driven link behind a mutex,
//! paced by a drive thread; a prefetch link races ahead of the playhead
//! filling a keyframe [`engine::SnapshotStore`], a [`engine::RewindRing`]
//! keeps every tick of the last ~1.5s so short backward steps land on
//! exact snapshots, and seeks are asynchronous — requests land on a
//! [`engine::SeekController`] and a dedicated worker chases the newest
//! target, so the UI never blocks on catch-up emulation. Rewind states are
//! sampled rather than serialized every frame so playback stays real-time.

pub mod engine;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use engine::{
    BootConfig, Playback, RewindRing, SeekController, SharedPlayback, Snapshot, SnapshotStore,
};

use crate::session::{
    Pacer, SessionDescriptor, SessionEnd, SessionKind, SessionRuntime, SharedSession, EXPECTED_FPS,
    PAUSED_TICK,
};

pub struct PlaybackArgs {
    pub replay: gbaroll_replay::Replay,
    /// Per player, the ROM bytes (resolved from the library by the
    /// replay's per-player CRC32s).
    pub roms: Vec<Vec<u8>>,
}

/// What the scrub UI drives: async seeks plus the snapshot stores it
/// blits drag previews from.
pub struct PlaybackHandles {
    pub seek: Arc<SeekController>,
    pub snapshots: SnapshotStore,
    pub rewind: RewindRing,
    pub prefetch_progress: Arc<AtomicU32>,
}

impl PlaybackHandles {
    /// The captured snapshot nearest `target`, if any — near the
    /// playhead the rewind window supplies exact frames; elsewhere it's
    /// the store's keyframes.
    pub fn nearest_snapshot(&self, target: u32) -> Option<Arc<Snapshot>> {
        [self.snapshots.nearest(target), self.rewind.nearest(target)]
            .into_iter()
            .flatten()
            .min_by_key(|s| s.tick.abs_diff(target))
    }
}

pub fn start(
    args: PlaybackArgs,
    audio_binder: &crate::platform::audio::LateBinder,
    frame_notify: Arc<tokio::sync::Notify>,
) -> anyhow::Result<SessionRuntime> {
    let num_players = args.replay.num_players();
    assert_eq!(args.roms.len(), num_players);

    let boot = BootConfig {
        roms: args.roms,
        boots: args
            .replay
            .metadata
            .players
            .iter()
            .map(|p| p.boot.clone())
            .collect(),
        rtc_unix_micros: args.replay.metadata.rtc_unix_micros,
    };
    let inputs: Arc<Vec<Vec<u32>>> = Arc::new(args.replay.inputs);
    let total_ticks = inputs.len() as u32;
    let view = args.replay.metadata.local_player as usize;

    let shared = SharedSession::new(0, frame_notify);
    shared
        .view_player
        .store(view.min(num_players - 1), Ordering::Relaxed);
    shared.total_ticks.store(total_ticks, Ordering::Relaxed);

    let playback: SharedPlayback = Arc::new(Mutex::new(None));
    let snapshots = SnapshotStore::new(total_ticks, num_players);
    let rewind = RewindRing::new(num_players, engine::KEYFRAME_INTERVAL_MIN);
    let seek = Arc::new(SeekController::new());
    let prefetch_progress = Arc::new(AtomicU32::new(0));
    let prefetch_cancel = Arc::new(AtomicBool::new(false));

    let descriptor = SessionDescriptor {
        kind: SessionKind::Playback,
        num_players,
        local_player: view,
        nicks: args
            .replay
            .metadata
            .players
            .iter()
            .map(|p| p.nick.clone())
            .collect(),
        room_code: None,
        rom_crc32: None,
    };

    let replay_audio = crate::platform::audio::ReplayAudioQueue::new(audio_binder.sample_rate());
    let audio = audio_binder
        .bind(Some(Box::new(crate::platform::audio::ReplayStream::new(
            replay_audio.clone(),
            shared.clone(),
        ))))
        .ok();

    let mut threads = Vec::new();

    // The drive thread: boots the link (black + silence until it's up),
    // then paces the linear re-sim at the published fps target,
    // sampling recent rewind states (keyframes shared into the store)
    // and publishing every frame.
    threads.push(
        std::thread::Builder::new()
            .name("gbaroll-playback-drive".to_owned())
            .spawn({
                let boot = boot.clone();
                let inputs = inputs.clone();
                let playback = playback.clone();
                let shared = shared.clone();
                let snapshots = snapshots.clone();
                let rewind = rewind.clone();
                let replay_audio = replay_audio.clone();
                move || {
                    run_drive(
                        boot,
                        inputs,
                        playback,
                        shared,
                        snapshots,
                        rewind,
                        replay_audio,
                    )
                }
            })?,
    );

    // The prefetch worker: races its own link through the whole stream
    // for keyframes.
    threads.push(
        std::thread::Builder::new()
            .name("gbaroll-playback-prefetch".to_owned())
            .spawn({
                let boot = boot.clone();
                let inputs = inputs.clone();
                let snapshots = snapshots.clone();
                let prefetch_progress = prefetch_progress.clone();
                let prefetch_cancel = prefetch_cancel.clone();
                move || {
                    if let Err(e) = engine::run_prefetch(
                        &boot,
                        &inputs,
                        snapshots,
                        prefetch_progress,
                        prefetch_cancel,
                    ) {
                        log::error!("replay prefetch worker exited with error: {e:?}");
                    }
                }
            })?,
    );

    // The seek worker: chases seek targets on the playback link.
    threads.push(
        std::thread::Builder::new()
            .name("gbaroll-playback-seek".to_owned())
            .spawn({
                let seek = seek.clone();
                let playback = playback.clone();
                let snapshots = snapshots.clone();
                let rewind = rewind.clone();
                let shared = shared.clone();
                let replay_audio = replay_audio.clone();
                move || {
                    engine::run_seek_worker(
                        &seek,
                        &playback,
                        &snapshots,
                        &rewind,
                        engine::SeekCallbacks {
                            on_progress: &mut |tick| shared.position.store(tick, Ordering::Relaxed),
                            publish_landing: &mut |snap| publish_snapshot(&shared, snap),
                            on_resume: &mut || shared.resume(),
                            on_audio_reset: &mut || {
                                replay_audio.reset();
                            },
                        },
                    );
                }
            })?,
    );

    Ok(SessionRuntime {
        shared,
        descriptor,
        link: None,
        playback: Some(PlaybackHandles {
            seek: seek.clone(),
            snapshots,
            rewind,
            prefetch_progress,
        }),
        _audio: audio,
        pre_join: Some(Box::new(move || {
            prefetch_cancel.store(true, Ordering::Relaxed);
            seek.shutdown();
        })),
        threads,
    })
}

/// Blit a captured snapshot's framebuffer for the viewed player into
/// the display surface (emulation-free).
pub fn publish_snapshot(shared: &SharedSession, snap: &Snapshot) {
    let view = shared
        .view_player
        .load(Ordering::Relaxed)
        .min(snap.framebuffers.len() - 1);
    let fb = &snap.framebuffers[view];
    if !fb.is_empty() {
        shared.publish_video(fb);
    }
}

fn run_drive(
    boot: BootConfig,
    inputs: Arc<Vec<Vec<u32>>>,
    playback: SharedPlayback,
    shared: Arc<SharedSession>,
    snapshots: SnapshotStore,
    rewind: RewindRing,
    replay_audio: crate::platform::audio::ReplayAudioQueue,
) {
    let pb = match Playback::new(&boot, inputs) {
        Ok(pb) => pb,
        Err(e) => {
            shared.finish(SessionEnd::Error(format!("failed to boot link: {e}")));
            return;
        }
    };
    *playback.lock().unwrap() = Some(pb);

    // Show the boot frame while paused-at-start or still spinning up.
    {
        let mut guard = playback.lock().unwrap();
        if let Some(pb) = guard.as_mut() {
            let view = shared.view_player.load(Ordering::Relaxed);
            if let Some(buf) = pb.link_mut().video_buffer(view) {
                shared.publish_video(buf);
            }
        }
    }

    let mut pacer = Pacer::new();
    let mut audio = crate::platform::audio::ReplayAudioProducer::new(replay_audio);
    let mut was_paused = false;
    loop {
        if shared.quit.load(Ordering::Relaxed) {
            break;
        }
        if shared.paused.load(Ordering::Acquire) {
            if !was_paused {
                audio.reset();
                was_paused = true;
            }
            shared.set_fps_target(0.0);
            std::thread::sleep(PAUSED_TICK);
            pacer.reset();
            continue;
        }
        if was_paused {
            audio.reset();
            was_paused = false;
            pacer.reset();
        }
        if shared.take_pace_reset() {
            pacer.reset();
        }

        let speed_percent = shared.speed.load(Ordering::Relaxed).max(25);
        let speed = speed_percent as f32 / 100.0;
        let fps_target = EXPECTED_FPS * speed;
        shared.set_fps_target(fps_target);

        {
            let mut guard = playback.lock().unwrap();
            let Some(pb) = guard.as_mut() else { break };
            if pb.at_end() {
                shared.paused.store(true, Ordering::Relaxed);
                continue;
            }
            pb.step();
            shared.position.store(pb.cursor(), Ordering::Relaxed);

            // Publish first. Snapshot serialization can then use the
            // remainder of this frame's pacing budget without delaying
            // the frame the player is waiting to see.
            let view = shared.view_player.load(Ordering::Relaxed);
            if let Some(buf) = pb.link_mut().video_buffer(view.min(boot.num_players() - 1)) {
                shared.publish_video(buf);
            }

            audio.publish(
                pb.link_mut(),
                view.min(boot.num_players() - 1),
                speed_percent,
                fps_target,
            );

            let tick = pb.cursor();
            let keyframe_needed = snapshots.snapshot_needed(tick);
            let rewind_needed = tick % engine::REWIND_CAPTURE_INTERVAL == 0;
            if keyframe_needed || rewind_needed {
                match pb.capture() {
                    Ok(snap) => {
                        if keyframe_needed {
                            snapshots.push(snap.clone());
                        }
                        if rewind_needed {
                            rewind.insert(snap);
                        }
                    }
                    Err(e) => log::warn!("replay: frame capture failed: {e:?}"),
                }
            }
        }

        pacer.pace(fps_target);
    }

    shared.finish(SessionEnd::LocalQuit);
}
