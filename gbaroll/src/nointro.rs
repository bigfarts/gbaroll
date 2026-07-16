//! Everything No-Intro: the DAT index (CRC32 → canonical game name),
//! a ClrMamePro-text parser (the format the libretro mirror serves —
//! Logiqx XML is deliberately not supported), and the fetcher that
//! downloads the GBA DAT so users don't have to hunt it down.
//! Datomatic (the official source) is form-gated, so the fetcher pulls
//! the libretro-database mirror (which serves
//! `Access-Control-Allow-Origin: *`); a bundled snapshot covers the
//! offline/first-load case. The downloaded copy lives at the OPFS root
//! as `nointro.dat`.

use std::collections::HashMap;

use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use crate::storage::{self, Storage};

pub const GBA_DAT_URL: &str =
    "https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/Nintendo%20-%20Game%20Boy%20Advance.dat";

pub const GBA_DAT_FILENAME: &str = "nointro.dat";

async fn fetch_text(url: &str) -> anyhow::Result<String> {
    let window = web_sys::window().ok_or_else(|| anyhow::anyhow!("no window"))?;
    let response: web_sys::Response = JsFuture::from(window.fetch_with_str(url))
        .await
        .map_err(|e| anyhow::anyhow!("fetch failed: {e:?}"))?
        .dyn_into()
        .map_err(|_| anyhow::anyhow!("fetch returned a non-Response"))?;
    if !response.ok() {
        anyhow::bail!("fetch failed: HTTP {}", response.status());
    }
    let text = JsFuture::from(
        response
            .text()
            .map_err(|e| anyhow::anyhow!("response.text: {e:?}"))?,
    )
    .await
    .map_err(|e| anyhow::anyhow!("reading response body: {e:?}"))?;
    text.as_string()
        .ok_or_else(|| anyhow::anyhow!("response body wasn't a string"))
}

/// Download the GBA No-Intro DAT into OPFS, validating that it actually
/// parses before committing (so a failed download never clobbers a good
/// copy). Returns the number of names.
pub async fn fetch_gba_dat(storage: &Storage) -> anyhow::Result<usize> {
    let text = fetch_text(GBA_DAT_URL).await?;

    let mut index = DatIndex::default();
    index.add_text(&text);
    anyhow::ensure!(!index.is_empty(), "downloaded DAT parsed to zero entries");

    storage::write(storage.root(), GBA_DAT_FILENAME, text.as_bytes())
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    log::info!("downloaded {} ({} names)", GBA_DAT_URL, index.len());
    Ok(index.len())
}

/// Load the DAT: the OPFS copy when present, else the bundled snapshot
/// at `fallback_url` (an app asset).
pub async fn load(storage: &Storage, fallback_url: &str) -> DatIndex {
    let mut index = DatIndex::default();
    match storage::read(storage.root(), GBA_DAT_FILENAME).await {
        Ok(Some(bytes)) => {
            index.add_text(&String::from_utf8_lossy(&bytes));
            log::info!("loaded {} name(s) from OPFS", index.len());
        }
        Ok(None) => match fetch_text(fallback_url).await {
            Ok(text) => {
                index.add_text(&text);
                log::info!("loaded {} name(s) from the bundled DAT", index.len());
            }
            Err(e) => log::warn!("couldn't load the bundled DAT: {e:#}"),
        },
        Err(e) => log::warn!("couldn't read the stored DAT: {e}"),
    }
    index
}

/// CRC32 → canonical game name parsed from the managed DAT. Understands
/// both Logiqx XML and ClrMamePro text.
#[derive(Default, Clone)]
pub struct DatIndex {
    by_crc32: HashMap<u32, String>,
}

impl DatIndex {
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

    /// Parse one DAT's text into the index (ClrMamePro only).
    pub fn add_text(&mut self, text: &str) {
        let text = text.trim_start_matches('\u{feff}');
        if text.trim_start().starts_with('<') {
            log::warn!("XML DATs aren't supported; use a ClrMamePro-format DAT");
            return;
        }
        self.parse_clrmamepro(text);
    }

    fn insert(&mut self, crc: &str, name: String) {
        let Ok(crc) = u32::from_str_radix(crc.trim().trim_start_matches("0x"), 16) else {
            return;
        };
        self.by_crc32.entry(crc).or_insert(name);
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

    #[test]
    fn xml_is_rejected() {
        let mut dat = DatIndex::default();
        dat.add_text(r#"<?xml version="1.0"?><datafile></datafile>"#);
        assert!(dat.is_empty());
    }

    #[test]
    fn clrmamepro_parses() {
        let mut dat = DatIndex::default();
        dat.add_text(
            "clrmamepro (\n\tname \"Nintendo - Game Boy Advance\"\n)\ngame (\n\tname \"Cool Game (Europe) (En,Fr,De)\"\n\trom ( name \"Cool Game (Europe).gba\" size 4 crc 0000bbbb md5 0 )\n)\n",
        );
        assert_eq!(dat.lookup(0xbbbb), Some("Cool Game (Europe) (En,Fr,De)"));
    }
}
