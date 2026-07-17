//! The Settings tab: the netplay signaling server, the game database,
//! video/audio, and the input binding editor with live capture
//! (keyboard via the document listener, gamepad via the pump).
//! Identity (the nickname) lives in the main page's top bar.

use dioxus::prelude::*;

use super::play::{flash, Flash, FlashText};
use super::{icons, use_ctx, Ctx};
use crate::platform::input::{self, DescribeKind, MappedKey};
use crate::runtime::{CAPTURED, CAPTURE_TARGET};

#[component]
pub fn SettingsScreen() -> Element {
    let Ctx {
        mut config,
        storage,
        dat,
        ..
    } = use_ctx();
    let db_flash = use_signal(|| Option::<Flash>::None);
    let dat_names = dat.read().as_ref().map(|d| d.len()).unwrap_or(0);

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

    // The control whose bindings the detail row is editing.
    let selected = use_signal(|| Option::<MappedKey>::None);
    let sel = *selected.read();
    let sel_chips: Vec<(DescribeKind, String)> = sel
        .map(|k| config.read().mapping.slot(k).iter().map(input::describe).collect())
        .unwrap_or_default();

    let (volume_pct, integer_scaling) = {
        let cfg = config.read();
        ((cfg.volume * 100.0).round() as u32, cfg.integer_scaling)
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
        // Hidden on touch screens (CSS decides): play happens on the
        // on-screen controls there, so there's nothing to bind.
        section { class: "card input-bindings",
            h2 { "Input bindings" }
            // The console plate: controls sit where they do on the
            // machine — shoulders up top, d-pad left, A/B right,
            // Start/Select at the bottom. Every control wears a tiny
            // one-line hint of its first binding so the geometry never
            // shifts; the full binding list edits in the detail row
            // below the plate.
            div { class: "gba",
                div { class: "gba-l", BindControl { mapped: MappedKey::L, label: "L", shape: "shoulder", selected } }
                div { class: "gba-r", BindControl { mapped: MappedKey::R, label: "R", shape: "shoulder", selected } }
                div { class: "gba-dpad",
                    div { class: "dp-up", BindControl { mapped: MappedKey::Up, label: "▲", shape: "pad", selected } }
                    div { class: "dp-left", BindControl { mapped: MappedKey::Left, label: "◀", shape: "pad", selected } }
                    div { class: "dp-right", BindControl { mapped: MappedKey::Right, label: "▶", shape: "pad", selected } }
                    div { class: "dp-down", BindControl { mapped: MappedKey::Down, label: "▼", shape: "pad", selected } }
                }
                div { class: "gba-screen", span { "gbaroll" } }
                div { class: "gba-face",
                    div { class: "face-a", BindControl { mapped: MappedKey::A, label: "A", shape: "round", selected } }
                    div { class: "face-b", BindControl { mapped: MappedKey::B, label: "B", shape: "round", selected } }
                }
                div { class: "gba-pills",
                    BindControl { mapped: MappedKey::Select, label: "select", shape: "pill", selected }
                    BindControl { mapped: MappedKey::Start, label: "start", shape: "pill", selected }
                }
            }
            // Not a console control; it rides below the plate.
            div { class: "gba-extra",
                BindControl { mapped: MappedKey::SpeedUp, label: "fast-forward", shape: "pill", selected }
            }
            // The selected control's bindings, editable. One reserved
            // row — the prompt swapping in and out must not shift the
            // buttons below.
            div { class: "bind-detail",
                if let Some(key) = sel {
                    span { class: "bind-detail-label", "{key_label(key)}" }
                    div { class: "chips",
                        for (index , (kind , chip_label)) in sel_chips.into_iter().enumerate() {
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
                    }
                    if capture_target == Some(key) {
                        span { class: "sub", "press a key or button… (Esc cancels)" }
                    }
                } else {
                    span { class: "sub", "Click a control, then press a key or gamepad input to bind it. Chips remove on click." }
                }
            }
            button {
                class: "btn",
                onclick: move |_| config.with_mut(|c| c.mapping = Default::default()),
                "Reset to defaults"
            }
        }
        section { class: "card",
            h2 { "Game database" }
            div { class: "field",
                label { "No-Intro names" }
                span { class: "status", "{dat_names}" }
                button {
                    class: "btn",
                    onclick: move |_| {
                        let storage = storage.read().clone().flatten();
                        async move {
                            let mut dat = dat;
                            let Some(storage) = storage else { return };
                            match crate::nointro::fetch_gba_dat(&storage).await {
                                Ok(_) => flash(db_flash, "Updated!", true, 2500),
                                Err(e) => flash(db_flash, format!("update failed: {e:#}"), false, 5000),
                            }
                            dat.restart();
                        }
                    },
                    if let Some(f) = db_flash.read().clone() {
                        FlashText { flash: f }
                    } else {
                        icons::RefreshCw {}
                        "Update database"
                    }
                }
            }
        }
    }
}

/// The detail row's name for a control.
fn key_label(key: MappedKey) -> &'static str {
    match key {
        MappedKey::Up => "Up",
        MappedKey::Down => "Down",
        MappedKey::Left => "Left",
        MappedKey::Right => "Right",
        MappedKey::A => "A",
        MappedKey::B => "B",
        MappedKey::L => "L",
        MappedKey::R => "R",
        MappedKey::Start => "Start",
        MappedKey::Select => "Select",
        MappedKey::SpeedUp => "Fast-forward",
    }
}

/// One console control: the physical-looking button selects it into
/// the detail row and arms capture (clicking again cancels). The tiny
/// hint under it is the first binding, so the plate reads at a glance
/// without its geometry moving. `shape` picks the silhouette (`round`,
/// `pad`, `shoulder`, `pill`).
#[component]
fn BindControl(
    mapped: MappedKey,
    label: &'static str,
    shape: &'static str,
    selected: Signal<Option<MappedKey>>,
) -> Element {
    let Ctx { config, .. } = use_ctx();
    let mut selected = selected;
    let capturing = *CAPTURE_TARGET.read() == Some(mapped);
    let is_selected = *selected.read() == Some(mapped);
    let hint = {
        let cfg = config.read();
        let slot = cfg.mapping.slot(mapped);
        match slot.split_first() {
            None => "—".to_string(),
            Some((first, rest)) => {
                let (_, label) = input::describe(first);
                if rest.is_empty() {
                    label
                } else {
                    format!("{label} +{}", rest.len())
                }
            }
        }
    };

    rsx! {
        div { class: "gba-bind",
            button {
                class: "gba-btn {shape}",
                class: if capturing { "capturing" },
                class: if is_selected { "selected" },
                title: "Rebind {key_label(mapped)}",
                onclick: move |_| {
                    selected.set(Some(mapped));
                    if *CAPTURE_TARGET.peek() == Some(mapped) {
                        *CAPTURE_TARGET.write() = None;
                    } else {
                        *CAPTURED.write() = None;
                        *CAPTURE_TARGET.write() = Some(mapped);
                    }
                },
                "{label}"
            }
            span { class: "bind-hint", "{hint}" }
        }
    }
}
