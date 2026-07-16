//! The browser client's UI shell. M2 scope: a debug-grade screen that
//! can boot a ROM (file picker or the built-in SIO test ROM), present
//! it on the WebGL canvas, and exercise audio, keyboard, gamepad,
//! pause, and volume. The real component tree lands with M4.

use dioxus::prelude::*;
use wasm_bindgen::JsCast;

use crate::runtime::{Runtime, FRAME_REV, SESSION_EPOCH};

const WORKLET_JS: Asset = asset!("/assets/audio-worklet.js");

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

/// Ensure the audio sink exists (must run within a user gesture), then
/// boot the ROM.
async fn boot(runtime: std::rc::Rc<std::cell::RefCell<Runtime>>, rom: Vec<u8>) {
    if !runtime.borrow().has_audio() {
        match crate::platform::audio::web::WebAudio::create(&WORKLET_JS.to_string(), || {
            crate::runtime::pump_from_audio_report();
        })
        .await
        {
            Ok(audio) => runtime.borrow_mut().set_audio(audio),
            Err(e) => log::error!("audio unavailable: {e:?}"),
        }
    }
    if let Err(e) = runtime.borrow_mut().start_local(rom, None) {
        log::error!("couldn't start session: {e:#}");
    }
}

fn app() -> Element {
    let runtime = use_hook(Runtime::install);
    let mut rom_bytes = use_signal(|| Option::<Vec<u8>>::None);
    let mut rom_name = use_signal(String::new);

    // Attach the presenter once the canvas exists.
    {
        let runtime = runtime.clone();
        use_effect(move || {
            let canvas = web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.get_element_by_id("framebuffer"))
                .and_then(|el| el.dyn_into::<web_sys::HtmlCanvasElement>().ok());
            match canvas {
                Some(canvas) => runtime.borrow_mut().attach_canvas(&canvas),
                None => log::error!("canvas missing"),
            }
        });
    }

    // Status line, re-rendered per presented frame.
    let status = {
        let runtime = runtime.clone();
        move || {
            let _ = FRAME_REV.read();
            let _ = SESSION_EPOCH.read();
            let rt = runtime.borrow();
            match rt.shared() {
                Some(shared) => {
                    let stats = shared.stats.lock().unwrap();
                    let paused = shared.paused.load(std::sync::atomic::Ordering::Relaxed);
                    if paused {
                        "paused".to_string()
                    } else {
                        format!(
                            "running · frame {} · target {:.1} fps",
                            stats.frontier, stats.fps_target
                        )
                    }
                }
                None => "no session".to_string(),
            }
        }
    };

    rsx! {
        h1 { "gbaroll" }
        p {
            "Pick a .gba ROM (stays in this tab — nothing uploads), or boot the built-in test ROM. "
            "Arrows move · Z/X = A/B · A/S = L/R · Enter/Space = Start/Select · hold LShift = fast-forward."
        }
        div {
            input {
                r#type: "file",
                accept: ".gba,.agb,.srl",
                onchange: move |evt| {
                    async move {
                        if let Some(file) = evt.files().into_iter().next() {
                            match file.read_bytes().await {
                                Ok(bytes) => {
                                    let bytes = bytes.to_vec();
                                    log::info!("loaded {} ({} bytes)", file.name(), bytes.len());
                                    rom_name.set(file.name());
                                    rom_bytes.set(Some(bytes));
                                }
                                Err(e) => log::error!("couldn't read {}: {e:?}", file.name()),
                            }
                        }
                    }
                },
            }
            button {
                disabled: rom_bytes.read().is_none(),
                onclick: {
                    let runtime = runtime.clone();
                    move |_| {
                        if let Some(rom) = rom_bytes.read().clone() {
                            let runtime = runtime.clone();
                            spawn(async move { boot(runtime, rom).await; });
                        }
                    }
                },
                "Play {rom_name}"
            }
            button {
                onclick: {
                    let runtime = runtime.clone();
                    move |_| {
                        let runtime = runtime.clone();
                        spawn(async move { boot(runtime, mgba_siolink::testrom::build()).await; });
                    }
                },
                "Boot test ROM"
            }
            button {
                onclick: {
                    let runtime = runtime.clone();
                    move |_| runtime.borrow_mut().toggle_pause()
                },
                "Pause/Resume"
            }
            label {
                "Volume "
                input {
                    r#type: "range",
                    min: "0",
                    max: "100",
                    value: "100",
                    oninput: {
                        let runtime = runtime.clone();
                        move |evt: FormEvent| {
                            if let Ok(v) = evt.value().parse::<f32>() {
                                runtime.borrow().set_volume(v / 100.0);
                            }
                        }
                    },
                }
            }
        }
        p { {status()} }
        canvas { id: "framebuffer", width: "720", height: "480" }
    }
}
