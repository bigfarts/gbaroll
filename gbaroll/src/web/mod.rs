//! The browser client's UI shell. M3 scope: the OPFS-backed library —
//! import ROMs/saves (file picker; everything stays in the browser's
//! origin-private storage), scan + No-Intro naming, per-ROM play with
//! a save picker, delete, save export. The polished component tree
//! lands with M4.

use dioxus::prelude::*;
use wasm_bindgen::JsCast;

use crate::config::Config;
use crate::library::{self, Library, SAVE_EXTENSIONS};
use crate::nointro::DatIndex;
use crate::runtime::{Runtime, FRAME_REV, SESSION_EPOCH};
use crate::storage::{self, Storage};

const WORKLET_JS: Asset = asset!("/assets/audio-worklet.js");
/// Bundled No-Intro snapshot for offline/first load.
const FALLBACK_DAT: Asset = asset!("/assets/nointro-fallback.dat");

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
async fn boot(runtime: std::rc::Rc<std::cell::RefCell<Runtime>>, rom: Vec<u8>, save: Option<Vec<u8>>) {
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
    if let Err(e) = runtime.borrow_mut().start_local(rom, save) {
        log::error!("couldn't start session: {e:#}");
    }
}

/// Import picked files into OPFS, routed by extension (ROMs vs saves).
/// Returns (roms, saves, skipped) counts.
async fn import_files(storage: &Storage, files: Vec<dioxus::html::FileData>) -> (u32, u32, u32) {
    let (mut roms, mut saves, mut skipped) = (0, 0, 0);
    for file in files {
        let name = file.name();
        let bytes = match file.read_bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => {
                log::error!("couldn't read {name}: {e:?}");
                skipped += 1;
                continue;
            }
        };
        if library::has_extension(&name, library::ROM_EXTENSIONS) {
            if let Err(e) = library::rom_info(&name, &bytes) {
                log::warn!("not importing {name}: {e}");
                skipped += 1;
                continue;
            }
            match storage::write(storage.roms(), &name, &bytes).await {
                Ok(()) => roms += 1,
                Err(e) => {
                    log::error!("couldn't import {name}: {e}");
                    skipped += 1;
                }
            }
        } else if library::has_extension(&name, SAVE_EXTENSIONS) {
            // GBA flash tops out at 128 KiB; leave headroom for
            // emulator save footers.
            if bytes.len() > 512 * 1024 {
                log::warn!("not importing {name}: save file too large");
                skipped += 1;
                continue;
            }
            match storage::write(storage.saves(), &name, &bytes).await {
                Ok(()) => saves += 1,
                Err(e) => {
                    log::error!("couldn't import {name}: {e}");
                    skipped += 1;
                }
            }
        } else {
            log::warn!("not importing {name}: unrecognized extension");
            skipped += 1;
        }
    }
    (roms, saves, skipped)
}

/// Offer a byte blob as a download (save export).
fn download_bytes(name: &str, bytes: &[u8]) {
    let array = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes).buffer());
    let Ok(blob) = web_sys::Blob::new_with_buffer_source_sequence(&array) else {
        return;
    };
    let Ok(url) = web_sys::Url::create_object_url_with_blob(&blob) else {
        return;
    };
    let document = web_sys::window().unwrap().document().unwrap();
    if let Ok(a) = document.create_element("a") {
        let a: web_sys::HtmlAnchorElement = a.unchecked_into();
        a.set_href(&url);
        a.set_download(name);
        a.click();
    }
    let _ = web_sys::Url::revoke_object_url(&url);
}

fn app() -> Element {
    let runtime = use_hook(Runtime::install);
    let config = use_hook(|| Signal::new(Config::load()));
    let mut notice = use_signal(String::new);
    // Bumped to rescan the library after imports/deletes/DAT updates.
    let mut library_rev = use_signal(|| 0u64);

    let storage = use_resource(|| async {
        match Storage::open().await {
            Ok(s) => Some(s),
            Err(e) => {
                log::error!("OPFS unavailable: {e}");
                None
            }
        }
    });

    let dat = use_resource(move || {
        let storage = storage.read().clone();
        async move {
            match storage.flatten() {
                Some(s) => crate::nointro::load(&s, &FALLBACK_DAT.to_string()).await,
                None => DatIndex::default(),
            }
        }
    });

    let library = use_resource(move || {
        let _ = library_rev.read();
        let storage = storage.read().clone();
        let dat = dat.read().clone();
        async move {
            match (storage.flatten(), dat) {
                (Some(s), Some(d)) => {
                    let lib = Library::scan(&s, &d).await;
                    let saves = library::list_saves(&s).await;
                    Some((lib, saves))
                }
                _ => None,
            }
        }
    });

    let mut selected_save = use_signal(|| Option::<String>::None);

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

    // Volume follows config.
    {
        let runtime = runtime.clone();
        use_effect(move || {
            runtime.borrow().set_volume(config.read().volume);
        });
    }

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

    let lib_view = library.read();
    let (roms, saves) = match lib_view.as_ref() {
        Some(Some((lib, saves))) => (lib.roms.clone(), saves.clone()),
        _ => (Vec::new(), Vec::new()),
    };
    let dat_names = dat.read().as_ref().map(|d| d.len()).unwrap_or(0);

    rsx! {
        h1 { "gbaroll" }
        p {
            "Everything stays in this browser (origin-private storage) — nothing uploads. "
            "Arrows move · Z/X = A/B · A/S = L/R · Enter/Space = Start/Select · hold LShift = fast-forward."
        }
        div {
            label {
                "Import ROMs (.gba/.srl/.agb) and saves (.sav/.sa1/.srm): "
                input {
                    r#type: "file",
                    accept: ".gba,.agb,.srl,.sav,.sa1,.srm",
                    multiple: true,
                    onchange: move |evt| {
                        let storage = storage.read().clone().flatten();
                        async move {
                            let Some(storage) = storage else { return };
                            let (r, s, skipped) = import_files(&storage, evt.files()).await;
                            notice.set(format!("imported {r} ROM(s), {s} save(s), skipped {skipped}"));
                            *library_rev.write() += 1;
                        }
                    },
                }
            }
        }
        if !notice.read().is_empty() {
            p { em { {notice} } }
        }
        h2 { "Library" }
        p {
            "{roms.len()} ROM(s) · {dat_names} No-Intro name(s) "
            button {
                onclick: move |_| {
                    let storage = storage.read().clone().flatten();
                    async move {
                        let Some(storage) = storage else { return };
                        match crate::nointro::fetch_gba_dat(&storage).await {
                            Ok(n) => notice.set(format!("downloaded the No-Intro database ({n} names)")),
                            Err(e) => notice.set(format!("database download failed: {e:#}")),
                        }
                        *library_rev.write() += 1;
                    }
                },
                "Update game database"
            }
        }
        div {
            label {
                "Save for the next boot: "
                select {
                    onchange: move |evt| {
                        let v = evt.value();
                        selected_save.set(if v.is_empty() { None } else { Some(v) });
                    },
                    option { value: "", "(fresh save)" }
                    for save in saves.iter() {
                        option { value: "{save}", selected: selected_save.read().as_deref() == Some(save), "{save}" }
                    }
                }
            }
            for save in saves.iter() {
                button {
                    onclick: {
                        let save = save.clone();
                        move |_| {
                            let storage = storage.read().clone().flatten();
                            let save = save.clone();
                            async move {
                                let Some(storage) = storage else { return };
                                match storage::read(storage.saves(), &save).await {
                                    Ok(Some(bytes)) => download_bytes(&save, &bytes),
                                    _ => notice.set(format!("couldn't read {save}")),
                                }
                            }
                        }
                    },
                    "Export {save}"
                }
            }
        }
        table {
            for rom in roms.iter() {
                tr {
                    td { "{rom.display_name()}" }
                    td { code { "{rom.file_name}" } }
                    td { code { {format!("{:08x}", rom.crc32)} } }
                    td {
                        button {
                            onclick: {
                                let runtime = runtime.clone();
                                let file_name = rom.file_name.clone();
                                let info = rom.clone();
                                move |_| {
                                    let _ = &file_name;
                                    let runtime = runtime.clone();
                                    let info = info.clone();
                                    let storage = storage.read().clone().flatten();
                                    let save_name = selected_save.read().clone();
                                    spawn(async move {
                                        let Some(storage) = storage else { return };
                                        let rom = match library::read_rom(&storage, &info).await {
                                            Ok(b) => b,
                                            Err(e) => {
                                                notice.set(format!("{e:#}"));
                                                return;
                                            }
                                        };
                                        let save = match &save_name {
                                            Some(name) => storage::read(storage.saves(), name).await.ok().flatten(),
                                            None => None,
                                        };
                                        boot(runtime, rom, save).await;
                                    });
                                }
                            },
                            "Play"
                        }
                    }
                    td {
                        button {
                            onclick: {
                                let file_name = rom.file_name.clone();
                                move |_| {
                                    let storage = storage.read().clone().flatten();
                                    let file_name = file_name.clone();
                                    async move {
                                        let Some(storage) = storage else { return };
                                        if let Err(e) = storage::delete(storage.roms(), &file_name).await {
                                            notice.set(format!("couldn't delete {file_name}: {e}"));
                                        }
                                        *library_rev.write() += 1;
                                    }
                                }
                            },
                            "Delete"
                        }
                    }
                }
            }
        }
        div {
            button {
                onclick: {
                    let runtime = runtime.clone();
                    move |_| {
                        let runtime = runtime.clone();
                        spawn(async move { boot(runtime, mgba_siolink::testrom::build(), None).await; });
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
            button {
                onclick: {
                    let runtime = runtime.clone();
                    move |_| runtime.borrow_mut().close_session()
                },
                "Quit game"
            }
            label {
                "Volume "
                input {
                    r#type: "range",
                    min: "0",
                    max: "100",
                    value: "{(config.read().volume * 100.0) as u32}",
                    oninput: move |evt: FormEvent| {
                        if let Ok(v) = evt.value().parse::<f32>() {
                            let mut c = config.clone();
                            c.write().volume = v / 100.0;
                            c.read().save();
                        }
                    },
                }
            }
        }
        p { {status()} }
        canvas { id: "framebuffer", width: "720", height: "480" }
    }
}
