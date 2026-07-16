//! A solo GBA session with no rollback engine. This is the machine that
//! netplay plugs into and resumes after the virtual cable is unplugged.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use mgba_siolink::{BootSide, Link, LinkOptions, SideOptions};

use crate::session::{
    prepare_audio_buffers, Handoff, LinkAccess, Pacer, SessionDescriptor, SessionEnd, SessionKind,
    SessionRuntime, SharedSession, EXPECTED_FPS, PAUSED_TICK,
};

pub struct LocalArgs {
    pub rom: Vec<u8>,
    /// CRC32 of the ROM (for the session descriptor).
    pub rom_crc32: u32,
    /// Save data for the solo cart.
    pub save: Option<Vec<u8>>,
}

/// Boot a fresh local session from power-on.
pub fn start(
    args: LocalArgs,
    audio_binder: &crate::platform::audio::LateBinder,
    frame_notify: Arc<tokio::sync::Notify>,
) -> anyhow::Result<SessionRuntime> {
    let link = Link::with_options(LinkOptions {
        sides: vec![SideOptions {
            rom: args.rom,
            save: args.save,
        }],
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
    prepare_audio_buffers(&mut link);
    let link = Arc::new(Mutex::new(link));

    let shared = SharedSession::new(0, frame_notify);
    let descriptor = SessionDescriptor {
        kind: SessionKind::Local,
        num_players: 1,
        local_player: 0,
        nicks: vec!["Player 1".to_string()],
        room_code: None,
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
            .spawn(move || drive(shared, link))?
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

/// The solo session's per-tick body, extracted from the drive loop so a
/// host without threads (the wasm main loop) can call it directly. The
/// caller owns pacing and the pause gate; `tick` assumes it is only
/// called when the session should actually advance one frame.
pub struct LocalDriver {
    shared: Arc<SharedSession>,
    link: Arc<Mutex<Link>>,
}

impl LocalDriver {
    pub fn new(shared: Arc<SharedSession>, link: Arc<Mutex<Link>>) -> LocalDriver {
        LocalDriver { shared, link }
    }

    /// Advance one frame. Returns `false` once the session has ended
    /// (the end is already recorded in `shared`).
    pub fn tick(&mut self) -> bool {
        if self.shared.quit.load(Ordering::Relaxed) {
            self.shared.finish(SessionEnd::LocalQuit);
            return false;
        }

        let joyflags = self.shared.joyflags.load(Ordering::Relaxed) & 0x3ff;

        {
            let mut link = self.link.lock().unwrap();
            link.tick(&[joyflags]);
            if let Some(buf) = link.video_buffer(0) {
                self.shared.publish_video(buf);
            }
        }

        // Hold-to-fast-forward comes in via the speed knob.
        let speed = self.shared.speed.load(Ordering::Relaxed).max(25) as f32 / 100.0;
        let fps_target = EXPECTED_FPS * speed;
        self.shared.set_fps_target(fps_target);
        {
            let mut stats = self.shared.stats.lock().unwrap();
            stats.fps_target = fps_target;
            stats.frontier += 1;
        }
        true
    }
}

fn drive(shared: Arc<SharedSession>, link: Arc<Mutex<Link>>) {
    let mut driver = LocalDriver::new(shared.clone(), link);
    let mut pacer = Pacer::new();

    loop {
        if shared.paused.load(Ordering::Acquire) {
            // Quit must still be honored while paused.
            if shared.quit.load(Ordering::Relaxed) {
                shared.finish(SessionEnd::LocalQuit);
                break;
            }
            shared.set_fps_target(0.0);
            std::thread::sleep(PAUSED_TICK);
            pacer.reset();
            continue;
        }
        if shared.take_pace_reset() {
            pacer.reset();
        }
        if !driver.tick() {
            break;
        }
        pacer.pace(f32::from_bits(shared.fps_target.load(Ordering::Relaxed)));
    }
}
