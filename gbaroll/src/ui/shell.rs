//! The root component: global state wiring (config, runtime, OPFS
//! resources), the session-view swap, and the tab shell with its
//! notice bar.

use dioxus::prelude::*;

use super::{icons, play, session_view, settings, use_ctx, Ctx, Tab};
use crate::config::Config;
use crate::library::{self, Library};
use crate::nointro::DatIndex;
use crate::runtime::{Runtime, SESSION_EPOCH};
use crate::storage::Storage;

const STYLE: Asset = asset!("/assets/style.css");

#[component]
pub fn App() -> Element {
    let config = use_hook(|| Signal::new(Config::load()));
    let runtime = use_hook(Runtime::install);
    let notice = use_signal(|| Option::<String>::None);
    // Bumped to rescan the library after imports/deletes.
    let library_rev = use_signal(|| 0u64);
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
                    let saves = library::list_saves(&s).await;
                    Some((lib, saves))
                }
                _ => None,
            }
        }
    });

    use_context_provider(|| Ctx {
        runtime: runtime.clone(),
        config,
        notice,
        library_rev,
        storage,
        dat,
        library,
        selected_save,
    });

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
        if in_session {
            session_view::SessionView {}
        } else {
            Shell {}
        }
    }
}

#[component]
fn Shell() -> Element {
    let mut notice = use_ctx().notice;
    let mut tab = use_signal(Tab::default);
    let current = tab();

    rsx! {
        document::Title { "gbaroll" }
        div { class: "shell",
            header { class: "topbar",
                div { class: "brand",
                    h1 { "gbaroll" }
                    span { class: "tagline", "GBA link play, without the cable" }
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
            }
            if let Some(message) = notice.read().clone() {
                div { class: "notice",
                    span { "{message}" }
                    button {
                        class: "btn ghost icon-btn",
                        title: "Dismiss",
                        onclick: move |_| notice.set(None),
                        icons::X {}
                    }
                }
            }
            main {
                if current == Tab::Play {
                    play::PlayScreen {}
                } else {
                    settings::SettingsScreen {}
                }
            }
        }
    }
}
