//! The Settings tab: the netplay signaling server, the game database,
//! video/audio, and the input binding editor with live capture
//! (keyboard via the document listener, gamepad via the pump).
//! Identity (the nickname) lives in the main page's top bar.

use dioxus::prelude::*;

use super::{icons, use_ctx, Ctx};
use crate::platform::input::{self, DescribeKind, MappedKey};
use crate::runtime::{CAPTURED, CAPTURE_TARGET};

/// Order and labels of the binding editor's rows.
const MAPPED_KEYS: [(MappedKey, &str); 11] = [
    (MappedKey::Up, "Up"),
    (MappedKey::Down, "Down"),
    (MappedKey::Left, "Left"),
    (MappedKey::Right, "Right"),
    (MappedKey::A, "A"),
    (MappedKey::B, "B"),
    (MappedKey::L, "L"),
    (MappedKey::R, "R"),
    (MappedKey::Start, "Start"),
    (MappedKey::Select, "Select"),
    (MappedKey::SpeedUp, "Fast-forward"),
];

#[component]
pub fn SettingsScreen() -> Element {
    let Ctx { mut config, .. } = use_ctx();

    // Apply captured bindings. The Config is the source of truth; the
    // shell's sync effect mirrors it into the runtime's mapping.
    use_effect(move || {
        let Some((key, physical)) = CAPTURED.read().clone() else {
            return;
        };
        *CAPTURED.write() = None;
        config.with_mut(|c| {
            let slot = c.mapping.slot_mut(key);
            if !slot.contains(&physical) {
                slot.push(physical);
            }
        });
    });

    // Leaving the tab cancels any pending capture.
    use_drop(|| {
        *CAPTURE_TARGET.write() = None;
        *CAPTURED.write() = None;
    });

    let capture_target = *CAPTURE_TARGET.read();

    let (volume_pct, integer_scaling, rows) = {
        let cfg = config.read();
        let rows: Vec<(MappedKey, &'static str, Vec<(DescribeKind, String)>)> = MAPPED_KEYS
            .iter()
            .map(|&(key, label)| {
                (
                    key,
                    label,
                    cfg.mapping.slot(key).iter().map(input::describe).collect(),
                )
            })
            .collect();
        (
            (cfg.volume * 100.0).round() as u32,
            cfg.integer_scaling,
            rows,
        )
    };

    rsx! {
        section { class: "card",
            h2 { "Video / audio" }
            div { class: "field",
                label { "Volume" }
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
                span { class: "status", "{volume_pct}%" }
            }
            div { class: "field",
                label { "Integer scaling" }
                input {
                    r#type: "checkbox",
                    checked: integer_scaling,
                    onchange: move |evt: FormEvent| {
                        config.with_mut(|c| c.integer_scaling = evt.checked())
                    },
                }
            }
        }
        section { class: "card",
            h2 { "Input bindings" }
            div { class: "bindings",
                for (key, label, chips) in rows {
                    div { class: "field",
                        label { "{label}" }
                        div { class: "chips",
                            for (index, (kind, chip_label)) in chips.into_iter().enumerate() {
                                button {
                                    class: "chip",
                                    title: "Remove this binding",
                                    onclick: move |_| {
                                        config.with_mut(|c| {
                                            let slot = c.mapping.slot_mut(key);
                                            if index < slot.len() {
                                                slot.remove(index);
                                            }
                                        });
                                    },
                                    if kind == DescribeKind::Keyboard {
                                        icons::Keyboard {}
                                    } else {
                                        icons::Gamepad2 {}
                                    }
                                    span { "{chip_label}" }
                                    icons::X {}
                                }
                            }
                            if capture_target == Some(key) {
                                button {
                                    class: "chip capturing",
                                    onclick: move |_| *CAPTURE_TARGET.write() = None,
                                    "Press a key or button… (Esc cancels)"
                                }
                            } else {
                                button {
                                    class: "chip add",
                                    onclick: move |_| {
                                        *CAPTURED.write() = None;
                                        *CAPTURE_TARGET.write() = Some(key);
                                    },
                                    "+ Add"
                                }
                            }
                        }
                    }
                }
            }
            button {
                class: "btn",
                onclick: move |_| config.with_mut(|c| c.mapping = Default::default()),
                "Reset to defaults"
            }
        }
    }
}
