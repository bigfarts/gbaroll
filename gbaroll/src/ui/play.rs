//! The Play tab: the OPFS ROM library (import, search, per-row play
//! and delete), the save picker applied to the next boot, and a
//! developer corner for the game database and the test ROM.

use dioxus::prelude::*;

use super::{icons, use_ctx, Ctx};
use crate::library::{self, RomInfo};
use crate::storage;

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
    // Deleting a ROM is irreversible, so it takes two clicks: arm on
    // the row, then confirm in place.
    let mut pending_delete = use_signal(|| Option::<String>::None);

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
                || r.file_name.to_ascii_lowercase().contains(&needle)
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
                                    code { "{rom.file_name}" }
                                    " · {rom.code} · "
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
                                        title: "Delete {rom.file_name}",
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
                            code { "{save}" }
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
