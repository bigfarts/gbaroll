//! Browser bootstrap and platform glue: the wasm entry point, plus the
//! gesture-gated boot, OPFS import, and save-export helpers the UI
//! screens call into. The component tree itself lives in `crate::ui`.

use dioxus::prelude::*;
use wasm_bindgen::JsCast;

use crate::library::{self, SAVE_EXTENSIONS};
use crate::runtime::Runtime;
use crate::storage::{self, Storage};

const WORKLET_JS: Asset = asset!("/assets/audio-worklet.js");
/// Bundled No-Intro snapshot for offline/first load.
pub const FALLBACK_DAT: Asset = asset!("/assets/nointro-fallback.dat");

/// The C shim's clock (mgba's `gettimeofday` for savestate stamps).
#[no_mangle]
pub extern "C" fn gbaroll_now_unix_ms() -> f64 {
    js_sys::Date::now()
}

pub fn main() {
    console_error_panic_hook::set_once();
    let _ = console_log::init_with_level(log::Level::Info);
    mgba::log::install_default_logger();
    dioxus::launch(crate::ui::App);
}

/// Ensure the audio sink exists (must run within a user gesture), then
/// boot the ROM. A missing sink degrades to silence rather than failing
/// the boot.
pub async fn boot(
    runtime: std::rc::Rc<std::cell::RefCell<Runtime>>,
    rom: Vec<u8>,
    save: Option<Vec<u8>>,
    save_file: Option<String>,
) -> anyhow::Result<()> {
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
    runtime.borrow_mut().start_local(rom, save, save_file)
}

/// Import picked files into OPFS, routed by extension (ROMs vs saves).
/// Returns (roms, saves, skipped) counts.
pub async fn import_files(storage: &Storage, files: Vec<dioxus::html::FileData>) -> (u32, u32, u32) {
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
            let info = match library::rom_info(&name, &bytes) {
                Ok(info) => info,
                Err(e) => {
                    log::warn!("not importing {name}: {e}");
                    skipped += 1;
                    continue;
                }
            };
            // The stored name is normalized to the cartridge, not the
            // picked file: "CODE (crc32).gba". Re-importing the same
            // ROM overwrites itself instead of piling up copies, and
            // the UI never needs to show a filename.
            let stored = library::normalized_file_name(&info);
            match storage::write(storage.roms(), &stored, &bytes).await {
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
pub fn download_bytes(name: &str, bytes: &[u8]) {
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
