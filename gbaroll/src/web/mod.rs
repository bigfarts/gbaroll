//! The browser client. Currently the M1 spike: boot the built-in SIO
//! test ROM through `mgba_siolink::Link`, drive it from a
//! requestAnimationFrame accumulator, and present through the WebGL2
//! framebuffer pipeline — proving the C core, the rAF loop shape, and
//! the render path end to end.

mod webgl;

use std::cell::RefCell;
use std::rc::Rc;

use dioxus::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

/// GBA cycles per second / cycles per frame — the exact tick rate.
const EXPECTED_FPS: f64 = 16777216.0 / 280896.0;

/// Bounds worst-case callback time when catching up after a stall.
const MAX_TICKS_PER_PUMP: u32 = 6;

/// The determinism gate: log a digest of the frame at this tick for
/// comparison against the native build.
const DIGEST_TICK: u64 = 600;

/// The C shim's clock (mgba's `gettimeofday` for savestate stamps).
#[no_mangle]
pub extern "C" fn gbaroll_now_unix_ms() -> f64 {
    js_sys::Date::now()
}

pub fn main() {
    console_error_panic_hook::set_once();
    let _ = console_log::init_with_level(log::Level::Info);
    mgba::log::install_default_logger();
    dioxus::launch(app);
}

fn app() -> Element {
    use_effect(|| {
        if let Err(e) = start_spike() {
            log::error!("spike failed to start: {e}");
        }
    });
    rsx! {
        h1 { "gbaroll — wasm spike" }
        p { "SIO test ROM on the mgba core, rAF-driven, WebGL2-presented." }
        canvas { id: "framebuffer", width: "720", height: "480" }
    }
}

struct Spike {
    link: mgba_siolink::Link,
    presenter: webgl::WebGlPresenter,
    /// rAF accumulator: fractional ticks owed.
    last_ms: Option<f64>,
    owed: f64,
    ticks: u64,
    digest_logged: bool,
    /// Worst tick-batch duration in the current one-second window.
    batch_max_ms: f64,
    window_start_ms: f64,
}

impl Spike {
    fn pump(&mut self, now_ms: f64) {
        let Some(last) = self.last_ms.replace(now_ms) else {
            return;
        };
        let mut dt = (now_ms - last) / 1000.0;
        if dt > 0.25 {
            // Stall (hidden tab, long GC): resync, don't sprint.
            self.owed = 0.0;
            dt = 1.0 / 60.0;
        }
        self.owed += dt * EXPECTED_FPS;
        let due = (self.owed.floor() as u32).min(MAX_TICKS_PER_PUMP);
        self.owed = (self.owed - due as f64).min(1.0);

        let batch_start = performance_now();
        for _ in 0..due {
            self.link.tick(&[0]);
            self.ticks += 1;
            if self.ticks == DIGEST_TICK && !self.digest_logged {
                self.digest_logged = true;
                if let Some(buf) = self.link.video_buffer(0) {
                    log::info!(
                        "determinism gate: video crc32 {:08x} at tick {}",
                        crc32fast::hash(buf),
                        self.ticks
                    );
                }
            }
        }
        let batch_ms = performance_now() - batch_start;

        if let Some(buf) = self.link.video_buffer(0) {
            self.presenter.present(buf);
        }

        // Once a second, report the worst tick-batch time — the frame
        // budget headroom measurement.
        self.batch_max_ms = self.batch_max_ms.max(batch_ms);
        if now_ms - self.window_start_ms >= 1000.0 {
            log::info!(
                "tick {}: worst batch {:.2}ms (of 16.7ms budget)",
                self.ticks,
                self.batch_max_ms
            );
            self.batch_max_ms = 0.0;
            self.window_start_ms = now_ms;
        }
    }
}

fn performance_now() -> f64 {
    web_sys::window().unwrap().performance().unwrap().now()
}

fn start_spike() -> Result<(), String> {
    let document = web_sys::window()
        .ok_or("no window")?
        .document()
        .ok_or("no document")?;
    let canvas: web_sys::HtmlCanvasElement = document
        .get_element_by_id("framebuffer")
        .ok_or("canvas missing")?
        .dyn_into()
        .map_err(|_| "not a canvas")?;
    let presenter = webgl::WebGlPresenter::new(&canvas)?;

    let rom = mgba_siolink::testrom::build();
    // Fixed RTC (not wall clock) so the determinism-gate digest is
    // directly comparable against a native run of the same boot.
    let rtc = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    let link = mgba_siolink::Link::with_options(mgba_siolink::LinkOptions {
        sides: vec![mgba_siolink::SideOptions { rom, save: None }],
        rtc: Some(rtc),
    })
    .map_err(|e| format!("link boot: {e}"))?;
    log::info!("link booted");

    let spike = Rc::new(RefCell::new(Spike {
        link,
        presenter,
        last_ms: None,
        owed: 0.0,
        ticks: 0,
        digest_logged: false,
        batch_max_ms: 0.0,
        window_start_ms: performance_now(),
    }));

    // The self-rescheduling rAF closure.
    let handle: Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>> = Rc::new(RefCell::new(None));
    let handle2 = handle.clone();
    *handle.borrow_mut() = Some(Closure::new(move |now_ms: f64| {
        spike.borrow_mut().pump(now_ms);
        request_animation_frame(handle2.borrow().as_ref().unwrap());
    }));
    request_animation_frame(handle.borrow().as_ref().unwrap());
    Ok(())
}

fn request_animation_frame(closure: &Closure<dyn FnMut(f64)>) {
    web_sys::window()
        .unwrap()
        .request_animation_frame(closure.as_ref().unchecked_ref())
        .expect("requestAnimationFrame");
}
