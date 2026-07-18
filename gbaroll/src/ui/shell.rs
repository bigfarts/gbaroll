//! The root component: global state wiring (config, runtime, OPFS
//! resources), the session-view swap, and the tab shell. Action
//! feedback is inline with whatever triggered it, not a global bar.

use dioxus::html::HasFileData;
use dioxus::prelude::*;

use super::{icons, play, session_view, settings, use_ctx, Ctx, Tab};
use crate::config::Config;
use crate::library::{self, Library};
use crate::nointro::DatIndex;
use crate::runtime::{Runtime, SESSION_EPOCH};
use crate::storage::Storage;

const STYLE: Asset = asset!("/assets/style.css");
/// Unhashed: the manifest names the icons by path, so both sides keep
/// stable names (the shell's own tags resolve through the assets).
const MANIFEST: Asset = asset!(
    "/assets/manifest.webmanifest",
    AssetOptions::builder().with_hash_suffix(false)
);
const ICON: Asset = asset!("/assets/icon-192.png", AssetOptions::builder().with_hash_suffix(false));
const APPLE_TOUCH_ICON: Asset =
    asset!("/assets/icon-180.png", AssetOptions::builder().with_hash_suffix(false));
/// Referenced only from inside the manifest, so nothing in Rust reads
/// it — `#[used]` keeps it in the bundle.
#[used]
static ICON_512: Asset =
    asset!("/assets/icon-512.png", AssetOptions::builder().with_hash_suffix(false));
/// The brand fonts, named by the stylesheet's @font-face urls —
/// unhashed and `#[used]` for the same reason as the manifest icons.
#[used]
static FONT_NEUE: Asset = asset!(
    "/assets/ComicNeue-Regular.woff2",
    AssetOptions::builder().with_hash_suffix(false)
);
#[used]
static FONT_NEUE_BOLD: Asset = asset!(
    "/assets/ComicNeue-Bold.woff2",
    AssetOptions::builder().with_hash_suffix(false)
);
#[used]
static FONT_MONO: Asset =
    asset!("/assets/ComicMono.ttf", AssetOptions::builder().with_hash_suffix(false));
#[used]
static FONT_MONO_BOLD: Asset = asset!(
    "/assets/ComicMono-Bold.ttf",
    AssetOptions::builder().with_hash_suffix(false)
);

#[component]
pub fn App() -> Element {
    let mut config = use_hook(|| Signal::new(Config::load()));
    let runtime = use_hook(Runtime::install);
    // Bumped to rescan the library after imports/deletes.
    let library_rev = use_signal(|| 0u64);
    // The last pick is remembered across loads.
    let selected_game = use_signal(|| config.peek().last_game);
    let selected_save = use_signal(|| Option::<String>::None);

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
                Some(s) => crate::nointro::load(&s, &crate::web::FALLBACK_DAT.to_string()).await,
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
                    let (saves, adopted) = library::scan_saves(&s, &lib).await;
                    // A migrated legacy save was the old auto-pick for
                    // its game, so it becomes the game's default.
                    if !adopted.is_empty() {
                        config.with_mut(|c| {
                            for (crc32, name) in adopted {
                                c.default_saves.entry(crc32).or_insert(name);
                            }
                        });
                    }
                    Some((lib, saves))
                }
                _ => None,
            }
        }
    });

    use_context_provider(|| Ctx {
        runtime: runtime.clone(),
        config,
        library_rev,
        storage,
        dat,
        library,
        selected_game,
        selected_save,
    });

    // The runtime persists SRAM (and netplay recordings) into OPFS.
    {
        let runtime = runtime.clone();
        use_effect(move || {
            if let Some(Some(storage)) = storage.read().clone() {
                runtime.borrow_mut().set_storage(storage);
            }
        });
    }

    // Persist every config edit; the screens just mutate the signal.
    use_effect(move || config.read().save());

    // Keep the runtime fed with the settings it consumes: the master
    // volume and the input mapping (which otherwise stays at default).
    {
        let runtime = runtime.clone();
        use_effect(move || {
            let c = config.read();
            let mut rt = runtime.borrow_mut();
            rt.set_volume(c.volume);
            rt.mapping = c.mapping.clone();
        });
    }

    // A running session — or its still-undismissed end — swaps the tab
    // shell for the fullscreen session view.
    let in_session = {
        let _ = SESSION_EPOCH.read();
        let rt = runtime.borrow();
        rt.shared().is_some() || rt.last_end().is_some()
    };

    rsx! {
        document::Stylesheet { href: STYLE }
        // App-frame viewport: no pinch zoom, edge-to-edge on notched
        // screens, browser chrome tinted to match.
        document::Meta {
            name: "viewport",
            // maximum-scale=1 stops iOS Safari's zoom-into-focused-field
            // jump without oversizing fonts.
            content: "width=device-width, initial-scale=1, maximum-scale=1, viewport-fit=cover, user-scalable=no",
        }
        document::Meta { name: "theme-color", content: "#16161e" }
        // Installable app: manifest plus icons (Safari ignores the
        // manifest's icons and wants its own link).
        document::Link { rel: "manifest", href: MANIFEST }
        document::Link { rel: "icon", href: ICON }
        document::Link { rel: "apple-touch-icon", href: APPLE_TOUCH_ICON }
        if in_session {
            session_view::SessionView {}
        } else {
            Shell {}
        }
    }
}

#[component]
fn Shell() -> Element {
    let Ctx {
        mut config,
        storage,
        mut library_rev,
        library,
        selected_game,
        ..
    } = use_ctx();
    let mut tab = use_signal(Tab::default);
    let current = tab();
    let nick = config.read().nick.clone();
    // True while a file drag hovers the content area (the drop cue).
    let mut drop_hover = use_signal(|| false);

    rsx! {
        document::Title { "gbaroll" }
        div {
            class: "shell",
            // A stray file drop must not navigate away from the app
            // (imports go through the explicit pickers).
            ondragover: move |evt| evt.prevent_default(),
            ondrop: move |evt| evt.prevent_default(),
            header { class: "topbar",
                div { class: "brand",
                    img { class: "logo", src: ICON, alt: "" }
                    h1 { "gbaroll" }
                }
                nav { class: "tabs",
                    button {
                        class: "btn tab",
                        class: if current == Tab::Play { "active" },
                        onclick: move |_| tab.set(Tab::Play),
                        icons::Gamepad2 {}
                        "Play"
                    }
                    button {
                        class: "btn tab",
                        class: if current == Tab::Settings { "active" },
                        onclick: move |_| tab.set(Tab::Settings),
                        icons::Sliders {}
                        "Settings"
                    }
                }
                // Identity lives on the main page: this is the name the
                // roster shows to other players.
                label { class: "identity",
                    icons::User {}
                    input {
                        value: "{nick}",
                        placeholder: "nickname",
                        spellcheck: "false",
                        autocomplete: "off",
                        oninput: move |evt: FormEvent| {
                            config.with_mut(|c| c.nick = evt.value())
                        },
                    }
                }
            }
            // The whole content area is one drop target: dropped files
            // import wherever they land, sorted by extension, and the
            // outcome flashes on whichever pane(s) received something.
            main {
                class: if current == Tab::Play { "play-main" } else { "settings-main" },
                class: if drop_hover() { "dropping" },
                ondragover: move |evt: DragEvent| {
                    evt.prevent_default();
                    if !*drop_hover.peek() {
                        drop_hover.set(true);
                    }
                },
                ondragleave: move |_| {
                    if *drop_hover.peek() {
                        drop_hover.set(false);
                    }
                },
                ondrop: move |evt: DragEvent| {
                    evt.prevent_default();
                    drop_hover.set(false);
                    let storage = storage.read().clone().flatten();
                    // Dropped saves land in the selected game's
                    // namespace — only if that game is really in the
                    // library (the remembered pick can be stale).
                    let dest = selected_game.read().filter(|crc32| {
                        library
                            .read()
                            .as_ref()
                            .and_then(|v| v.as_ref())
                            .is_some_and(|(lib, _)| lib.by_crc32(*crc32).is_some())
                    });
                    let files = evt.files();
                    async move {
                        let Some(storage) = storage else { return };
                        let counts = crate::web::import_files(&storage, files, dest).await;
                        play::import_flashes(counts, play::ROM_IMPORT_FLASH.signal());
                        *library_rev.write() += 1;
                    }
                },
                if current == Tab::Play {
                    play::PlayScreen {}
                } else {
                    settings::SettingsScreen {}
                }
            }
        }
    }
}
