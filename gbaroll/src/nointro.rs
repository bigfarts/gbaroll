//! Everything No-Intro: the DAT index (CRC32 → canonical game name),
//! parsers for both formats No-Intro distributes (Logiqx XML and
//! ClrMamePro text), and the fetcher that downloads the GBA DAT so
//! users don't have to hunt it down. Datomatic (the official source)
//! is form-gated, so the fetcher pulls the libretro-database mirror.

use std::collections::HashMap;
use std::path::Path;

pub const GBA_DAT_URL: &str =
    "https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/Nintendo%20-%20Game%20Boy%20Advance.dat";

pub const GBA_DAT_FILENAME: &str = "nointro.dat";

/// The single app-managed DAT beside gbaroll's `config.json`.
pub fn storage_path() -> std::path::PathBuf {
    crate::config::config_dir().join(GBA_DAT_FILENAME)
}

/// Download the GBA No-Intro DAT into internal storage, validating that it
/// actually parses before committing (temp file + rename, so a failed
/// download never clobbers a good copy). Returns the number of names.
pub async fn fetch_gba_dat() -> anyhow::Result<usize> {
    let response = reqwest::get(GBA_DAT_URL).await?.error_for_status()?;
    let text = response.text().await?;

    let mut index = DatIndex::default();
    index.add_text(&text);
    anyhow::ensure!(!index.is_empty(), "downloaded DAT parsed to zero entries");

    let path = storage_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    replace_gba_dat(&path, &text)?;
    log::info!(
        "downloaded {} ({} names) to {}",
        GBA_DAT_URL,
        index.len(),
        path.display()
    );
    Ok(index.len())
}

/// Replace an existing database portably. Windows cannot rename over an
/// existing file, so retain the previous copy until the new one commits.
fn replace_gba_dat(path: &Path, text: &str) -> std::io::Result<()> {
    let filename = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!("{filename}.part"));
    let backup = path.with_file_name(format!("{filename}.bak"));
    std::fs::write(&tmp, text)?;

    // Recover the old copy if a previous process stopped between the two
    // renames, then begin a fresh replacement.
    if !path.exists() && backup.exists() {
        std::fs::rename(&backup, path)?;
    }
    if backup.exists() {
        std::fs::remove_file(&backup)?;
    }
    if path.exists() {
        std::fs::rename(path, &backup)?;
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        if backup.exists() {
            let _ = std::fs::rename(&backup, path);
        }
        return Err(error);
    }
    if backup.exists() {
        if let Err(error) = std::fs::remove_file(&backup) {
            log::warn!(
                "couldn't remove stale DAT backup {}: {error}",
                backup.display()
            );
        }
    }
    Ok(())
}

/// CRC32 → canonical game name parsed from the managed DAT. Understands
/// both Logiqx XML and ClrMamePro text.
#[derive(Default)]
pub struct DatIndex {
    by_crc32: HashMap<u32, String>,
}

impl DatIndex {
    /// Load the app-managed GBA DAT database.
    pub fn load() -> DatIndex {
        Self::load_path(&storage_path())
    }

    fn load_path(path: &Path) -> DatIndex {
        let mut index = DatIndex::default();
        match std::fs::read_to_string(path) {
            Ok(text) => {
                index.add_text(&text);
                log::info!("loaded {} name(s) from {}", index.len(), path.display());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => log::warn!("can't read {}: {error}", path.display()),
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

    /// Parse one DAT's text (format auto-detected) into the index.
    pub fn add_text(&mut self, text: &str) {
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
            if (tokens[i] == "game" || tokens[i] == "machine")
                && tokens.get(i + 1).map(|t| t.as_str()) == Some("(")
            {
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
    use crate::library::Library;

    fn fake_rom() -> Vec<u8> {
        let mut bytes = vec![0u8; 0x200];
        bytes[0xa0..0xa9].copy_from_slice(b"HEADERTTL");
        bytes[0xac..0xb0].copy_from_slice(b"ABCE");
        bytes
    }

    #[test]
    fn managed_storage_lives_under_config_and_updates_in_place() {
        assert_eq!(
            storage_path(),
            crate::config::config_dir().join(GBA_DAT_FILENAME)
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(GBA_DAT_FILENAME);
        replace_gba_dat(&path, "old database").unwrap();
        replace_gba_dat(&path, "latest database").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "latest database");
        assert!(!path
            .with_file_name(format!("{GBA_DAT_FILENAME}.bak"))
            .exists());
    }

    #[test]
    fn logiqx_xml_names_win_over_header() {
        let rom = fake_rom();
        let crc = crc32fast::hash(&rom);

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("roms")).unwrap();
        std::fs::write(dir.path().join("roms/test.gba"), &rom).unwrap();
        let dat_path = dir.path().join("gba.dat");
        std::fs::write(
            &dat_path,
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

        let dat = DatIndex::load_path(&dat_path);
        assert_eq!(dat.lookup(crc), Some("Some Game & Friends (USA)"));

        let library = Library::scan(&dir.path().join("roms"), &dat);
        assert_eq!(library.roms.len(), 1);
        assert_eq!(library.roms[0].display_name(), "Some Game & Friends (USA)");
        assert_eq!(library.roms[0].title, "HEADERTTL");
    }

    /// Validates the parser against a real downloaded DAT. Ignored by
    /// default (needs a file): run with
    /// `GBAROLL_TEST_DAT=path/to.dat cargo test -- --ignored real_dat`.
    #[test]
    #[ignore]
    fn real_dat_parses() {
        let path = std::env::var("GBAROLL_TEST_DAT").expect("set GBAROLL_TEST_DAT");
        let text = std::fs::read_to_string(path).unwrap();
        let mut index = DatIndex::default();
        index.add_text(&text);
        println!("parsed {} names", index.len());
        assert!(
            index.len() > 1000,
            "real DAT parsed to only {} names",
            index.len()
        );
    }

    #[test]
    fn clrmamepro_and_header_fallback() {
        let rom = fake_rom();
        let crc = crc32fast::hash(&rom);

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("roms")).unwrap();
        std::fs::write(dir.path().join("roms/test.gba"), &rom).unwrap();
        // A second rom no DAT knows: header fallback.
        let mut other = fake_rom();
        other[0xa0..0xa9].copy_from_slice(b"OTHERTITL");
        other[0x100] = 0x77; // different crc
        std::fs::write(dir.path().join("roms/other.gba"), &other).unwrap();
        let dat_path = dir.path().join("gba.dat");
        std::fs::write(
            &dat_path,
            format!(
                "clrmamepro (\n\tname \"Nintendo - Game Boy Advance\"\n)\ngame (\n\tname \"Cool Game (Europe) (En,Fr,De)\"\n\tdescription \"Cool Game (Europe)\"\n\trom ( name \"Cool Game (Europe).gba\" size {} crc {crc:08x} md5 0 )\n)\n",
                rom.len()
            ),
        )
        .unwrap();

        let dat = DatIndex::load_path(&dat_path);
        assert_eq!(dat.lookup(crc), Some("Cool Game (Europe) (En,Fr,De)"));

        let library = Library::scan(&dir.path().join("roms"), &dat);
        assert_eq!(library.roms.len(), 2);
        let cool = library.roms.iter().find(|r| r.crc32 == crc).unwrap();
        assert_eq!(cool.display_name(), "Cool Game (Europe) (En,Fr,De)");
        let fallback = library.roms.iter().find(|r| r.crc32 != crc).unwrap();
        assert_eq!(fallback.display_name(), "OTHERTITL");
    }
}
