use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::platform::input::Mapping;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub nick: String,
    pub roms_dir: PathBuf,
    pub saves_dir: PathBuf,
    pub replays_dir: PathBuf,
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

/// Folder under the user's Documents that holds ROMs, saves, and
/// replays — visible and easy to drop files into, the way tango keeps a
/// `Tango` folder there.
const DATA_DIR_NAME: &str = "gbaroll";

impl Default for Config {
    fn default() -> Self {
        // Fall back to ./gbaroll-data if the Documents lookup fails so the
        // app still runs rather than panicking.
        let data = directories_next::UserDirs::new()
            .and_then(|u| u.document_dir().map(|d| d.join(DATA_DIR_NAME)))
            .unwrap_or_else(|| PathBuf::from("./gbaroll-data"));
        Config {
            nick: "player".to_string(),
            roms_dir: data.join("roms"),
            saves_dir: data.join("saves"),
            replays_dir: data.join("replays"),
            signaling_server: "ws://127.0.0.1:1984".to_string(),
            present_delay: 2,
            volume: 1.0,
            integer_scaling: true,
            mapping: Mapping::default(),
        }
    }
}

fn project_dirs() -> Option<directories_next::ProjectDirs> {
    directories_next::ProjectDirs::from("com", "gbaroll", "gbaroll")
}

/// gbaroll-owned configuration storage. User-managed data such as ROMs
/// stays in Documents; internal support files live beside `config.json`.
pub fn config_dir() -> PathBuf {
    project_dirs()
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("./gbaroll-data/config"))
}

fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

impl Config {
    pub fn load() -> Config {
        let mut config: Config = std::fs::read(config_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        config.ensure_dirs();
        config
    }

    pub fn save(&self) {
        let path = config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_vec_pretty(self) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(&path, bytes) {
                    log::error!("failed to write config: {e}");
                }
            }
            Err(e) => log::error!("failed to serialize config: {e}"),
        }
    }

    pub fn ensure_dirs(&mut self) {
        for dir in [&self.roms_dir, &self.saves_dir, &self.replays_dir] {
            let _ = std::fs::create_dir_all(dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dat_storage_is_not_user_configurable() {
        let serialized = serde_json::to_value(Config::default()).unwrap();
        assert!(serialized.get("dats_dir").is_none());

        // Old configs remain loadable; serde ignores the retired field.
        let migrated: Config = serde_json::from_value(serde_json::json!({
            "dats_dir": "/previous/user/chosen/path"
        }))
        .unwrap();
        assert_eq!(migrated.roms_dir, Config::default().roms_dir);
    }
}
