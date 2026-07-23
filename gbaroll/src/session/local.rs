//! A solo GBA session with no rollback engine. This is the machine that
//! netplay plugs into and resumes after the virtual cable is unplugged.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use mgba_rollback::{BootSide, Link, LinkOptions, SideOptions};

use crate::session::{
    prepare_audio_buffers, Handoff, LinkAccess, LinkKind, SessionDescriptor, SessionEnd,
    SessionKind, SharedSession, EXPECTED_FPS,
};

pub struct LocalArgs {
    pub rom: Vec<u8>,
    /// CRC32 of the ROM (for the session descriptor).
    pub rom_crc32: u32,
    /// Save data for the solo cart.
    pub save: Option<Vec<u8>>,
    /// The cart clock's boot value. `SystemTime::now()` panics on wasm,
    /// so the host supplies it (from `js_sys::Date::now()`).
    pub rtc: std::time::SystemTime,
    /// What's on the machine's link port. A wireless machine gets its
    /// adapter from power-on, so the game's wireless menus work solo.
    pub link: LinkKind,
}

/// A booted local session: the driver the runtime pump ticks, plus the
/// shared state and link access the presenter/audio/UI hang off.
pub struct LocalSession {
    pub driver: LocalDriver,
    pub shared: Arc<SharedSession>,
    pub link: LinkAccess,
    pub descriptor: SessionDescriptor,
}

/// Boot a fresh local session from power-on.
pub fn start(args: LocalArgs) -> anyhow::Result<LocalSession> {
    let link = Link::with_options(LinkOptions {
        sides: vec![SideOptions {
            rom: args.rom,
            save: args.save,
        }],
        rtc: Some(args.rtc),
        peripheral: args.link.peripheral(),
    })?;
    Ok(build(link, args.rom_crc32, args.link))
}

/// Continue a solo machine from a netplay teardown: the cable was
/// unplugged, the game keeps running from exactly where the link left it.
#[allow(dead_code)] // the unplug-continue path returns with netplay (M5)
pub fn resume(handoff: Handoff, rom_crc32: u32) -> anyhow::Result<LocalSession> {
    let link = Link::from_states(
        vec![BootSide {
            rom: handoff.rom,
            save: handoff.save,
            state: handoff.state,
            adapter: handoff.adapter,
        }],
        Some(std::time::UNIX_EPOCH + std::time::Duration::from_micros(handoff.rtc_unix_micros)),
        handoff.link.peripheral(),
    )?;
    Ok(build(link, rom_crc32, handoff.link))
}

fn build(mut link: Link, rom_crc32: u32, link_kind: LinkKind) -> LocalSession {
    prepare_audio_buffers(&mut link);
    let link = Arc::new(Mutex::new(link));

    let shared = SharedSession::new(0);
    let descriptor = SessionDescriptor {
        kind: SessionKind::Local,
        local_player: 0,
        nicks: vec!["Player 1".to_string()],
        room_code: None,
        rom_crc32: Some(rom_crc32),
        link: link_kind,
    };

    LocalSession {
        driver: LocalDriver::new(shared.clone(), link.clone()),
        shared,
        link: LinkAccess::Shared(link),
        descriptor,
    }
}

/// The solo session's per-tick body. The pump owns pacing and the pause
/// gate; `tick` assumes it is only called when the session should
/// actually advance one frame.
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
            // A corrupt link state must end the session with a message,
            // not panic the app into a frozen tab.
            if let Err(e) = link.try_tick(&[joyflags]) {
                drop(link);
                self.shared.finish(SessionEnd::Error(format!("emulation error: {e}")));
                return false;
            }
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
