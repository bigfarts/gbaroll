//! The single-threaded session host: one `Runtime` in an
//! `Rc<RefCell<_>>`, pumped from three sources that all funnel into
//! [`Runtime::pump`] —
//!
//! - **rAF** while the tab is visible: ticks + audio + present + a
//!   `FRAME_REV` signal bump for the reactive UI;
//! - **the AudioWorklet's queue reports** (~10.7ms cadence): ticks +
//!   audio only. These keep firing when the tab is hidden and rAF
//!   stops, so a netplay session holds full speed in the background;
//! - double-fires are harmless — the accumulator sees ~0 elapsed.
//!
//! Everything here runs on the JS main thread; the atomics/mutexes in
//! `SharedSession` are uncontended. The one real hazard is re-entrant
//! locking (a single thread deadlocks itself), so the pump strictly
//! sequences tick → audio → present as siblings and nothing calls back
//! into the pump from inside a `with_link` scope.

pub mod clock;

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use dioxus::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::platform::audio::web::WebAudio;
use crate::platform::audio::{Binding, LateBinder, LinkStream};
use crate::platform::video::webgl::WebGlPresenter;
use crate::platform::{gamepad, input};
use crate::session::local::{LocalArgs, LocalSession};
use crate::session::SharedSession;

/// Bumped once per pump that changed anything the reactive UI shows
/// (new frame, session end, boot). The canvas is NOT a subscriber —
/// pixels go through WebGL imperatively; this drives the status/menu
/// components.
pub static FRAME_REV: GlobalSignal<u64> = Signal::global(|| 0);

/// Bumped on session start/swap/close — drives structural UI changes.
pub static SESSION_EPOCH: GlobalSignal<u64> = Signal::global(|| 0);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PumpSource {
    Raf,
    Audio,
}

pub struct Runtime {
    session: Option<LocalSession>,
    audio: Option<WebAudio>,
    audio_binder: LateBinder,
    /// RAII: unbinding returns the output to silence.
    audio_binding: Option<Binding>,
    presenter: Option<WebGlPresenter>,
    presented_rev: u64,
    clock: clock::TickClock,
    pub held: input::HeldState,
    pub mapping: input::Mapping,
    /// Set while the pump runs — the keyboard handlers and UI callbacks
    /// re-borrow the Runtime, and anything that could re-enter must not.
    _pumping: bool,
}

thread_local! {
    /// The app-lifetime runtime singleton, reachable from JS callbacks.
    static RUNTIME: RefCell<Option<Rc<RefCell<Runtime>>>> = const { RefCell::new(None) };
}

impl Runtime {
    /// Create the singleton and install its callback sources (rAF loop,
    /// document keyboard listeners). Idempotent per page load.
    pub fn install() -> Rc<RefCell<Runtime>> {
        if let Some(existing) = RUNTIME.with(|r| r.borrow().clone()) {
            return existing;
        }
        let runtime = Rc::new(RefCell::new(Runtime {
            session: None,
            audio: None,
            audio_binder: LateBinder::new(),
            audio_binding: None,
            presenter: None,
            presented_rev: 0,
            clock: clock::TickClock::new(),
            held: input::HeldState::default(),
            mapping: input::Mapping::default(),
            _pumping: false,
        }));
        RUNTIME.with(|r| *r.borrow_mut() = Some(runtime.clone()));
        install_raf(Rc::downgrade(&runtime));
        install_keyboard(Rc::downgrade(&runtime));
        runtime
    }

    /// Attach (or replace) the presenter for the session canvas.
    pub fn attach_canvas(&mut self, canvas: &web_sys::HtmlCanvasElement) {
        match WebGlPresenter::new(canvas) {
            Ok(p) => {
                self.presenter = Some(p);
                // Force a re-upload on the next pump.
                self.presented_rev = 0;
            }
            Err(e) => log::error!("webgl presenter: {e}"),
        }
    }

    #[allow(dead_code)] // session view unmount (M4)
    pub fn detach_canvas(&mut self) {
        self.presenter = None;
    }

    /// Boot a fresh solo session from ROM bytes. The caller must have
    /// ensured the audio sink exists (user-gesture requirement).
    pub fn start_local(&mut self, rom: Vec<u8>, save: Option<Vec<u8>>) -> anyhow::Result<()> {
        self.close_session();
        let rom_crc32 = crc32fast::hash(&rom);
        let rtc = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_millis(js_sys::Date::now() as u64);
        let session = crate::session::local::start(LocalArgs {
            rom,
            rom_crc32,
            save,
            rtc,
        })?;
        let sample_rate = self.audio.as_ref().map(|a| a.sample_rate()).unwrap_or(48_000);
        self.audio_binder.set_sample_rate(sample_rate);
        self.audio_binding = self
            .audio_binder
            .bind(Some(Box::new(LinkStream::new(
                session.link.clone(),
                session.shared.clone(),
                sample_rate,
            ))))
            .ok();
        self.session = Some(session);
        self.clock.reset();
        *SESSION_EPOCH.write() += 1;
        Ok(())
    }

    pub fn close_session(&mut self) {
        self.audio_binding = None;
        if self.session.take().is_some() {
            *SESSION_EPOCH.write() += 1;
        }
    }

    /// Install the audio sink (built asynchronously from a user
    /// gesture; see `WebAudio::create`).
    pub fn set_audio(&mut self, audio: WebAudio) {
        self.audio_binder.set_sample_rate(audio.sample_rate());
        self.audio = Some(audio);
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    pub fn shared(&self) -> Option<&std::sync::Arc<SharedSession>> {
        self.session.as_ref().map(|s| &s.shared)
    }

    pub fn set_volume(&self, v: f32) {
        self.audio_binder.set_volume(v);
    }

    pub fn toggle_pause(&mut self) {
        let Some(shared) = self.shared() else { return };
        if shared.paused.load(std::sync::atomic::Ordering::Acquire) {
            shared.resume();
        } else {
            shared.paused.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// Route one document-level key event. Returns true when the code
    /// is bound (the listener then preventDefaults it).
    pub fn key_event(&mut self, code: &str, pressed: bool) -> bool {
        self.held.set_key(code, pressed);
        self.mapping.binds_code(code)
    }

    /// The single pump every source funnels into.
    pub fn pump(&mut self, source: PumpSource) {
        let now_ms = performance_now();

        // Input: gamepad snapshot + the held keyboard state → joyflags.
        if let Some(session) = &self.session {
            gamepad::poll_into(&mut self.held);
            let joyflags = self.mapping.to_mgba_keys(&self.held);
            session
                .shared
                .joyflags
                .store(joyflags, std::sync::atomic::Ordering::Relaxed);
            // Hold-to-fast-forward.
            let speed = if self.mapping.speed_up_held(&self.held) {
                300
            } else {
                100
            };
            session
                .shared
                .speed
                .store(speed, std::sync::atomic::Ordering::Relaxed);
        }

        // Ticks.
        let mut changed = false;
        if let Some(session) = &mut self.session {
            let shared = session.shared.clone();
            let paused = shared.paused.load(std::sync::atomic::Ordering::Acquire);
            if paused {
                shared.set_fps_target(0.0);
                self.clock.reset();
            } else {
                if shared.take_pace_reset() {
                    self.clock.reset();
                }
                let fps = f32::from_bits(
                    shared
                        .fps_target
                        .load(std::sync::atomic::Ordering::Relaxed),
                )
                .max(crate::session::EXPECTED_FPS * 0.25); // fresh boots start at 0
                for _ in 0..self.clock.due(now_ms, fps) {
                    if !session.driver.tick() {
                        // Ended: drop the session on this pump; the end
                        // reason stays readable via the UI's own copy.
                        let end = shared.end.lock().unwrap().clone();
                        log::info!("session ended: {end:?}");
                        self.close_session();
                        changed = true;
                        break;
                    }
                    changed = true;
                }
            }
        }

        // Audio: top the sink up from whatever is bound (silence when
        // no session). Strictly after ticks — LinkStream locks the link.
        if let (Some(audio), true) = (&mut self.audio, self.session.is_some()) {
            audio.resume_if_suspended();
            audio.pump(&mut self.audio_binder);
        }

        // Present + UI signal: only on the visible-path source.
        if source == PumpSource::Raf {
            if let (Some(presenter), Some(session)) = (&mut self.presenter, &self.session) {
                let rev = session
                    .shared
                    .vbuf_rev
                    .load(std::sync::atomic::Ordering::Acquire);
                if rev != self.presented_rev {
                    self.presented_rev = rev;
                    let vbuf = session.shared.vbuf.lock().unwrap();
                    presenter.present(&vbuf);
                }
            }
            if changed {
                *FRAME_REV.write() += 1;
            }
        }
    }
}

fn performance_now() -> f64 {
    web_sys::window().unwrap().performance().unwrap().now()
}

/// The worklet's queue-report hook: pump with the Audio source. Wired
/// by `WebAudio::create` via this free function so the closure only
/// holds a weak handle.
pub fn pump_from_audio_report() {
    if let Some(runtime) = RUNTIME.with(|r| r.borrow().clone()) {
        if let Ok(mut rt) = runtime.try_borrow_mut() {
            rt.pump(PumpSource::Audio);
        }
    }
}

fn install_raf(runtime: Weak<RefCell<Runtime>>) {
    let handle: Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>> = Rc::new(RefCell::new(None));
    let handle2 = handle.clone();
    *handle.borrow_mut() = Some(Closure::new(move |_now: f64| {
        if let Some(rt) = runtime.upgrade() {
            rt.borrow_mut().pump(PumpSource::Raf);
            request_animation_frame(handle2.borrow().as_ref().unwrap());
        }
    }));
    request_animation_frame(handle.borrow().as_ref().unwrap());
}

fn request_animation_frame(closure: &Closure<dyn FnMut(f64)>) {
    web_sys::window()
        .unwrap()
        .request_animation_frame(closure.as_ref().unchecked_ref())
        .expect("requestAnimationFrame");
}

fn install_keyboard(runtime: Weak<RefCell<Runtime>>) {
    let document = web_sys::window().unwrap().document().unwrap();
    for (event, pressed) in [("keydown", true), ("keyup", false)] {
        let runtime = runtime.clone();
        let closure: Closure<dyn FnMut(web_sys::KeyboardEvent)> =
            Closure::new(move |e: web_sys::KeyboardEvent| {
                // Text inputs keep their keys.
                if let Some(target) = e.target() {
                    if let Some(el) = target.dyn_ref::<web_sys::Element>() {
                        let tag = el.tag_name();
                        if tag == "INPUT" || tag == "TEXTAREA" || tag == "SELECT" {
                            return;
                        }
                    }
                }
                let Some(rt) = runtime.upgrade() else { return };
                let Ok(mut rt) = rt.try_borrow_mut() else { return };
                if rt.key_event(&e.code(), pressed) {
                    // Bound key: don't let arrows/space scroll the page.
                    e.prevent_default();
                }
            });
        document
            .add_event_listener_with_callback(event, closure.as_ref().unchecked_ref())
            .expect("addEventListener");
        // App-lifetime listeners: leak deliberately.
        closure.forget();
    }
}
