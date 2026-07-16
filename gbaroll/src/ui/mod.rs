//! The Dioxus component tree: a Play/Settings tab shell while idle and
//! a fullscreen session view while a game runs (or its end is still on
//! screen). A functional port of the retired native client's iced
//! screens (`native-final` tag), reshaped for the DOM.

mod cable;
mod icons;
mod play;
mod session_view;
mod settings;
mod shell;
mod telemetry;

pub use shell::App;

use std::cell::RefCell;
use std::rc::Rc;

use dioxus::prelude::*;

use crate::config::Config;
use crate::library::Library;
use crate::nointro::DatIndex;
use crate::runtime::Runtime;
use crate::storage::Storage;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Play,
    Settings,
}

/// Handles shared by every screen, provided once by [`shell::App`].
/// Everything but the runtime handle is `Copy`.
#[derive(Clone)]
struct Ctx {
    runtime: Rc<RefCell<Runtime>>,
    config: Signal<Config>,
    /// The notice bar's message (errors/status); `None` hides the bar.
    notice: Signal<Option<String>>,
    /// Bumped to rescan the library after imports and deletes.
    library_rev: Signal<u64>,
    /// `Some(None)` when the browser has no OPFS.
    storage: Resource<Option<Storage>>,
    dat: Resource<DatIndex>,
    /// Library scan + save list; `None` until OPFS and the DAT are up.
    library: Resource<Option<(Library, Vec<String>)>>,
    /// The save picker's choice for the next boot (`None` = fresh).
    selected_save: Signal<Option<String>>,
}

fn use_ctx() -> Ctx {
    use_context::<Ctx>()
}
