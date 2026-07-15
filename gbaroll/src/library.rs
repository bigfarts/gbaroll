//! The ROM library: a scan of the roms directory, keyed by CRC32 —
//! which is how sessions and replays name the ROM each side runs.
//! No-Intro DATs (see [`crate::nointro`]) supply the display names,
//! matched by CRC32; ROMs missing from every DAT fall back to their
//! header title.

use std::path::{Path, PathBuf};

use crate::nointro::DatIndex;

pub const ROM_EXTENSIONS: &[&str] = &["gba", "srl", "agb"];

#[derive(Debug, Clone)]
pub struct RomInfo {
    pub path: PathBuf,
    /// The ROM header's internal title (0xA0..0xAC, ASCII, NUL-padded).
    pub title: String,
    /// The ROM header's game code (0xAC..0xB0).
    pub code: String,
    pub crc32: u32,
    pub size: u64,
    /// The No-Intro name for this ROM, when a loaded DAT knows its CRC.
    pub dat_name: Option<String>,
}

impl RomInfo {
    /// The name to show for this ROM: the No-Intro name when known,
    /// else the header title.
    pub fn display_name(&self) -> &str {
        self.dat_name.as_deref().unwrap_or(&self.title)
    }
}

#[derive(Default)]
pub struct Library {
    pub roms: Vec<RomInfo>,
}

impl Library {
    pub fn scan(roms_dir: &Path, dats: &DatIndex) -> Library {
        let mut roms = Vec::new();
        for entry in walkdir::WalkDir::new(roms_dir)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let ext_ok = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| ROM_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
                .unwrap_or(false);
            if !ext_ok {
                continue;
            }
            match load_rom_info(path) {
                Ok(mut info) => {
                    info.dat_name = dats.lookup(info.crc32).map(|n| n.to_string());
                    roms.push(info);
                }
                Err(e) => log::warn!("skipping {}: {e}", path.display()),
            }
        }
        roms.sort_by(|a, b| {
            let an = a.display_name().to_ascii_lowercase();
            let bn = b.display_name().to_ascii_lowercase();
            an.cmp(&bn).then_with(|| a.path.cmp(&b.path))
        });
        Library { roms }
    }

    pub fn by_crc32(&self, crc32: u32) -> Option<&RomInfo> {
        self.roms.iter().find(|r| r.crc32 == crc32)
    }
}

fn header_str(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    bytes[..end]
        .iter()
        .map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '?' })
        .collect()
}

fn load_rom_info(path: &Path) -> anyhow::Result<RomInfo> {
    let bytes = std::fs::read(path)?;
    if bytes.len() < 0xc0 {
        anyhow::bail!("too small to be a GBA ROM");
    }
    Ok(RomInfo {
        path: path.to_path_buf(),
        title: header_str(&bytes[0xa0..0xac]),
        code: header_str(&bytes[0xac..0xb0]),
        crc32: crc32fast::hash(&bytes),
        size: bytes.len() as u64,
        dat_name: None,
    })
}

/// Read a library ROM's bytes back for booting a link.
pub fn read_rom(info: &RomInfo) -> anyhow::Result<Vec<u8>> {
    let bytes = std::fs::read(&info.path)?;
    if crc32fast::hash(&bytes) != info.crc32 {
        anyhow::bail!("{} changed on disk since it was scanned", info.path.display());
    }
    Ok(bytes)
}

/// Files offered by the save picker.
pub fn list_saves(saves_dir: &Path) -> Vec<PathBuf> {
    let mut saves: Vec<PathBuf> = walkdir::WalkDir::new(saves_dir)
        .max_depth(2)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| matches!(e.to_ascii_lowercase().as_str(), "sav" | "sa1" | "srm"))
                .unwrap_or(false)
        })
        .collect();
    saves.sort();
    saves
}
