//! The Play tab: pick a game, pick one of *its* saves, hit Play. The
//! library and the saves are twin OPFS-backed pick-lists with import,
//! search, rename (saves), and delete; saves are namespaced per game
//! (`saves/<crc32>/`), the save pane only ever shows the selected
//! game's, and each game remembers its last-picked save. Action
//! feedback flashes inline next to whatever triggered it.

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

/// Flash an import's outcome onto whichever side(s) it landed: ROMs on
/// the library, saves on the saves pane, skips reported on the library
/// side unless only saves imported. When nothing landed at all the
/// import *failed*, and the failure reports on `fail_slot` — the pane
/// whose picker ran, or the library for area-wide drops.
pub(crate) fn import_flashes(
    counts: crate::web::ImportCounts,
    fail_slot: Signal<Option<Flash>>,
) {
    let crate::web::ImportCounts {
        roms,
        saves,
        skipped,
        saves_without_game,
        ..
    } = counts;
    // Saves offered with no game picked had nowhere to land; the save
    // pane says what to do about it.
    if saves_without_game > 0 {
        flash(
            SAVE_IMPORT_FLASH.signal(),
            "Pick a game first, then import its saves",
            false,
            5000,
        );
    }
    if roms == 0 && saves == 0 {
        // Homeless saves already explained themselves above.
        if saves_without_game > 0 {
            if skipped > 0 {
                flash(fail_slot, format!("Skipped {skipped}"), false, 5000);
            }
            return;
        }
        let msg = if skipped > 0 {
            format!("Import failed, skipped {skipped}")
        } else {
            // The picker handed over nothing (iOS does this).
            "Import failed: no files received".to_string()
        };
        flash(fail_slot, msg, false, 5000);
        return;
    }
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

/// A lone new arrival is what the player wants next: a single
/// imported ROM becomes the selected game (bringing its remembered
/// save), else a single imported save becomes the selected pick.
/// Batches select nothing. When a lone ROM and a lone save arrive
/// together the ROM wins — the save landed in the *previously*
/// selected game's directory, so picking it under the new game would
/// name a file that isn't there.
pub(crate) fn select_imported(
    counts: &crate::web::ImportCounts,
    mut selected_game: Signal<Option<u32>>,
    mut selected_save: Signal<Option<String>>,
    mut config: Signal<crate::config::Config>,
) {
    if counts.roms == 1 {
        if let Some(crc32) = counts.rom_crc32 {
            selected_game.set(Some(crc32));
            selected_save.set(config.peek().last_saves.get(&crc32).cloned());
            config.with_mut(|c| c.last_game = Some(crc32));
        }
    } else if counts.saves == 1 {
        if let Some(name) = &counts.save_name {
            selected_save.set(Some(name.clone()));
            if let Some(crc32) = *selected_game.peek() {
                config.with_mut(|c| {
                    c.last_saves.insert(crc32, name.clone());
                });
            }
        }
    }
}

#[component]
pub fn PlayScreen() -> Element {
    let Ctx {
        runtime,
        mut config,
        mut library_rev,
        storage: storage_res,
        library,
        mut selected_game,
        mut selected_save,
        ..
    } = use_ctx();

    let mut search = use_signal(String::new);
    let mut save_search = use_signal(String::new);
    // Launching is two picks and a click: pick a game (its default
    // save comes along), adjust the save if wanted, hit Play. The
    // last pick is remembered across loads.
    // Inline action feedback slots.
    let rom_import_flash = ROM_IMPORT_FLASH.signal();
    let save_import_flash = SAVE_IMPORT_FLASH.signal();
    let library_flash = use_signal(|| Option::<Flash>::None);
    let save_flash = use_signal(|| Option::<Flash>::None);
    let launch_flash = use_signal(|| Option::<Flash>::None);

    // The selected game's saves, listed straight from its directory
    // on every pick and every SAVES_REV bump — there is no cached
    // index. The listed game rides along so a stale listing can't
    // speak for a new pick, and `authoritative` marks a listing taken
    // with no SRAM write-back in flight — one taken mid-write may
    // predate the file the write is creating.
    let game_saves_res = use_resource(move || {
        let _ = crate::runtime::SAVES_REV.read();
        let authoritative = *crate::runtime::SAVES_IN_FLIGHT.read() == 0;
        let crc32 = *selected_game.read();
        let storage = storage_res.read().clone();
        async move {
            match (storage.flatten(), crc32) {
                (Some(storage), Some(crc32)) => (
                    Some(crc32),
                    authoritative,
                    library::list_game_saves(&storage, crc32).await,
                ),
                // No storage yet: an empty answer that must not speak
                // for the game (the pruning below would trust it).
                _ => (None, false, Vec::new()),
            }
        }
    });
    // A picked save the directory doesn't actually hold (a stale
    // remembered pick, a file deleted elsewhere) falls back to fresh
    // once an authoritative listing lands, and the memory of it goes
    // too.
    use_effect(move || {
        let guard = game_saves_res.read();
        let Some((for_game, authoritative, saves)) = guard.as_ref() else {
            return;
        };
        if !authoritative || *for_game != *selected_game.peek() {
            return;
        }
        let sel = selected_save.peek().clone();
        if let Some(sel) = sel {
            if !saves.contains(&sel) {
                selected_save.set(None);
                if let Some(crc32) = *for_game {
                    config.with_mut(|c| {
                        c.last_saves.remove(&crc32);
                    });
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
    // Commit the rename editor: `save` gets the typed stem, keeping
    // its extension. A blank stem is a no-op (the submit button is
    // disabled, which also blocks Enter's implicit submission).
    let commit_rename = move |save: String| {
        let stem = rename_value.read().trim().to_string();
        if stem.is_empty() {
            return;
        }
        let Some(crc32) = *selected_game.peek() else {
            return;
        };
        let to = format!("{stem}.{}", library::ext_of(&save));
        let storage = storage_res.read().clone().flatten();
        spawn(async move {
            let Some(storage) = storage else { return };
            let renamed = match storage.save_dir(crc32).await {
                Ok(dir) => storage::rename(&dir, &save, &to).await,
                Err(e) => Err(e),
            };
            match renamed {
                Ok(()) => {
                    // The picker and the remembered pick follow.
                    if selected_save.read().as_deref() == Some(save.as_str()) {
                        selected_save.set(Some(to.clone()));
                    }
                    if config.peek().last_saves.get(&crc32).map(String::as_str)
                        == Some(save.as_str())
                    {
                        config.with_mut(|c| {
                            c.last_saves.insert(crc32, to.clone());
                        });
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
            *crate::runtime::SAVES_REV.write() += 1;
        });
    };

    let (scanned, roms) = match library.read().as_ref() {
        Some(Some(lib)) => (true, lib.roms.clone()),
        _ => (false, Vec::new()),
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

    let selected_info: Option<RomInfo> = selected_game
        .read()
        .and_then(|crc| roms.iter().find(|r| r.crc32 == crc).cloned());

    // The save pane shows only the selected game's saves (empty while
    // a fresh pick's directory is still listing).
    let game_crc32 = selected_info.as_ref().map(|r| r.crc32);
    let game_saves: Vec<String> = game_saves_res
        .read()
        .as_ref()
        .filter(|(for_game, _, _)| *for_game == game_crc32)
        .map(|(_, _, saves)| saves.clone())
        .unwrap_or_default();
    let save_needle = save_search.read().to_ascii_lowercase();
    let filtered_saves: Vec<String> = game_saves
        .iter()
        .filter(|s| save_needle.is_empty() || s.to_ascii_lowercase().contains(&save_needle))
        .cloned()
        .collect();

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
                            // iOS greys out every file in its picker when
                            // `accept` names extensions the system has no
                            // type for (.gba and friends), so there the
                            // picker goes unfiltered.
                            accept: if !crate::web::is_ios() { ".gba,.agb,.srl" },
                            multiple: true,
                            onchange: move |evt| {
                                let storage = storage_res.read().clone().flatten();
                                // iOS's unfiltered picker can hand this
                                // importer saves too; they land with the
                                // selected game like everywhere else.
                                let dest = game_crc32;
                                let files = evt.files();
                                // Re-picking the same file must fire again.
                                crate::web::reset_file_input(&evt);
                                async move {
                                    let Some(storage) = storage else { return };
                                    let counts =
                                        crate::web::import_files(&storage, files, dest).await;
                                    select_imported(&counts, selected_game, selected_save, config);
                                    import_flashes(counts, rom_import_flash);
                                    *library_rev.write() += 1;
                                    *crate::runtime::SAVES_REV.write() += 1;
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
                                    // Picking the game restores its last-
                                    // picked save (pruned if the file turns
                                    // out gone); none remembered = fresh.
                                    onchange: {
                                        let crc32 = rom.crc32;
                                        move |_| {
                                            selected_game.set(Some(crc32));
                                            selected_save.set(
                                                config.peek().last_saves.get(&crc32).cloned(),
                                            );
                                            // Remembered for the next load.
                                            config.with_mut(|c| c.last_game = Some(crc32));
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
                                            let crc32 = rom.crc32;
                                            move |evt: MouseEvent| {
                                                evt.stop_propagation();
                                                let storage = storage_res.read().clone().flatten();
                                                let file_name = file_name.clone();
                                                async move {
                                                    let Some(storage) = storage else { return };
                                                    match storage::delete(storage.roms(), &file_name).await {
                                                        Err(e) => flash(
                                                            library_flash,
                                                            format!("couldn't delete {file_name}: {e}"),
                                                            false,
                                                            5000,
                                                        ),
                                                        // The game's saves go with it —
                                                        // without their game they're
                                                        // unreachable dead weight.
                                                        Ok(()) => {
                                                            if let Err(e) =
                                                                storage.delete_save_dir(crc32).await
                                                            {
                                                                flash(
                                                                    library_flash,
                                                                    format!(
                                                                        "deleted {file_name}, but not its saves: {e}"
                                                                    ),
                                                                    false,
                                                                    5000,
                                                                );
                                                            }
                                                            // The pick and the remembered
                                                            // game/save all forget it.
                                                            if *selected_game.peek() == Some(crc32) {
                                                                selected_game.set(None);
                                                                selected_save.set(None);
                                                            }
                                                            config.with_mut(|c| {
                                                                c.last_saves.remove(&crc32);
                                                                if c.last_game == Some(crc32) {
                                                                    c.last_game = None;
                                                                }
                                                            });
                                                        }
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
                                        title: "Delete",
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
                    // Whose saves these are — the pane only ever shows
                    // the selected game's.
                    if let Some(info) = &selected_info {
                        span { class: "sub", "{info.display_name()}" }
                    }
                }
            }
            if selected_info.is_some() {
                input {
                    class: "search",
                    placeholder: "Search saves…",
                    value: "{save_search}",
                    spellcheck: "false",
                    autocomplete: "off",
                    oninput: move |evt: FormEvent| save_search.set(evt.value()),
                }
            }
            if let Some(f) = save_flash.read().clone() {
                p { class: "sub", FlashText { flash: f } }
            }
            div { class: "save-list",
                // Saves are per-game: with no game picked there is
                // nothing to list, and imports have nowhere to land.
                if selected_info.is_none() {
                    div { class: "empty",
                        icons::Save {}
                        p { "No game picked" }
                        p { class: "sub", "Each game keeps its own saves — pick one to see them." }
                        // A save dropped in this state flashes its
                        // refusal here — the import row that usually
                        // hosts the slot isn't rendered.
                        if let Some(f) = save_import_flash.read().clone() {
                            p { class: "sub", FlashText { flash: f } }
                        }
                    }
                }
                if selected_info.is_some() {
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
                            // Unfiltered on iOS — see the ROM picker.
                            accept: if !crate::web::is_ios() { ".sav,.sa1,.srm" },
                            multiple: true,
                            onchange: move |evt| {
                                let storage = storage_res.read().clone().flatten();
                                // Into the selected game's namespace.
                                let dest = game_crc32;
                                let files = evt.files();
                                // Re-picking the same file must fire again.
                                crate::web::reset_file_input(&evt);
                                async move {
                                    let Some(storage) = storage else { return };
                                    let counts =
                                        crate::web::import_files(&storage, files, dest).await;
                                    select_imported(&counts, selected_game, selected_save, config);
                                    import_flashes(counts, save_import_flash);
                                    *library_rev.write() += 1;
                                    *crate::runtime::SAVES_REV.write() += 1;
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
                                onchange: move |_| {
                                    selected_save.set(None);
                                    // Choosing fresh forgets the game's
                                    // remembered pick.
                                    if let Some(crc32) = *selected_game.peek() {
                                        config.with_mut(|c| {
                                            c.last_saves.remove(&crc32);
                                        });
                                    }
                                },
                            }
                            span { class: "sub", "(fresh save)" }
                        }
                    }
                }
                for save in filtered_saves {
                    div {
                        class: if selected_save.read().as_deref() == Some(save.as_str()) { "pick-row selected" } else { "pick-row" },
                        if rename_target.read().as_deref() == Some(save.as_str()) {
                            form {
                                class: "rename-form",
                                // Enter and the Rename button both land here.
                                onsubmit: {
                                    let save = save.clone();
                                    move |evt: FormEvent| {
                                        evt.prevent_default();
                                        commit_rename(save.clone());
                                    }
                                },
                                div { class: "pick-label",
                                    // Renaming: edit the stem, keep the extension.
                                    input {
                                        class: "rename",
                                        value: "{rename_value}",
                                        spellcheck: "false",
                                        autocomplete: "off",
                                        // The editor opens focused.
                                        onmounted: move |evt| async move {
                                            let _ = evt.set_focus(true).await;
                                        },
                                        onclick: move |evt: MouseEvent| evt.stop_propagation(),
                                        oninput: move |evt: FormEvent| rename_value.set(evt.value()),
                                    }
                                    code { {format!(".{}", library::ext_of(&save))} }
                                }
                                div { class: "row-actions",
                                    button {
                                        class: "btn primary",
                                        r#type: "submit",
                                        disabled: rename_value.read().trim().is_empty(),
                                        onclick: move |evt: MouseEvent| evt.stop_propagation(),
                                        "Rename"
                                    }
                                    button {
                                        class: "btn",
                                        r#type: "button",
                                        onclick: move |evt: MouseEvent| {
                                            evt.stop_propagation();
                                            rename_target.set(None);
                                        },
                                        "Cancel"
                                    }
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
                                            let crc32 = *selected_game.peek();
                                            let save = save.clone();
                                            async move {
                                                let (Some(storage), Some(crc32)) = (storage, crc32)
                                                else {
                                                    return;
                                                };
                                                let deleted = match storage.save_dir(crc32).await {
                                                    Ok(dir) => storage::delete(&dir, &save).await,
                                                    Err(e) => Err(e),
                                                };
                                                if let Err(e) = deleted {
                                                    flash(
                                                        save_flash,
                                                        format!("couldn't delete {save}: {e}"),
                                                        false,
                                                        5000,
                                                    );
                                                } else {
                                                    if selected_save.read().as_deref()
                                                        == Some(save.as_str())
                                                    {
                                                        selected_save.set(None);
                                                    }
                                                    // No remembering deleted
                                                    // saves.
                                                    if config
                                                        .peek()
                                                        .last_saves
                                                        .get(&crc32)
                                                        .map(String::as_str)
                                                        == Some(save.as_str())
                                                    {
                                                        config.with_mut(|c| {
                                                            c.last_saves.remove(&crc32);
                                                        });
                                                    }
                                                }
                                                pending_save_delete.set(None);
                                                *crate::runtime::SAVES_REV.write() += 1;
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
                                        move |_| {
                                            selected_save.set(Some(save.clone()));
                                            // Remembered per game, restored
                                            // when the game is picked again.
                                            if let Some(crc32) = *selected_game.peek() {
                                                config.with_mut(|c| {
                                                    c.last_saves.insert(crc32, save.clone());
                                                });
                                            }
                                        }
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
                                            rename_value.set(library::stem_of(&save).to_string());
                                            rename_target.set(Some(save.clone()));
                                            pending_save_delete.set(None);
                                        }
                                    },
                                    icons::Pencil {}
                                }
                                button {
                                    class: "btn ghost icon-btn",
                                    title: "Export",
                                    onclick: {
                                        let save = save.clone();
                                        move |evt: MouseEvent| {
                                            evt.stop_propagation();
                                            let storage = storage_res.read().clone().flatten();
                                            let crc32 = *selected_game.peek();
                                            let save = save.clone();
                                            async move {
                                                let (Some(storage), Some(crc32)) = (storage, crc32)
                                                else {
                                                    return;
                                                };
                                                let bytes = match storage.save_dir(crc32).await {
                                                    Ok(dir) => storage::read(&dir, &save)
                                                        .await
                                                        .ok()
                                                        .flatten(),
                                                    Err(_) => None,
                                                };
                                                match bytes {
                                                    Some(bytes) => crate::web::download_bytes(&save, &bytes),
                                                    None => flash(
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
        // The footer: what's on the link port, then Play (grayed out
        // until a game is picked). Launch failures flash in between.
        div { class: "launch-bar",
                // The peripheral is plugged in before power-on, so the
                // choice lives here, at launch. It also decides what a
                // room created from this session announces to peers.
                {
                    let link = config.read().link;
                    rsx! {
                        div { class: "port-pick",
                            span { class: "port-pick-label", "Link port" }
                            div { class: "tabs", role: "group",
                                button {
                                    class: if link == crate::session::LinkKind::Cable { "btn tab active" } else { "btn tab" },
                                    title: "Boot with the multi-cable on the link port",
                                    onclick: move |_| config.with_mut(|c| c.link = crate::session::LinkKind::Cable),
                                    icons::Cable {}
                                    "Cable"
                                }
                                button {
                                    class: if link == crate::session::LinkKind::Wireless { "btn tab active" } else { "btn tab" },
                                    title: "Boot with the wireless adapter on the link port",
                                    onclick: move |_| config.with_mut(|c| c.link = crate::session::LinkKind::Wireless),
                                    icons::Wifi {}
                                    "Wireless"
                                }
                            }
                        }
                    }
                }
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
                                    Some(name) => match storage.save_dir(info.crc32).await {
                                        Ok(dir) => {
                                            storage::read(&dir, name).await.ok().flatten()
                                        }
                                        Err(_) => None,
                                    },
                                    None => None,
                                };
                                // SRAM writes back into the picked save (in
                                // the game's own directory), or a fresh
                                // "<name>.sav" the first time it persists.
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
