//! A local link session: 1–4 cores on one machine, no rollback engine —
//! the joypad drives whichever player is currently controlled and the
//! rest idle. One core is the solo mode netplay plugs into and unplugs
//! from; 2–4 are a whole link on one machine, useful for poking at a
//! game's link mode (and for testing the whole client without a peer).

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use mgba_siolink::{BootSide, Link, LinkOptions, SideOptions};

use crate::session::{
    apply_view_frameskip, prepare_audio_buffers, Handoff, LinkAccess, Pacer, SessionDescriptor, SessionEnd,
    SessionKind, SessionRuntime, SharedSession, EXPECTED_FPS, PAUSED_TICK,
};

pub struct LocalArgs {
    /// Per player, the ROM their side boots.
    pub roms: Vec<Vec<u8>>,
    /// CRC32 of the controlled player's ROM (for the session descriptor).
    pub rom_crc32: u32,
    /// Save applied to every side (each GBA has its own identical cart).
    pub save: Option<Vec<u8>>,
}

/// Boot a fresh local session from power-on.
pub fn start(
    args: LocalArgs,
    audio_binder: &crate::platform::audio::LateBinder,
    frame_notify: Arc<tokio::sync::Notify>,
) -> anyhow::Result<SessionRuntime> {
    let link = Link::with_options(LinkOptions {
        sides: args
            .roms
            .iter()
            .map(|rom| SideOptions {
                rom: rom.clone(),
                save: args.save.clone(),
            })
            .collect(),
        rtc: Some(std::time::SystemTime::now()),
    })?;
    run(link, args.rom_crc32, audio_binder, frame_notify)
}

/// Continue a solo machine from a netplay teardown: the cable was
/// unplugged, the game keeps running from exactly where the link left it.
pub fn resume(
    handoff: Handoff,
    rom_crc32: u32,
    audio_binder: &crate::platform::audio::LateBinder,
    frame_notify: Arc<tokio::sync::Notify>,
) -> anyhow::Result<SessionRuntime> {
    let link = Link::from_states(
        vec![BootSide {
            rom: handoff.rom,
            save: handoff.save,
            state: handoff.state,
        }],
        Some(std::time::UNIX_EPOCH + std::time::Duration::from_micros(handoff.rtc_unix_micros)),
    )?;
    run(link, rom_crc32, audio_binder, frame_notify)
}

fn run(
    mut link: Link,
    rom_crc32: u32,
    audio_binder: &crate::platform::audio::LateBinder,
    frame_notify: Arc<tokio::sync::Notify>,
) -> anyhow::Result<SessionRuntime> {
    let num_players = link.num_players();
    prepare_audio_buffers(&mut link);
    apply_view_frameskip(&mut link, 0);
    let link = Arc::new(Mutex::new(link));

    let shared = SharedSession::new(0, frame_notify);
    let descriptor = SessionDescriptor {
        kind: SessionKind::Local,
        num_players,
        local_player: 0,
        nicks: (0..num_players).map(|i| format!("Player {}", i + 1)).collect(),
        room_code: None,
        replay_path: None,
        rom_crc32: Some(rom_crc32),
    };

    let audio = audio_binder
        .bind(Some(Box::new(crate::platform::audio::LinkStream::new(
            LinkAccess::Shared(link.clone()),
            shared.clone(),
            audio_binder.sample_rate(),
        ))))
        .ok();

    let drive = {
        let shared = shared.clone();
        let link = link.clone();
        std::thread::Builder::new()
            .name("gbaroll-local-drive".to_owned())
            .spawn(move || drive(shared, link, num_players))?
    };

    Ok(SessionRuntime {
        shared,
        descriptor,
        link: Some(LinkAccess::Shared(link)),
        playback: None,
        _audio: audio,
        pre_join: None,
        threads: vec![drive],
    })
}

fn drive(shared: Arc<SharedSession>, link: Arc<Mutex<Link>>, num_players: usize) {
    let mut pacer = Pacer::new();
    let mut last_view = 0usize;

    loop {
        if shared.quit.load(Ordering::Relaxed) {
            break;
        }
        if shared.paused.load(Ordering::Relaxed) {
            shared.set_fps_target(0.0);
            std::thread::sleep(PAUSED_TICK);
            pacer.reset();
            continue;
        }

        // The viewed player is also the controlled one.
        let view = shared.view_player.load(Ordering::Relaxed).min(num_players - 1);
        let joyflags = shared.joyflags.load(Ordering::Relaxed) & 0x3ff;
        let mut keys = vec![0u32; num_players];
        keys[view] = joyflags;

        {
            let mut link = link.lock().unwrap();
            if view != last_view {
                apply_view_frameskip(&mut link, view);
                last_view = view;
            }
            link.tick(&keys);
            if let Some(buf) = link.video_buffer(view) {
                shared.publish_video(buf);
            }
        }

        // Hold-to-fast-forward comes in via the speed knob.
        let speed = shared.speed.load(Ordering::Relaxed).max(25) as f32 / 100.0;
        let fps_target = EXPECTED_FPS * speed;
        shared.set_fps_target(fps_target);
        {
            let mut stats = shared.stats.lock().unwrap();
            stats.fps_target = fps_target;
            stats.frontier += 1;
        }
        pacer.pace(fps_target);
    }

    shared.finish(SessionEnd::LocalQuit);
}
