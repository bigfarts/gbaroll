//! The ROM library: a scan of the roms directory, keyed by CRC32 —
//! which is how sessions and replays name the ROM each side runs.
//! No-Intro DAT files supply the display names (matched by CRC32);
//! ROMs missing from every DAT fall back to their header title.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

// ---------------------------------------------------------------------------
// No-Intro DATs.

/// CRC32 → canonical game name, merged from every DAT in the dats
/// directory. Understands both distribution formats No-Intro offers:
/// Logiqx XML datafiles and ClrMamePro text.
#[derive(Default)]
pub struct DatIndex {
    by_crc32: HashMap<u32, String>,
    files: usize,
}

impl DatIndex {
    /// Load and merge every `*.dat` / `*.xml` under `dats_dir`. The
    /// first name seen for a CRC wins (overlapping DATs agree anyway).
    pub fn load_dir(dats_dir: &Path) -> DatIndex {
        let mut index = DatIndex::default();
        for entry in walkdir::WalkDir::new(dats_dir)
            .max_depth(2)
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
                .map(|e| matches!(e.to_ascii_lowercase().as_str(), "dat" | "xml"))
                .unwrap_or(false);
            if !ext_ok {
                continue;
            }
            let before = index.by_crc32.len();
            match std::fs::read_to_string(path) {
                Ok(text) => {
                    index.parse(&text);
                    index.files += 1;
                    log::info!(
                        "loaded {} name(s) from {}",
                        index.by_crc32.len() - before,
                        path.display()
                    );
                }
                Err(e) => log::warn!("can't read {}: {e}", path.display()),
            }
        }
        index
    }

    pub fn lookup(&self, crc32: u32) -> Option<&str> {
        self.by_crc32.get(&crc32).map(|s| s.as_str())
    }

    /// Total names known, for the settings readout.
    pub fn len(&self) -> usize {
        self.by_crc32.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_crc32.is_empty()
    }

    /// DAT files loaded, for the settings readout.
    pub fn files(&self) -> usize {
        self.files
    }

    fn parse(&mut self, text: &str) {
        let text = text.trim_start_matches('\u{feff}');
        if text.trim_start().starts_with('<') {
            self.parse_logiqx_xml(text);
        } else {
            self.parse_clrmamepro(text);
        }
    }

    fn insert(&mut self, crc: &str, name: String) {
        let Ok(crc) = u32::from_str_radix(crc.trim().trim_start_matches("0x"), 16) else {
            return;
        };
        self.by_crc32.entry(crc).or_insert(name);
    }

    /// Logiqx XML: `<game name="…"><rom name="…" crc="…"/></game>`
    /// (some tools emit `<machine>` instead of `<game>`).
    fn parse_logiqx_xml(&mut self, text: &str) {
        use quick_xml::events::Event;
        let mut reader = quick_xml::Reader::from_str(text);
        let mut game_name: Option<String> = None;
        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                    let attr = |key: &[u8]| {
                        e.attributes()
                            .flatten()
                            .find(|a| a.key.as_ref() == key)
                            .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
                    };
                    match e.name().as_ref() {
                        b"game" | b"machine" => game_name = attr(b"name"),
                        b"rom" => {
                            if let Some(crc) = attr(b"crc") {
                                // Prefer the game's canonical name; fall
                                // back to the rom entry's own name minus
                                // its extension.
                                let name = game_name.clone().or_else(|| {
                                    attr(b"name").map(|n| match n.rsplit_once('.') {
                                        Some((stem, _)) => stem.to_string(),
                                        None => n,
                                    })
                                });
                                if let Some(name) = name {
                                    self.insert(&crc, name);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::End(e)) => {
                    if matches!(e.name().as_ref(), b"game" | b"machine") {
                        game_name = None;
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    log::warn!("DAT XML parse error: {e}");
                    break;
                }
                Ok(_) => {}
            }
        }
    }

    /// ClrMamePro text: `game ( name "…" rom ( name "…" crc HHHHHHHH ) )`.
    fn parse_clrmamepro(&mut self, text: &str) {
        let tokens = clrmamepro_tokens(text);
        let mut i = 0;
        while i < tokens.len() {
            if (tokens[i] == "game" || tokens[i] == "machine") && tokens.get(i + 1).map(|t| t.as_str()) == Some("(") {
                i += 2;
                let mut depth = 1usize;
                let mut game_name: Option<String> = None;
                let mut crcs: Vec<String> = Vec::new();
                while i < tokens.len() && depth > 0 {
                    match tokens[i].as_str() {
                        "(" => depth += 1,
                        ")" => depth -= 1,
                        "name" if depth == 1 && game_name.is_none() => {
                            game_name = tokens.get(i + 1).cloned();
                            i += 1;
                        }
                        "crc" | "crc32" if depth >= 2 => {
                            if let Some(v) = tokens.get(i + 1) {
                                crcs.push(v.clone());
                                i += 1;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
                if let Some(name) = game_name {
                    for crc in crcs {
                        self.insert(&crc, name.clone());
                    }
                }
            } else {
                i += 1;
            }
        }
    }
}

/// Tokenize ClrMamePro text: quoted strings are single tokens, parens
/// are their own tokens, everything else splits on whitespace.
fn clrmamepro_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                let mut s = String::new();
                for c in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                    s.push(c);
                }
                tokens.push(s);
            }
            '(' | ')' => tokens.push(c.to_string()),
            c if c.is_whitespace() => {}
            c => {
                let mut s = String::from(c);
                while let Some(&n) = chars.peek() {
                    if n.is_whitespace() || n == '(' || n == ')' || n == '"' {
                        break;
                    }
                    s.push(n);
                    chars.next();
                }
                tokens.push(s);
            }
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_rom() -> Vec<u8> {
        let mut bytes = vec![0u8; 0x200];
        bytes[0xa0..0xa9].copy_from_slice(b"HEADERTTL");
        bytes[0xac..0xb0].copy_from_slice(b"ABCE");
        bytes
    }

    #[test]
    fn logiqx_xml_names_win_over_header() {
        let rom = fake_rom();
        let crc = crc32fast::hash(&rom);

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("roms")).unwrap();
        std::fs::create_dir(dir.path().join("dats")).unwrap();
        std::fs::write(dir.path().join("roms/test.gba"), &rom).unwrap();
        std::fs::write(
            dir.path().join("dats/gba.dat"),
            format!(
                r#"<?xml version="1.0"?>
<!DOCTYPE datafile PUBLIC "-//Logiqx//DTD ROM Management Datafile//EN" "http://www.logiqx.com/Dats/datafile.dtd">
<datafile>
  <header><name>Nintendo - Game Boy Advance</name></header>
  <game name="Some Game &amp; Friends (USA)">
    <description>Some Game &amp; Friends (USA)</description>
    <rom name="Some Game &amp; Friends (USA).gba" size="{}" crc="{crc:08X}" md5="0" sha1="0"/>
  </game>
  <game name="Unrelated (Japan)">
    <rom name="Unrelated (Japan).gba" size="4" crc="DEADBEEF"/>
  </game>
</datafile>"#,
                rom.len()
            ),
        )
        .unwrap();

        let dats = DatIndex::load_dir(&dir.path().join("dats"));
        assert_eq!(dats.files(), 1);
        assert_eq!(dats.lookup(crc), Some("Some Game & Friends (USA)"));

        let library = Library::scan(&dir.path().join("roms"), &dats);
        assert_eq!(library.roms.len(), 1);
        assert_eq!(library.roms[0].display_name(), "Some Game & Friends (USA)");
        assert_eq!(library.roms[0].title, "HEADERTTL");
    }

    #[test]
    fn clrmamepro_and_header_fallback() {
        let rom = fake_rom();
        let crc = crc32fast::hash(&rom);

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("roms")).unwrap();
        std::fs::create_dir(dir.path().join("dats")).unwrap();
        std::fs::write(dir.path().join("roms/test.gba"), &rom).unwrap();
        // A second rom no DAT knows: header fallback.
        let mut other = fake_rom();
        other[0xa0..0xa9].copy_from_slice(b"OTHERTITL");
        other[0x100] = 0x77; // different crc
        std::fs::write(dir.path().join("roms/other.gba"), &other).unwrap();
        std::fs::write(
            dir.path().join("dats/gba.dat"),
            format!(
                "clrmamepro (\n\tname \"Nintendo - Game Boy Advance\"\n)\ngame (\n\tname \"Cool Game (Europe) (En,Fr,De)\"\n\tdescription \"Cool Game (Europe)\"\n\trom ( name \"Cool Game (Europe).gba\" size {} crc {crc:08x} md5 0 )\n)\n",
                rom.len()
            ),
        )
        .unwrap();

        let dats = DatIndex::load_dir(&dir.path().join("dats"));
        assert_eq!(dats.lookup(crc), Some("Cool Game (Europe) (En,Fr,De)"));

        let library = Library::scan(&dir.path().join("roms"), &dats);
        assert_eq!(library.roms.len(), 2);
        let cool = library.roms.iter().find(|r| r.crc32 == crc).unwrap();
        assert_eq!(cool.display_name(), "Cool Game (Europe) (En,Fr,De)");
        let fallback = library.roms.iter().find(|r| r.crc32 != crc).unwrap();
        assert_eq!(fallback.display_name(), "OTHERTITL");
    }
}
