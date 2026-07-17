//! The fullscreen session view: the framebuffer canvas, a compact
//! header, the Escape-toggled menu overlay, and the end-of-session
//! overlay (the session itself is already torn down by then; the
//! runtime keeps the end readable until it's dismissed).

use std::sync::atomic::Ordering;

use dioxus::prelude::*;
use wasm_bindgen::JsCast;

use super::{cable, icons, overlay, touch, use_ctx, Ctx};
use crate::platform::input::{self, MappedKey};
use crate::runtime::{FRAME_REV, MENU_OPEN, SESSION_EPOCH};
use crate::session::{SessionEnd, SessionKind};

#[component]
pub fn SessionView() -> Element {
    let Ctx {
        runtime,
        config,
        library,
        mut library_rev,
        mut selected_save,
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
        use_drop(move || {
            let mut rt = runtime.borrow_mut();
            rt.detach_canvas();
            // A "(fresh save)" session that persisted SRAM created a
            // real saves/ file — move the picker onto it so the next
            // Play continues it instead of booting fresh again.
            if selected_save.peek().is_none() {
                if let Some(name) = rt.take_persisted_save() {
                    selected_save.set(Some(name));
                }
            }
            drop(rt);
            // Leaving the session lands back on the pickers, and the
            // session's SRAM write-back probably created (or updated)
            // a save — rescan so it shows immediately.
            *library_rev.write() += 1;
        });
    }

    // Reactive inputs: per-frame stats, structural session changes, and
    // the Escape-toggled menu.
    let _ = FRAME_REV.read();
    let _ = SESSION_EPOCH.read();
    let menu_open = *MENU_OPEN.read();

    let (title, running, paused, end) = {
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
        let end = rt.last_end();
        match rt.shared() {
            Some(shared) => {
                let paused = shared.paused.load(Ordering::Relaxed);
                (title, true, paused, end)
            }
            None => (title, false, false, end),
        }
    };

    rsx! {
        document::Title { "{title} — gbaroll" }
        div { class: "session",
            div { class: "stage",
                // Backing store per scaling mode: native 240x160 for
                // integer mode (pixelated CSS upscale stays square), a
                // 6x nearest-neighbour render for fit mode (the browser
                // then bilinears it to the window — sharp, no shimmer).
                canvas {
                    id: "framebuffer",
                    width: if config.read().integer_scaling { "240" } else { "1440" },
                    height: if config.read().integer_scaling { "160" } else { "960" },
                    class: if !config.read().integer_scaling { "fit" },
                }
                // The chip row (and whichever card is dropped from it —
                // menu or cable) keeps its spot in every state; only the
                // end overlay is modal.
                if end.is_none() && running {
                    overlay::SessionOverlay {}
                    // Coarse-pointer screens get on-screen controls (CSS
                    // decides; it renders inert elsewhere). They stay put
                    // under an open card — the backdrop (z 7) covers them,
                    // so a touch there dismisses the card instead.
                    touch::TouchControls {}
                }
                // Pause only happens transiently (the lobby freezes the
                // machine for its capture); the badge says why the game
                // stopped moving.
                if paused && !menu_open && end.is_none() {
                    span { class: "badge pause-badge", "Paused" }
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
            }
        }
    }
}

/// The session menu as a card dropped from the chip row, mirroring the
/// cable panel's layout. Rendered by `telemetry::CableOverlay` so both
/// cards share one anchor.
#[component]
pub fn SessionMenuCard() -> Element {
    let Ctx {
        runtime,
        mut config,
        library,
        ..
    } = use_ctx();

    let (title, caption) = {
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
        (title, caption)
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
        div { class: "tele-card",
            div { class: "tele-head",
                div {
                    h3 { "{title}" }
                    p { class: "sub", "{caption}" }
                }
                button {
                    class: "btn ghost icon-btn",
                    onclick: move |_| *MENU_OPEN.write() = false,
                    icons::ChevronUp {}
                }
            }
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
                            // A lobby can't outlive the session it would
                            // plug into.
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
