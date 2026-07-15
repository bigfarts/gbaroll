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
    /// No-Intro DAT files (Logiqx XML or ClrMamePro) live here; they
    /// supply the library's display names.
    pub dats_dir: PathBuf,
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
    /// Show the connection stats HUD during netplay.
    pub show_hud: bool,
    pub mapping: Mapping,
}

impl Default for Config {
    fn default() -> Self {
        let data = project_dirs()
            .map(|d| d.data_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        Config {
            nick: "player".to_string(),
            roms_dir: data.join("roms"),
            saves_dir: data.join("saves"),
            replays_dir: data.join("replays"),
            dats_dir: data.join("dats"),
            signaling_server: "ws://127.0.0.1:1984".to_string(),
            present_delay: 2,
            volume: 1.0,
            integer_scaling: true,
            show_hud: true,
            mapping: Mapping::default(),
        }
    }
}

fn project_dirs() -> Option<directories_next::ProjectDirs> {
    directories_next::ProjectDirs::from("com", "gbaroll", "gbaroll")
}

fn config_path() -> Option<PathBuf> {
    project_dirs().map(|d| d.config_dir().join("config.json"))
}

impl Config {
    pub fn load() -> Config {
        let mut config: Config = config_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        config.ensure_dirs();
        config
    }

    pub fn save(&self) {
        let Some(path) = config_path() else { return };
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
        for dir in [&self.roms_dir, &self.saves_dir, &self.replays_dir, &self.dats_dir] {
            let _ = std::fs::create_dir_all(dir);
        }
    }
}
