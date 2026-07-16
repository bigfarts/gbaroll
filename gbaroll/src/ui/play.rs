//! The Play tab: the OPFS ROM library (import, search, per-row play
//! and delete), the save picker applied to the next boot, and a
//! developer corner for the game database and the test ROM.

use dioxus::prelude::*;

use super::{icons, use_ctx, Ctx};
use crate::library::{self, RomInfo};
use crate::storage;

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
        mut notice,
        mut library_rev,
        storage: storage_res,
        dat,
        library,
        mut selected_save,
        ..
    } = use_ctx();

    let mut search = use_signal(String::new);
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
    let dat_names = dat.read().as_ref().map(|d| d.len()).unwrap_or(0);

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
    let count = match roms.len() {
        1 => "1 game".to_string(),
        n => format!("{n} games"),
    };

    rsx! {
        section { class: "card",
            div { class: "card-head",
                div {
                    h2 { "Game library" }
                    p { class: "sub",
                        "{count} · everything stays in this browser (origin-private storage) — nothing uploads"
                    }
                }
                label { class: "btn primary file-btn",
                    icons::Upload {}
                    "Import…"
                    input {
                        r#type: "file",
                        accept: ".gba,.agb,.srl,.sav,.sa1,.srm",
                        multiple: true,
                        onchange: move |evt| {
                            let storage = storage_res.read().clone().flatten();
                            async move {
                                let Some(storage) = storage else { return };
                                let (r, s, skipped) =
                                    crate::web::import_files(&storage, evt.files()).await;
                                notice.set(Some(format!(
                                    "Imported {r} ROM(s) and {s} save(s), skipped {skipped}."
                                )));
                                *library_rev.write() += 1;
                            }
                        },
                    }
                }
            }
            input {
                class: "search",
                placeholder: "Search by title or code…",
                value: "{search}",
                oninput: move |evt: FormEvent| search.set(evt.value()),
            }
            if opfs_down {
                div { class: "empty",
                    p { "This browser doesn't offer origin-private storage; the library is unavailable." }
                }
            } else if !scanned {
                div { class: "empty", p { "Scanning the library…" } }
            } else if filtered.is_empty() {
                div { class: "empty",
                    icons::Gamepad2 {}
                    p { if roms.is_empty() { "No games yet" } else { "No games match your search" } }
                    p { class: "sub",
                        if roms.is_empty() { "Import .gba files to get started." } else { "Try a title or game code." }
                    }
                }
            } else {
                div { class: "rom-list",
                    for rom in filtered {
                        div { class: "rom-row",
                            div { class: "rom-name",
                                span { class: "game", "{rom.display_name()}" }
                                span { class: "rom-meta",
                                    "{rom.code} · "
                                    code { {format!("{:08x}", rom.crc32)} }
                                    " · {rom.size / 1024} KiB"
                                }
                            }
                            div { class: "rom-actions",
                                button {
                                    class: "btn primary",
                                    onclick: {
                                        let runtime = runtime.clone();
                                        let info = rom.clone();
                                        move |_| {
                                            let runtime = runtime.clone();
                                            let info = info.clone();
                                            let storage = storage_res.read().clone().flatten();
                                            let save_name = selected_save.read().clone();
                                            spawn(async move {
                                                let Some(storage) = storage else { return };
                                                let bytes = match library::read_rom(&storage, &info).await {
                                                    Ok(b) => b,
                                                    Err(e) => {
                                                        notice.set(Some(format!("{e:#}")));
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
                                                    notice.set(Some(format!("couldn't start session: {e:#}")));
                                                }
                                            });
                                        }
                                    },
                                    icons::Play {}
                                    "Play"
                                }
                                if pending_delete.read().as_deref() == Some(rom.file_name.as_str()) {
                                    button {
                                        class: "btn danger",
                                        onclick: {
                                            let file_name = rom.file_name.clone();
                                            move |_| {
                                                let storage = storage_res.read().clone().flatten();
                                                let file_name = file_name.clone();
                                                async move {
                                                    let Some(storage) = storage else { return };
                                                    if let Err(e) =
                                                        storage::delete(storage.roms(), &file_name).await
                                                    {
                                                        notice.set(Some(format!(
                                                            "couldn't delete {file_name}: {e}"
                                                        )));
                                                    }
                                                    pending_delete.set(None);
                                                    *library_rev.write() += 1;
                                                }
                                            }
                                        },
                                        "Confirm"
                                    }
                                    button {
                                        class: "btn",
                                        onclick: move |_| pending_delete.set(None),
                                        "Cancel"
                                    }
                                } else {
                                    button {
                                        class: "btn ghost icon-btn",
                                        title: "Delete {rom.display_name()}",
                                        onclick: {
                                            let file_name = rom.file_name.clone();
                                            move |_| pending_delete.set(Some(file_name.clone()))
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
                    p { class: "sub", "The picked save applies to the next game you start." }
                }
            }
            div { class: "field",
                label { "Save for the next boot" }
                select {
                    onchange: move |evt| {
                        let v = evt.value();
                        selected_save.set(if v.is_empty() { None } else { Some(v) });
                    },
                    option { value: "", selected: selected_save.read().is_none(), "(fresh save)" }
                    for save in saves.iter() {
                        option {
                            value: "{save}",
                            selected: selected_save.read().as_deref() == Some(save.as_str()),
                            "{save}"
                        }
                    }
                }
            }
            if !saves.is_empty() {
                div { class: "save-list",
                    for save in saves.clone() {
                        div { class: "save-row",
                            if rename_target.read().as_deref() == Some(save.as_str()) {
                                // Renaming: edit the stem, keep the extension.
                                input {
                                    class: "rename",
                                    value: "{rename_value}",
                                    oninput: move |evt: FormEvent| rename_value.set(evt.value()),
                                }
                                code { {format!(".{}", ext_of(&save))} }
                                button {
                                    class: "btn primary",
                                    disabled: rename_value.read().trim().is_empty(),
                                    onclick: {
                                        let save = save.clone();
                                        move |_| {
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
                                                    Err(e) => notice.set(Some(format!(
                                                        "couldn't rename {save}: {e}"
                                                    ))),
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
                                    onclick: move |_| rename_target.set(None),
                                    "Cancel"
                                }
                            } else if pending_save_delete.read().as_deref() == Some(save.as_str()) {
                                code { "{save}" }
                                span { class: "spacer" }
                                button {
                                    class: "btn danger",
                                    onclick: {
                                        let save = save.clone();
                                        move |_| {
                                            let storage = storage_res.read().clone().flatten();
                                            let save = save.clone();
                                            async move {
                                                let Some(storage) = storage else { return };
                                                if let Err(e) =
                                                    storage::delete(storage.saves(), &save).await
                                                {
                                                    notice.set(Some(format!(
                                                        "couldn't delete {save}: {e}"
                                                    )));
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
                                    "Confirm"
                                }
                                button {
                                    class: "btn",
                                    onclick: move |_| pending_save_delete.set(None),
                                    "Cancel"
                                }
                            } else {
                                code { "{save}" }
                                span { class: "spacer" }
                                button {
                                    class: "btn ghost icon-btn",
                                    title: "Rename",
                                    onclick: {
                                        let save = save.clone();
                                        move |_| {
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
                                        move |_| {
                                            let storage = storage_res.read().clone().flatten();
                                            let save = save.clone();
                                            async move {
                                                let Some(storage) = storage else { return };
                                                match storage::read(storage.saves(), &save).await {
                                                    Ok(Some(bytes)) => crate::web::download_bytes(&save, &bytes),
                                                    _ => notice.set(Some(format!("couldn't read {save}"))),
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
                                        move |_| {
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
        div { class: "dev-corner",
            span { "Game database: {dat_names} No-Intro name(s)" }
            button {
                class: "btn ghost",
                onclick: move |_| {
                    let storage = storage_res.read().clone().flatten();
                    async move {
                        let mut dat = dat;
                        let Some(storage) = storage else { return };
                        match crate::nointro::fetch_gba_dat(&storage).await {
                            Ok(n) => notice.set(Some(format!(
                                "Downloaded the No-Intro database ({n} names)."
                            ))),
                            Err(e) => notice.set(Some(format!("database download failed: {e:#}"))),
                        }
                        dat.restart();
                    }
                },
                icons::RefreshCw {}
                "Update game database"
            }
            span { class: "spacer" }
            button {
                class: "btn ghost",
                onclick: {
                    let runtime = runtime.clone();
                    move |_| {
                        let runtime = runtime.clone();
                        spawn(async move {
                            if let Err(e) =
                                crate::web::boot(runtime, mgba_siolink::testrom::build(), None, None)
                                    .await
                            {
                                notice.set(Some(format!("couldn't start session: {e:#}")));
                            }
                        });
                    }
                },
                "Boot test ROM"
            }
        }
    }
}
