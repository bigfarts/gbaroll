//! The fullscreen session view: the framebuffer canvas, a compact
//! header, the Escape-toggled menu overlay, and the end-of-session
//! overlay (the session itself is already torn down by then; the
//! runtime keeps the end readable until it's dismissed).

use std::sync::atomic::Ordering;

use dioxus::prelude::*;
use wasm_bindgen::JsCast;

use super::{cable, icons, use_ctx, Ctx};
use crate::platform::input::{self, MappedKey};
use crate::runtime::{FRAME_REV, MENU_OPEN, SESSION_EPOCH};
use crate::session::{SessionEnd, SessionKind};

#[component]
pub fn SessionView() -> Element {
    let Ctx {
        runtime,
        mut config,
        library,
        ..
    } = use_ctx();

    // Attach the presenter once the canvas exists; detach on unmount so
    // the next mount starts from a fresh WebGL context.
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
    {
        let runtime = runtime.clone();
        use_drop(move || runtime.borrow_mut().detach_canvas());
    }

    // Reactive inputs: per-frame stats, structural session changes, and
    // the Escape-toggled menu.
    let _ = FRAME_REV.read();
    let _ = SESSION_EPOCH.read();
    let menu_open = *MENU_OPEN.read();

    let (title, running, paused, status, end, caption) = {
        let lib = library.read();
        let rt = runtime.borrow();
        let title = rt
            .descriptor()
            .and_then(|d| d.rom_crc32)
            .and_then(|crc| {
                lib.as_ref()
                    .and_then(|v| v.as_ref())
                    .and_then(|(lib, _)| lib.by_crc32(crc))
            })
            .map(|rom| rom.display_name().to_string())
            .unwrap_or_else(|| "Session".to_string());
        let caption = match rt.descriptor().map(|d| d.kind) {
            Some(SessionKind::Netplay) => "Netplay",
            _ => "Playing solo",
        };
        let end = rt.last_end();
        match rt.shared() {
            Some(shared) => {
                let stats = shared.stats.lock().unwrap();
                let paused = shared.paused.load(Ordering::Relaxed);
                let status = if paused {
                    String::new()
                } else {
                    format!("frame {} · {:.1} fps", stats.frontier, stats.fps_target)
                };
                (title, true, paused, status, end, caption)
            }
            None => (title, false, false, String::new(), end, caption),
        }
    };

    let volume_pct = (config.read().volume * 100.0).round() as u32;
    let hints = {
        let cfg = config.read();
        let mut hints = vec!["Esc — menu".to_string()];
        if let Some(physical) = cfg.mapping.slot(MappedKey::SpeedUp).first() {
            let (_, label) = input::describe(physical);
            hints.push(format!("hold {label} — fast-forward"));
        }
        hints.join("  ·  ")
    };

    rsx! {
        document::Title { "{title} — gbaroll" }
        div { class: "session",
            header { class: "session-header",
                if running {
                    button {
                        class: "btn ghost",
                        onclick: move |_| {
                            let open = *MENU_OPEN.peek();
                            *MENU_OPEN.write() = !open;
                        },
                        icons::Sliders {}
                        "Menu"
                        kbd { "esc" }
                    }
                }
                div { class: "session-title",
                    span { class: "game", "{title}" }
                    span { class: "sub", "{caption}" }
                }
                div { class: "session-status",
                    if paused {
                        span { class: "badge", "Paused" }
                    } else if !status.is_empty() {
                        span { class: "sub", "{status}" }
                    }
                    if running {
                        button {
                            class: "btn ghost icon-btn",
                            title: if paused { "Resume" } else { "Pause" },
                            onclick: {
                                let runtime = runtime.clone();
                                move |_| runtime.borrow_mut().toggle_pause()
                            },
                            if paused {
                                icons::Play {}
                            } else {
                                icons::Pause {}
                            }
                        }
                    }
                }
            }
            div { class: "stage",
                canvas {
                    id: "framebuffer",
                    width: "720",
                    height: "480",
                    class: if !config.read().integer_scaling { "fit" },
                }
            }
            if let Some(end) = end {
                div { class: "overlay",
                    div { class: "overlay-panel",
                        p { class: "end-message", {end_message(&end)} }
                        button {
                            class: "btn primary",
                            onclick: {
                                let runtime = runtime.clone();
                                move |_| runtime.borrow_mut().dismiss_end()
                            },
                            "Back"
                        }
                    }
                }
            } else if menu_open {
                div { class: "overlay",
                    div { class: "overlay-panel",
                        div { class: "overlay-head",
                            h2 { "{title}" }
                            p { class: "sub", "{caption}" }
                        }
                        cable::CablePanel {}
                        div { class: "menu-volume",
                            label { "Volume · {volume_pct}%" }
                            input {
                                r#type: "range",
                                min: "0",
                                max: "100",
                                value: "{volume_pct}",
                                oninput: move |evt: FormEvent| {
                                    if let Ok(v) = evt.value().parse::<f32>() {
                                        config.with_mut(|c| c.volume = (v / 100.0).clamp(0.0, 1.0));
                                    }
                                },
                            }
                        }
                        div { class: "menu-actions",
                            button {
                                class: "btn primary",
                                onclick: move |_| *MENU_OPEN.write() = false,
                                "Back to game"
                            }
                            button {
                                class: "btn danger",
                                onclick: {
                                    let runtime = runtime.clone();
                                    move |_| {
                                        // A lobby can't outlive the session
                                        // it would plug into.
                                        cable::leave();
                                        runtime.borrow_mut().close_session()
                                    }
                                },
                                "Quit game"
                            }
                        }
                        p { class: "hint", "{hints}" }
                    }
                }
            }
        }
    }
}

/// The end overlay's one-liner. Netplay's per-player variants name
/// players by index until the roster port lands (M5).
fn end_message(end: &SessionEnd) -> String {
    let player = |p: &usize| format!("player {}", p + 1);
    match end {
        SessionEnd::LocalQuit => "Session ended.".to_string(),
        SessionEnd::Unplugged => "Unplugged.".to_string(),
        SessionEnd::PeerQuit { player: p } => format!("{} left the session.", player(p)),
        SessionEnd::PeerDisconnected { player: p } => {
            format!("Connection to {} lost.", player(p))
        }
        SessionEnd::Desync { tick } => {
            format!("Desync detected at tick {tick} — session aborted.")
        }
        SessionEnd::Error(e) => format!("Session error: {e}"),
    }
}
