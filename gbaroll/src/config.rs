//! App configuration, one small JSON blob in localStorage. The dirs the
//! native client exposed are gone — ROMs and saves live in OPFS (see
//! `storage`), which has no user-facing paths.

use serde::{Deserialize, Serialize};

use crate::platform::input::Mapping;

const KEY: &str = "gbaroll.config";

/// The signaling server every build points at; override per page load
/// with `?signaling_server_addr=…` (there is no settings knob).
pub const DEFAULT_SIGNALING: &str = "wss://gbaroll-signaling.farts.fyi";

/// The signaling server URL for this page load.
pub fn signaling_server() -> String {
    web_sys::window()
        .and_then(|w| w.location().search().ok())
        .and_then(|s| web_sys::UrlSearchParams::new_with_str(&s).ok())
        .and_then(|p| p.get("signaling_server_addr"))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_SIGNALING.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub nick: String,
    /// How many ticks behind the input frontier to present (the input
    /// delay / rollback depth tradeoff), adjustable live in-session.
    pub present_delay: u32,
    /// Master volume, 0.0..=1.0.
    pub volume: f32,
    /// Snap the game image to integer multiples of 240x160.
    pub integer_scaling: bool,
    /// The library's last-picked game (CRC32), restored on load.
    pub last_game: Option<u32>,
    pub mapping: Mapping,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            nick: "player".to_string(),
            present_delay: 2,
            volume: 1.0,
            integer_scaling: true,
            last_game: None,
            mapping: Mapping::default(),
        }
    }
}

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

impl Config {
    pub fn load() -> Config {
        local_storage()
            .and_then(|s| s.get_item(KEY).ok()?)
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(storage) = local_storage() else {
            return;
        };
        match serde_json::to_string(self) {
            Ok(json) => {
                if storage.set_item(KEY, &json).is_err() {
                    log::error!("failed to write config to localStorage");
                }
            }
            Err(e) => log::error!("failed to serialize config: {e}"),
        }
    }
}
