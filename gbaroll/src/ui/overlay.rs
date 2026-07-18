//! The session overlay: the top-center chip row (menu + link cable)
//! and whichever card is dropped from it. The cards themselves live
//! with their owners — the menu with `session_view`, the lobby with
//! `cable`, the connected stats with `telemetry` — this module only
//! anchors the chips and dispatches to one card at a time.

use dioxus::prelude::*;

use super::{cable, icons, session_view, telemetry, use_ctx};
use crate::runtime::{FRAME_REV, MENU_OPEN, PANEL_OPEN, SESSION_EPOCH};
use crate::session::SessionKind;

#[component]
pub fn SessionOverlay() -> Element {
    let ctx = use_ctx();
    let _ = SESSION_EPOCH.read();
    let _ = FRAME_REV.read();
    let expanded = *PANEL_OPEN.read();
    let menu_open = *MENU_OPEN.read();

    // The link chip's face: the link-quality glyph while connected,
    // else the session's peripheral.
    let (is_netplay, skew, link_kind) = {
        let rt = ctx.runtime.borrow();
        let link_kind = rt.descriptor().map(|d| d.link).unwrap_or_default();
        match rt.descriptor() {
            Some(d) if d.kind == SessionKind::Netplay => {
                let skew = rt
                    .shared()
                    .map(|s| s.stats.lock().unwrap().skew)
                    .unwrap_or(0);
                (true, skew, link_kind)
            }
            _ => (false, 0, link_kind),
        }
    };

    rsx! {
        // A dropped card dismisses on any click outside it: an
        // invisible catcher over the stage, under the chip row/card.
        if menu_open || expanded {
            div {
                class: "overlay-backdrop",
                onclick: move |_| {
                    *MENU_OPEN.write() = false;
                    *PANEL_OPEN.write() = false;
                },
            }
        }
        div { class: "session-overlay",
            // The chip pair holds the top center on every screen; the
            // dropped card hangs from the same spot. (Touch has no
            // Escape, so the menu chip is the menu's only way in there.)
            div { class: "chip-row",
                // Each chip toggles its own card; only one is down at a
                // time.
                button {
                    class: "btn status-chip",
                    onclick: move |_| {
                        let open = *MENU_OPEN.peek();
                        if !open {
                            *PANEL_OPEN.write() = false;
                        }
                        *MENU_OPEN.write() = !open;
                    },
                    icons::Menu {}
                    span { class: "chip-label", "Menu" }
                }
                button {
                    class: "btn status-chip",
                    onclick: move |_| {
                        let open = *PANEL_OPEN.peek();
                        if !open {
                            *MENU_OPEN.write() = false;
                        }
                        *PANEL_OPEN.write() = !open;
                    },
                    if is_netplay {
                        // Bars for link quality, colour for the same
                        // reading — no number.
                        span {
                            class: "signal",
                            style: "color: {telemetry::skew_tone_css(skew)}",
                            {telemetry::signal_icon(skew)}
                        }
                    } else if link_kind == crate::session::LinkKind::Wireless {
                        icons::Wifi {}
                    } else {
                        icons::Cable {}
                    }
                    span { class: "chip-label", {link_kind.label()} }
                }
            }
            if menu_open {
                session_view::SessionMenuCard {}
            } else if expanded && is_netplay {
                telemetry::TelemetryCard {}
            } else if expanded {
                cable::CableCard {}
            }
        }
    }
}
