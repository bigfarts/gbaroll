//! The ROM library: a scan of OPFS `roms/`, keyed by CRC32 — which is
//! how sessions and replays name the ROM each side runs. No-Intro DATs
//! (see [`crate::nointro`]) supply the display names, matched by CRC32;
//! ROMs missing from every DAT fall back to their header title.

use crate::nointro::DatIndex;
use crate::storage::{self, Storage};

pub const ROM_EXTENSIONS: &[&str] = &["gba", "srl", "agb"];
pub const SAVE_EXTENSIONS: &[&str] = &["sav", "sa1", "srm"];

pub fn has_extension(name: &str, extensions: &[&str]) -> bool {
    name.rsplit_once('.')
        .map(|(_, ext)| extensions.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

#[derive(Debug, Clone)]
pub struct RomInfo {
    /// File name inside OPFS `roms/`.
    pub file_name: String,
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

#[derive(Default, Clone)]
pub struct Library {
    pub roms: Vec<RomInfo>,
}

impl Library {
    pub async fn scan(storage: &Storage, dat: &DatIndex) -> Library {
        let mut roms = Vec::new();
        let files = match storage::list_files(storage.roms()).await {
            Ok(files) => files,
            Err(e) => {
                log::error!("couldn't list the ROM library: {e}");
                return Library::default();
            }
        };
        for (name, handle) in files {
            if !has_extension(&name, ROM_EXTENSIONS) {
                continue;
            }
            match storage::read_handle(&handle).await {
                Ok(bytes) => match rom_info(&name, &bytes) {
                    Ok(mut info) => {
                        info.dat_name = dat.lookup(info.crc32).map(|n| n.to_string());
                        roms.push(info);
                    }
                    Err(e) => log::warn!("skipping {name}: {e}"),
                },
                Err(e) => log::warn!("skipping {name}: {e}"),
            }
        }
        roms.sort_by(|a, b| {
            let an = a.display_name().to_ascii_lowercase();
            let bn = b.display_name().to_ascii_lowercase();
            an.cmp(&bn).then_with(|| a.file_name.cmp(&b.file_name))
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

/// The OPFS name an imported ROM is stored under: `CODE (crc32).gba`,
/// derived from the cartridge rather than whatever the picked file was
/// called. Same cartridge → same name, so re-imports overwrite.
pub fn normalized_file_name(info: &RomInfo) -> String {
    let code: String = info
        .code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let code = if code.is_empty() { "ROM".to_string() } else { code };
    format!("{} ({:08x}).gba", code, info.crc32)
}

/// The largest real GBA cartridge (32 MiB).
pub const MAX_ROM_SIZE: usize = 32 * 1024 * 1024;

/// Parse the header of an imported ROM. Also the import-time validator:
/// the cartridge header self-identifies with a fixed byte and a
/// checksum over 0xA0..=0xBC — the BIOS refuses carts without them, and
/// so do we. This is what keeps a mis-picked zip (or whatever else a
/// phone's file picker hands over) out of the library.
pub fn rom_info(file_name: &str, bytes: &[u8]) -> anyhow::Result<RomInfo> {
    if bytes.len() < 0xc0 {
        anyhow::bail!("too small to be a GBA ROM");
    }
    if bytes.len() > MAX_ROM_SIZE {
        anyhow::bail!(
            "{} MiB is larger than any GBA cartridge",
            bytes.len() / (1024 * 1024)
        );
    }
    if bytes[0xb2] != 0x96 {
        anyhow::bail!("not a GBA ROM (missing the header's fixed byte)");
    }
    let checksum = bytes[0xa0..=0xbc]
        .iter()
        .fold(0u8, |acc, b| acc.wrapping_sub(*b))
        .wrapping_sub(0x19);
    if checksum != bytes[0xbd] {
        anyhow::bail!("not a GBA ROM (bad header checksum)");
    }
    Ok(RomInfo {
        file_name: file_name.to_owned(),
        title: header_str(&bytes[0xa0..0xac]),
        code: header_str(&bytes[0xac..0xb0]),
        crc32: crc32fast::hash(bytes),
        size: bytes.len() as u64,
        dat_name: None,
    })
}

/// Read a library ROM's bytes back for booting a link.
pub async fn read_rom(storage: &Storage, info: &RomInfo) -> anyhow::Result<Vec<u8>> {
    let bytes = storage::read(storage.roms(), &info.file_name)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .ok_or_else(|| anyhow::anyhow!("{} disappeared from the library", info.file_name))?;
    if crc32fast::hash(&bytes) != info.crc32 {
        anyhow::bail!("{} changed since it was scanned", info.file_name);
    }
    Ok(bytes)
}

/// File names offered by the save picker (OPFS `saves/`).
pub async fn list_saves(storage: &Storage) -> Vec<String> {
    match storage::list_files(storage.saves()).await {
        Ok(files) => files
            .into_iter()
            .map(|(name, _)| name)
            .filter(|n| has_extension(n, SAVE_EXTENSIONS))
            .collect(),
        Err(e) => {
            log::error!("couldn't list saves: {e}");
            Vec::new()
        }
    }
}
