//! App configuration, one small JSON blob in localStorage. The dirs the
//! native client exposed are gone — ROMs and saves live in OPFS (see
//! `storage`), which has no user-facing paths.

use serde::{Deserialize, Serialize};

use crate::platform::input::Mapping;

const KEY: &str = "gbaroll.config";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub nick: String,
    /// WebSocket URL of the gbaroll signaling server (which also hands
    /// out the ICE servers for the mesh).
    pub signaling_server: String,
    /// How many ticks behind the input frontier to present (the input
    /// delay / rollback depth tradeoff), adjustable live in-session.
    pub present_delay: u32,
    /// Master volume, 0.0..=1.0.
    pub volume: f32,
    /// Snap the game image to integer multiples of 240x160.
    pub integer_scaling: bool,
    pub mapping: Mapping,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            nick: "player".to_string(),
            signaling_server: "ws://127.0.0.1:1984".to_string(),
            present_delay: 2,
            volume: 1.0,
            integer_scaling: true,
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
