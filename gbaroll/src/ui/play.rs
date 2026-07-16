//! The Play tab: pick a game, pick a save, hit Play. The library and
//! the saves are twin OPFS-backed pick-lists with import, search,
//! rename (saves), and delete; action feedback flashes inline next to
//! whatever triggered it.

use dioxus::prelude::*;

use super::{icons, use_ctx, Ctx};
use crate::library::{self, RomInfo};
use crate::storage;

/// One inline feedback message: `ok` renders green with a check,
/// otherwise the danger tone.
#[derive(Clone, PartialEq)]
pub(crate) struct Flash {
    text: String,
    ok: bool,
}

/// Show a message in an inline feedback slot, clearing it after `ms`
/// unless something newer landed meanwhile. Feedback lives next to the
/// control that produced it — there is no global notice bar.
pub(crate) fn flash(mut slot: Signal<Option<Flash>>, text: impl Into<String>, ok: bool, ms: u32) {
    let text = text.into();
    slot.set(Some(Flash {
        text: text.clone(),
        ok,
    }));
    spawn(async move {
        gloo_timers::future::TimeoutFuture::new(ms).await;
        if slot.peek().as_ref().is_some_and(|f| f.text == text) {
            slot.set(None);
        }
    });
}

/// The two import feedback slots — global so the shell-level drop
/// handler (the whole Play area is one drop target) can flash the
/// side(s) that actually received something.
pub(crate) static ROM_IMPORT_FLASH: GlobalSignal<Option<Flash>> = Signal::global(|| None);
pub(crate) static SAVE_IMPORT_FLASH: GlobalSignal<Option<Flash>> = Signal::global(|| None);

/// Flash a drop's outcome onto whichever side(s) it landed: ROMs on
/// the library, saves on the saves pane, skips reported on the library
/// side unless only saves imported.
pub(crate) fn import_flashes(roms: u32, saves: u32, skipped: u32) {
    let skips_on_saves = roms == 0 && saves > 0;
    if roms > 0 || (skipped > 0 && !skips_on_saves) {
        let msg = if skipped == 0 {
            format!("Imported {roms}!")
        } else {
            format!("Imported {roms}, skipped {skipped}")
        };
        flash(ROM_IMPORT_FLASH.signal(), msg, skipped == 0, 3000);
    }
    if saves > 0 {
        let msg = if skips_on_saves && skipped > 0 {
            format!("Imported {saves}, skipped {skipped}")
        } else {
            format!("Imported {saves}!")
        };
        flash(SAVE_IMPORT_FLASH.signal(), msg, !(skips_on_saves && skipped > 0), 3000);
    }
}

/// The rendered form of a [`Flash`].
#[component]
pub(crate) fn FlashText(flash: Flash) -> Element {
    rsx! {
        span { class: if flash.ok { "flash-ok" } else { "link-notice" },
            if flash.ok {
                icons::Check {}
            }
            "{flash.text}"
        }
    }
}

/// The save the picker auto-selects for a game: the one whose stem is
/// the game's display name (the write-back default).
fn matching_save(saves: &[String], display: &str) -> Option<String> {
    saves
        .iter()
        .find(|s| stem_of(s).eq_ignore_ascii_case(display))
        .cloned()
}

/// Split a save's file name around its final dot.
fn stem_of(name: &str) -> &str {
    name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(name)
}

fn ext_of(name: &str) -> &str {
    name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("sav")
}

#[component]
pub fn PlayScreen() -> Element {
    let Ctx {
        runtime,
        mut config,
        mut library_rev,
        storage: storage_res,
        library,
        mut selected_save,
        ..
    } = use_ctx();

    let mut search = use_signal(String::new);
    let mut save_search = use_signal(String::new);
    // Launching is two picks and a click: pick a game (auto-picks its
    // matching save), adjust the save if wanted, hit Play. The last
    // pick is remembered across loads.
    let mut selected_game = use_signal(|| config.peek().last_game);
    // Inline action feedback slots.
    let rom_import_flash = ROM_IMPORT_FLASH.signal();
    let save_import_flash = SAVE_IMPORT_FLASH.signal();
    let library_flash = use_signal(|| Option::<Flash>::None);
    let save_flash = use_signal(|| Option::<Flash>::None);
    let launch_flash = use_signal(|| Option::<Flash>::None);

    // The remembered pick restores its matching save once the library
    // arrives (only if nothing else was chosen yet).
    let mut restored = use_signal(|| false);
    use_effect(move || {
        if *restored.peek() {
            return;
        }
        let guard = library.read();
        let Some(Some((lib, saves))) = guard.as_ref() else {
            return;
        };
        restored.set(true);
        if selected_save.peek().is_none() {
            if let Some(info) =
                (*selected_game.peek()).and_then(|crc| lib.by_crc32(crc))
            {
                if let Some(matched) = matching_save(saves, info.display_name()) {
                    selected_save.set(Some(matched));
                }
            }
        }
    });
    // Deleting a ROM or save is irreversible, so it takes two clicks:
    // arm on the row, then confirm in place.
    let mut pending_delete = use_signal(|| Option::<String>::None);
    let mut pending_save_delete = use_signal(|| Option::<String>::None);
    // The save being renamed, and the stem typed so far.
    let mut rename_target = use_signal(|| Option::<String>::None);
    let mut rename_value = use_signal(String::new);

    let (scanned, roms, saves) = match library.read().as_ref() {
        Some(Some((lib, saves))) => (true, lib.roms.clone(), saves.clone()),
        _ => (false, Vec::new(), Vec::new()),
    };
    let opfs_down = matches!(storage_res.read().as_ref(), Some(None));

    let needle = search.read().to_ascii_lowercase();
    let filtered: Vec<RomInfo> = roms
        .iter()
        .filter(|r| {
            needle.is_empty()
                || r.display_name().to_ascii_lowercase().contains(&needle)
                || r.title.to_ascii_lowercase().contains(&needle)
                || r.code.to_ascii_lowercase().contains(&needle)
        })
        .cloned()
        .collect();
    let save_needle = save_search.read().to_ascii_lowercase();
    let filtered_saves: Vec<String> = saves
        .iter()
        .filter(|s| save_needle.is_empty() || s.to_ascii_lowercase().contains(&save_needle))
        .cloned()
        .collect();

    let selected_info: Option<RomInfo> = selected_game
        .read()
        .and_then(|crc| roms.iter().find(|r| r.crc32 == crc).cloned());

    rsx! {
        section { class: "card",
            div { class: "card-head",
                div {
                    h2 { "Game library" }
                }
            }
            input {
                class: "search",
                placeholder: "Search by title or code…",
                value: "{search}",
                spellcheck: "false",
                autocomplete: "off",
                oninput: move |evt: FormEvent| search.set(evt.value()),
            }
            if let Some(f) = library_flash.read().clone() {
                p { class: "sub", FlashText { flash: f } }
            }
            if opfs_down {
                div { class: "empty",
                    p { "This browser doesn't offer origin-private storage; the library is unavailable." }
                }
            } else {
                div { class: "rom-list",
                    // The importer leads the list as a ghost row; while
                    // flashing it wears the result's tone.
                    label {
                        class: "pick-row ghost-add file-btn",
                        class: if rom_import_flash.read().as_ref().is_some_and(|f| f.ok) { "success" },
                        class: if rom_import_flash.read().as_ref().is_some_and(|f| !f.ok) { "danger" },
                        if let Some(f) = rom_import_flash.read().clone() {
                            FlashText { flash: f }
                        } else {
                            icons::Upload {}
                            "Import ROMs…"
                        }
                        input {
                            r#type: "file",
                            accept: ".gba,.agb,.srl",
                            multiple: true,
                            onchange: move |evt| {
                                let storage = storage_res.read().clone().flatten();
                                async move {
                                    let Some(storage) = storage else { return };
                                    let (r, _, skipped) =
                                        crate::web::import_files(&storage, evt.files()).await;
                                    let msg = if skipped == 0 {
                                        format!("Imported {r}!")
                                    } else {
                                        format!("Imported {r}, skipped {skipped}")
                                    };
                                    flash(rom_import_flash, msg, skipped == 0, 3000);
                                    *library_rev.write() += 1;
                                }
                            },
                        }
                    }
                    if !scanned {
                        div { class: "empty", p { "Scanning the library…" } }
                    } else if filtered.is_empty() {
                        div { class: "empty",
                            icons::Gamepad2 {}
                            p { if roms.is_empty() { "No games yet" } else { "No games match your search" } }
                            p { class: "sub",
                                if roms.is_empty() { "Import .gba files to get started." } else { "Try a title or game code." }
                            }
                        }
                    }
                    // One radio group: native click, Tab, and arrow-key
                    // behavior — the radio itself is visually hidden,
                    // the row is its label.
                    for rom in filtered {
                        div {
                            class: if *selected_game.read() == Some(rom.crc32) { "pick-row selected" } else { "pick-row" },
                            label { class: "pick-label",
                                input {
                                    r#type: "radio",
                                    name: "game-pick",
                                    class: "pick-radio",
                                    checked: *selected_game.read() == Some(rom.crc32),
                                    // Picking the game brings its matching
                                    // save along (same stem as the display
                                    // name); no match = fresh save.
                                    onchange: {
                                        let crc = rom.crc32;
                                        let display = rom.display_name().to_string();
                                        let saves = saves.clone();
                                        move |_| {
                                            selected_game.set(Some(crc));
                                            selected_save.set(matching_save(&saves, &display));
                                            // Remembered for the next load.
                                            config.with_mut(|c| c.last_game = Some(crc));
                                        }
                                    },
                                }
                                div { class: "rom-name",
                                    span { class: "game", "{rom.display_name()}" }
                                    span { class: "rom-meta",
                                        "{rom.code} · "
                                        code { {format!("{:08x}", rom.crc32)} }
                                        " · {rom.size / 1024} KiB"
                                    }
                                }
                            }
                            div { class: "row-actions",
                                if pending_delete.read().as_deref() == Some(rom.file_name.as_str()) {
                                    button {
                                        class: "btn danger",
                                        onclick: {
                                            let file_name = rom.file_name.clone();
                                            move |evt: MouseEvent| {
                                                evt.stop_propagation();
                                                let storage = storage_res.read().clone().flatten();
                                                let file_name = file_name.clone();
                                                async move {
                                                    let Some(storage) = storage else { return };
                                                    if let Err(e) =
                                                        storage::delete(storage.roms(), &file_name).await
                                                    {
                                                        flash(
                                                            library_flash,
                                                            format!("couldn't delete {file_name}: {e}"),
                                                            false,
                                                            5000,
                                                        );
                                                    }
                                                    pending_delete.set(None);
                                                    *library_rev.write() += 1;
                                                }
                                            }
                                        },
                                        "Delete"
                                    }
                                    button {
                                        class: "btn",
                                        onclick: move |evt: MouseEvent| {
                                            evt.stop_propagation();
                                            pending_delete.set(None);
                                        },
                                        "Cancel"
                                    }
                                } else {
                                    button {
                                        class: "btn ghost icon-btn",
                                        title: "Delete {rom.display_name()}",
                                        onclick: {
                                            let file_name = rom.file_name.clone();
                                            move |evt: MouseEvent| {
                                                evt.stop_propagation();
                                                pending_delete.set(Some(file_name.clone()));
                                            }
                                        },
                                        icons::Trash2 {}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        section { class: "card",
            div { class: "card-head",
                div {
                    h2 { "Save data" }
                }
            }
            input {
                class: "search",
                placeholder: "Search saves…",
                value: "{save_search}",
                spellcheck: "false",
                autocomplete: "off",
                oninput: move |evt: FormEvent| save_search.set(evt.value()),
            }
            if let Some(f) = save_flash.read().clone() {
                p { class: "sub", FlashText { flash: f } }
            }
            div { class: "save-list",
                // The importer leads the list as a ghost row; while
                // flashing it wears the result's tone.
                label {
                    class: "pick-row ghost-add file-btn",
                    class: if save_import_flash.read().as_ref().is_some_and(|f| f.ok) { "success" },
                    class: if save_import_flash.read().as_ref().is_some_and(|f| !f.ok) { "danger" },
                    if let Some(f) = save_import_flash.read().clone() {
                        FlashText { flash: f }
                    } else {
                        icons::Upload {}
                        "Import saves…"
                    }
                    input {
                        r#type: "file",
                        accept: ".sav,.sa1,.srm",
                        multiple: true,
                        onchange: move |evt| {
                            let storage = storage_res.read().clone().flatten();
                            async move {
                                let Some(storage) = storage else { return };
                                let (_, s, skipped) =
                                    crate::web::import_files(&storage, evt.files()).await;
                                let msg = if skipped == 0 {
                                    format!("Imported {s}!")
                                } else {
                                    format!("Imported {s}, skipped {skipped}")
                                };
                                flash(save_import_flash, msg, skipped == 0, 3000);
                                *library_rev.write() += 1;
                            }
                        },
                    }
                }
                // The same radio-group widget as the game library; a
                // fresh save is its own row rather than a dropdown
                // special case.
                div {
                    class: if selected_save.read().is_none() { "pick-row selected" } else { "pick-row" },
                    label { class: "pick-label",
                        input {
                            r#type: "radio",
                            name: "save-pick",
                            class: "pick-radio",
                            checked: selected_save.read().is_none(),
                            onchange: move |_| selected_save.set(None),
                        }
                        span { class: "sub", "(fresh save)" }
                    }
                }
                for save in filtered_saves {
                    div {
                        class: if selected_save.read().as_deref() == Some(save.as_str()) { "pick-row selected" } else { "pick-row" },
                        if rename_target.read().as_deref() == Some(save.as_str()) {
                            div { class: "pick-label",
                                // Renaming: edit the stem, keep the extension.
                                input {
                                    class: "rename",
                                    value: "{rename_value}",
                                    spellcheck: "false",
                                    autocomplete: "off",
                                    onclick: move |evt: MouseEvent| evt.stop_propagation(),
                                    oninput: move |evt: FormEvent| rename_value.set(evt.value()),
                                }
                                code { {format!(".{}", ext_of(&save))} }
                            }
                            div { class: "row-actions",
                                button {
                                    class: "btn primary",
                                    disabled: rename_value.read().trim().is_empty(),
                                    onclick: {
                                        let save = save.clone();
                                        move |evt: MouseEvent| {
                                            evt.stop_propagation();
                                            let storage = storage_res.read().clone().flatten();
                                            let save = save.clone();
                                            let to = format!(
                                                "{}.{}",
                                                rename_value.read().trim(),
                                                ext_of(&save)
                                            );
                                            async move {
                                                let Some(storage) = storage else { return };
                                                match storage::rename(storage.saves(), &save, &to).await {
                                                    Ok(()) => {
                                                        // The picker follows the rename.
                                                        if selected_save.read().as_deref()
                                                            == Some(save.as_str())
                                                        {
                                                            selected_save.set(Some(to));
                                                        }
                                                    }
                                                    Err(e) => flash(
                                                        save_flash,
                                                        format!("couldn't rename {save}: {e}"),
                                                        false,
                                                        5000,
                                                    ),
                                                }
                                                rename_target.set(None);
                                                *library_rev.write() += 1;
                                            }
                                        }
                                    },
                                    "Rename"
                                }
                                button {
                                    class: "btn",
                                    onclick: move |evt: MouseEvent| {
                                        evt.stop_propagation();
                                        rename_target.set(None);
                                    },
                                    "Cancel"
                                }
                            }
                        } else if pending_save_delete.read().as_deref() == Some(save.as_str()) {
                            div { class: "pick-label",
                                code { "{save}" }
                            }
                            div { class: "row-actions",
                                button {
                                    class: "btn danger",
                                    onclick: {
                                        let save = save.clone();
                                        move |evt: MouseEvent| {
                                            evt.stop_propagation();
                                            let storage = storage_res.read().clone().flatten();
                                            let save = save.clone();
                                            async move {
                                                let Some(storage) = storage else { return };
                                                if let Err(e) =
                                                    storage::delete(storage.saves(), &save).await
                                                {
                                                    flash(
                                                        save_flash,
                                                        format!("couldn't delete {save}: {e}"),
                                                        false,
                                                        5000,
                                                    );
                                                } else if selected_save.read().as_deref()
                                                    == Some(save.as_str())
                                                {
                                                    selected_save.set(None);
                                                }
                                                pending_save_delete.set(None);
                                                *library_rev.write() += 1;
                                            }
                                        }
                                    },
                                    "Delete"
                                }
                                button {
                                    class: "btn",
                                    onclick: move |evt: MouseEvent| {
                                        evt.stop_propagation();
                                        pending_save_delete.set(None);
                                    },
                                    "Cancel"
                                }
                            }
                        } else {
                            label { class: "pick-label",
                                input {
                                    r#type: "radio",
                                    name: "save-pick",
                                    class: "pick-radio",
                                    checked: selected_save.read().as_deref() == Some(save.as_str()),
                                    onchange: {
                                        let save = save.clone();
                                        move |_| selected_save.set(Some(save.clone()))
                                    },
                                }
                                code { "{save}" }
                            }
                            div { class: "row-actions",
                                button {
                                    class: "btn ghost icon-btn",
                                    title: "Rename",
                                    onclick: {
                                        let save = save.clone();
                                        move |evt: MouseEvent| {
                                            evt.stop_propagation();
                                            rename_value.set(stem_of(&save).to_string());
                                            rename_target.set(Some(save.clone()));
                                            pending_save_delete.set(None);
                                        }
                                    },
                                    icons::Pencil {}
                                }
                                button {
                                    class: "btn ghost",
                                    onclick: {
                                        let save = save.clone();
                                        move |evt: MouseEvent| {
                                            evt.stop_propagation();
                                            let storage = storage_res.read().clone().flatten();
                                            let save = save.clone();
                                            async move {
                                                let Some(storage) = storage else { return };
                                                match storage::read(storage.saves(), &save).await {
                                                    Ok(Some(bytes)) => crate::web::download_bytes(&save, &bytes),
                                                    _ => flash(
                                                        save_flash,
                                                        format!("couldn't read {save}"),
                                                        false,
                                                        5000,
                                                    ),
                                                }
                                            }
                                        }
                                    },
                                    icons::Download {}
                                    "Export"
                                }
                                button {
                                    class: "btn ghost icon-btn",
                                    title: "Delete",
                                    onclick: {
                                        let save = save.clone();
                                        move |evt: MouseEvent| {
                                            evt.stop_propagation();
                                            pending_save_delete.set(Some(save.clone()));
                                            rename_target.set(None);
                                        }
                                    },
                                    icons::Trash2 {}
                                }
                            }
                        }
                    }
                }
            }
        }
        // The footer: just Play, right-aligned (grayed out until a game
        // is picked). Launch failures flash in transiently beside it.
        div { class: "launch-bar",
                if let Some(f) = launch_flash.read().clone() {
                    FlashText { flash: f }
                }
                button {
                    class: "btn primary launch",
                    disabled: selected_info.is_none(),
                    onclick: {
                        let runtime = runtime.clone();
                        let info = selected_info.clone();
                        move |_| {
                            let Some(info) = info.clone() else { return };
                            let runtime = runtime.clone();
                            let storage = storage_res.read().clone().flatten();
                            let save_name = selected_save.read().clone();
                            spawn(async move {
                                let Some(storage) = storage else { return };
                                let bytes = match library::read_rom(&storage, &info).await {
                                    Ok(b) => b,
                                    Err(e) => {
                                        flash(launch_flash, format!("{e:#}"), false, 6000);
                                        return;
                                    }
                                };
                                let save = match &save_name {
                                    Some(name) => {
                                        storage::read(storage.saves(), name).await.ok().flatten()
                                    }
                                    None => None,
                                };
                                // SRAM writes back into the picked save, or a
                                // fresh "<name>.sav" the first time it persists.
                                let save_file = save_name.clone().unwrap_or_else(|| {
                                    format!("{}.sav", info.display_name())
                                });
                                if let Err(e) =
                                    crate::web::boot(runtime, bytes, save, Some(save_file)).await
                                {
                                    flash(
                                        launch_flash,
                                        format!("couldn't start session: {e:#}"),
                                        false,
                                        6000,
                                    );
                                }
                            });
                        }
                    },
                    icons::Play {}
                    "Play"
                }
        }
    }
}
