//! Replay container for gbaroll sessions. Because the link is a
//! deterministic function of the joypad streams, a replay is just the boot
//! configuration plus confirmed inputs per tick — no savestate stream, no
//! rounds, no per-game knowledge: one roundless format for every link game
//! and every player count (2 to 4).
//!
//! The input stream uses a compact per-tick encoding in the spirit of
//! tango's replays: a joypad is only 10 bits and most ticks are idle or
//! repeat the previous tick, so one tag byte usually stands in for the
//! whole row, with an explicit little-endian `u16` appended only for each
//! player whose keys need spelling out. A whole-session recording is
//! therefore dominated by long idle runs that cost a byte each.

/// Bumped on any incompatible layout change.
const MAGIC: &[u8; 12] = b"GBAROLLRPLY\x01";

/// Preferred file extension for replays on disk.
pub const FILE_EXTENSION: &str = "gbrr";

/// Most players a link supports (mgba's `MAX_GBAS`).
pub const MAX_PLAYERS: usize = 4;

/// Per-tick tag byte:
///   bit 7 (op):        0 = the default keys are zero, 1 = the default is
///                      that player's previous tick
///   bits 0..=3:        player `i` takes the default (no explicit `u16`
///                      follows for them); bits at or above the player
///                      count are always clear
/// Explicit players follow the tag in player order, 2 bytes LE each.
///
/// `0x00` is the end-of-stream sentinel. An all-explicit tick with op=0
/// would also encode as `0x00`, so the writer forces op=1 whenever every
/// player is explicit (the defaults are unused then, so the bit is free).
const OP_PREV: u8 = 0b1000_0000;
const END_OF_REPLAY: u8 = 0x00;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not a gbaroll replay")]
    BadMagic,
    #[error("truncated replay")]
    Truncated,
    #[error("invalid player count {0}")]
    BadPlayerCount(u8),
    #[error("invalid metadata: {0}")]
    BadMetadata(&'static str),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlayerMeta {
    pub nick: String,
    /// CRC32 of the ROM this player's side runs. Sides may run different
    /// ROMs (each GBA on a real cable has its own cart); whoever plays
    /// the replay back needs a copy of each.
    pub rom_crc32: u32,
    /// That ROM's header-internal title (up to 12 bytes of ASCII).
    pub rom_title: String,
    /// That ROM's header game code (up to 4 bytes of ASCII).
    pub rom_code: String,
    /// Opaque boot capture the player's side loaded before the first tick
    /// (sessions start mid-game: a plugged-in cable rather than a power-on
    /// boot). The bytes are whatever the recorder handed in — gbaroll
    /// stores its compressed exchange blob, save image included — and
    /// playback must hand them back to the same boot path. Embedding them
    /// keeps the replay self-sufficient given the ROMs.
    pub boot: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// Which side's perspective recorded this (the default view on
    /// playback).
    pub local_player: u8,
    /// Micros since the unix epoch the recording started; `None` if
    /// unknown.
    pub started_at_unix_micros: Option<u64>,
    /// Micros since the unix epoch the carts' RTC was pinned to; `None`
    /// if the session ran without a pinned clock.
    pub rtc_unix_micros: Option<u64>,
    /// One entry per player, 2 to [`MAX_PLAYERS`], in player order.
    pub players: Vec<PlayerMeta>,
}

impl Default for Metadata {
    fn default() -> Self {
        Metadata {
            local_player: 0,
            started_at_unix_micros: None,
            rtc_unix_micros: None,
            players: vec![PlayerMeta::default(), PlayerMeta::default()],
        }
    }
}

/// A length-prefixed optional byte blob: `u32::MAX` = `None`.
fn write_blob<W: std::io::Write>(w: &mut W, blob: Option<&[u8]>) -> std::io::Result<()> {
    match blob {
        Some(blob) => {
            w.write_all(&(blob.len() as u32).to_le_bytes())?;
            w.write_all(blob)
        }
        None => w.write_all(&u32::MAX.to_le_bytes()),
    }
}

fn write_ascii_padded<W: std::io::Write>(w: &mut W, s: &str, len: usize) -> std::io::Result<()> {
    let mut buf = vec![0u8; len];
    let bytes = s.as_bytes();
    let n = bytes.len().min(len);
    buf[..n].copy_from_slice(&bytes[..n]);
    w.write_all(&buf)
}

fn read_ascii_padded(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Streams a recording into `w` as it goes: the header is written by
/// [`new`](Writer::new), each tick's record by [`push`](Writer::push).
/// Nothing is held back for the end but the one-byte sentinel, so a
/// recording that dies mid-session still parses up to its last flushed
/// tick (see [`Replay::is_complete`]).
pub struct Writer<W: std::io::Write> {
    w: W,
    num_players: usize,
    /// Last keys emitted per player, for the "default = previous" form.
    prev: [u16; MAX_PLAYERS],
}

impl<W: std::io::Write> Writer<W> {
    /// Write the header (magic + metadata) into `w` and wrap it.
    pub fn new(mut w: W, metadata: &Metadata) -> std::io::Result<Self> {
        let num_players = metadata.players.len();
        assert!(
            (2..=MAX_PLAYERS).contains(&num_players),
            "a replay takes 2 to {MAX_PLAYERS} players, got {num_players}"
        );
        assert!((metadata.local_player as usize) < num_players);

        w.write_all(MAGIC)?;
        w.write_all(&[num_players as u8, metadata.local_player])?;
        w.write_all(&metadata.started_at_unix_micros.unwrap_or(u64::MAX).to_le_bytes())?;
        w.write_all(&metadata.rtc_unix_micros.unwrap_or(u64::MAX).to_le_bytes())?;
        for player in &metadata.players {
            let nick = player.nick.as_bytes();
            let n = nick.len().min(u8::MAX as usize);
            w.write_all(&[n as u8])?;
            w.write_all(&nick[..n])?;
            w.write_all(&player.rom_crc32.to_le_bytes())?;
            write_ascii_padded(&mut w, &player.rom_title, 12)?;
            write_ascii_padded(&mut w, &player.rom_code, 4)?;
            write_blob(&mut w, player.boot.as_deref())?;
        }
        Ok(Writer {
            w,
            num_players,
            prev: [0; MAX_PLAYERS],
        })
    }

    /// Append one confirmed tick's input row (player-indexed keys; GBA
    /// joypads are 10 bits).
    pub fn push(&mut self, keys: &[u32]) -> std::io::Result<()> {
        assert_eq!(keys.len(), self.num_players, "one key set per player");
        let keys: Vec<u16> = keys.iter().map(|&k| k as u16).collect();

        // Prefer whichever default sense (zero vs previous) leaves fewer
        // players explicit; tie-break to op=0 so the canonical idle tick
        // stays a single all-defaults byte.
        let zero_defaults: Vec<bool> = keys.iter().map(|&k| k == 0).collect();
        let prev_defaults: Vec<bool> = keys.iter().enumerate().map(|(i, &k)| k == self.prev[i]).collect();
        let explicit = |d: &[bool]| d.iter().filter(|&&v| !v).count();
        let (mut op_prev, defaults) = if explicit(&prev_defaults) < explicit(&zero_defaults) {
            (true, prev_defaults)
        } else {
            (false, zero_defaults)
        };
        // Keep an all-explicit tick's tag off the 0x00 sentinel.
        if defaults.iter().all(|&d| !d) {
            op_prev = true;
        }

        let mut tag = 0u8;
        if op_prev {
            tag |= OP_PREV;
        }
        for (i, &d) in defaults.iter().enumerate() {
            if d {
                tag |= 1 << i;
            }
        }

        let mut record = Vec::with_capacity(1 + 2 * self.num_players);
        record.push(tag);
        for (i, &k) in keys.iter().enumerate() {
            if !defaults[i] {
                record.extend_from_slice(&k.to_le_bytes());
            }
            self.prev[i] = k;
        }
        self.w.write_all(&record)
    }

    /// Write the end-of-stream sentinel, flush, and hand back the sink.
    pub fn finish(mut self) -> std::io::Result<W> {
        self.w.write_all(&[END_OF_REPLAY])?;
        self.w.flush()?;
        Ok(self.w)
    }
}

pub struct Replay {
    pub metadata: Metadata,
    /// Player-indexed key rows, one per tick.
    pub inputs: Vec<Vec<u32>>,
    /// Whether the stream ended on the sentinel (vs. a truncated tail).
    pub is_complete: bool,
}

impl Replay {
    pub fn num_players(&self) -> usize {
        self.metadata.players.len()
    }

    pub fn parse(data: &[u8]) -> Result<Self, Error> {
        let (metadata, mut r) = parse_header(data)?;
        let num_players = metadata.players.len();

        // Streaming tag decode. `0x00` ends cleanly; a truncated tail
        // (missing an explicit key, or no sentinel) drops the partial tick
        // and leaves is_complete = false, so a crashed recording still
        // plays back everything that was flushed.
        let mut inputs = Vec::new();
        let mut prev = [0u16; MAX_PLAYERS];
        let mut is_complete = false;
        'stream: while let Ok(&tag) = r.peek() {
            r.at += 1;
            if tag == END_OF_REPLAY {
                is_complete = true;
                break;
            }
            let op_prev = tag & OP_PREV != 0;
            let mut row = Vec::with_capacity(num_players);
            for (i, prev_v) in prev.iter_mut().enumerate().take(num_players) {
                let v = if tag & (1 << i) != 0 {
                    if op_prev {
                        *prev_v
                    } else {
                        0
                    }
                } else {
                    let Ok(bytes) = r.take(2) else {
                        break 'stream;
                    };
                    u16::from_le_bytes(bytes.try_into().unwrap())
                };
                *prev_v = v;
                row.push(v as u32);
            }
            inputs.push(row);
        }

        Ok(Replay {
            metadata,
            inputs,
            is_complete,
        })
    }
}

/// Parse just the header, cheaply — for replay browsers that only need
/// the metadata. (The tick count still requires a full [`Replay::parse`].)
pub fn parse_metadata(data: &[u8]) -> Result<Metadata, Error> {
    Ok(parse_header(data)?.0)
}

fn parse_header(data: &[u8]) -> Result<(Metadata, Cursor<'_>), Error> {
    let mut r = Cursor { data, at: 0 };
    if r.take(MAGIC.len())? != MAGIC.as_slice() {
        return Err(Error::BadMagic);
    }
    let num_players = r.take(1)?[0];
    if !(2..=MAX_PLAYERS as u8).contains(&num_players) {
        return Err(Error::BadPlayerCount(num_players));
    }
    let local_player = r.take(1)?[0];
    if local_player >= num_players {
        return Err(Error::BadMetadata("local player out of range"));
    }
    let started = u64::from_le_bytes(r.take(8)?.try_into().unwrap());
    let rtc = u64::from_le_bytes(r.take(8)?.try_into().unwrap());
    let mut players = Vec::with_capacity(num_players as usize);
    for _ in 0..num_players {
        let nick_len = r.take(1)?[0] as usize;
        let nick = String::from_utf8_lossy(r.take(nick_len)?).into_owned();
        let rom_crc32 = u32::from_le_bytes(r.take(4)?.try_into().unwrap());
        let rom_title = read_ascii_padded(r.take(12)?);
        let rom_code = read_ascii_padded(r.take(4)?);
        let boot = read_blob(&mut r)?;
        players.push(PlayerMeta {
            nick,
            rom_crc32,
            rom_title,
            rom_code,
            boot,
        });
    }
    Ok((
        Metadata {
            local_player,
            started_at_unix_micros: (started != u64::MAX).then_some(started),
            rtc_unix_micros: (rtc != u64::MAX).then_some(rtc),
            players,
        },
        r,
    ))
}

fn read_blob(r: &mut Cursor<'_>) -> Result<Option<Vec<u8>>, Error> {
    let len = u32::from_le_bytes(r.take(4)?.try_into().unwrap());
    if len == u32::MAX {
        return Ok(None);
    }
    Ok(Some(r.take(len as usize)?.to_vec()))
}

struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.data.len() - self.at < n {
            return Err(Error::Truncated);
        }
        let s = &self.data[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }

    fn peek(&self) -> Result<&'a u8, Error> {
        self.data.get(self.at).ok_or(Error::Truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(num_players: usize) -> Metadata {
        Metadata {
            local_player: 1,
            started_at_unix_micros: Some(1_752_000_000_000_000),
            rtc_unix_micros: Some(1_752_000_000_000_001),
            players: (0..num_players)
                .map(|i| PlayerMeta {
                    nick: format!("player{i}"),
                    rom_crc32: 0xdead_beef + i as u32,
                    rom_title: format!("TESTGAME{i}"),
                    rom_code: "ATST".to_string(),
                    boot: (i == 1).then(|| vec![9; 64]),
                })
                .collect(),
        }
    }

    fn roundtrip(num_players: usize, inputs: &[Vec<u32>]) -> Replay {
        let m = meta(num_players);
        let mut w = Writer::new(Vec::new(), &m).unwrap();
        for row in inputs {
            w.push(row).unwrap();
        }
        let bytes = w.finish().unwrap();
        let parsed = Replay::parse(&bytes).unwrap();
        assert_eq!(parsed.inputs, inputs);
        assert!(parsed.is_complete);
        assert_eq!(parsed.metadata, m);
        parsed
    }

    #[test]
    fn roundtrips_representative_streams() {
        for n in 2..=4usize {
            roundtrip(n, &[]);
            roundtrip(n, &vec![vec![0; n]; 500]); // idle: 1 byte/tick
            roundtrip(n, &vec![(1..=n as u32).collect::<Vec<_>>(); 3]); // held keys
            roundtrip(
                n,
                &[
                    (0..n as u32).map(|i| 0x3ff - i).collect(),
                    vec![0; n],
                    (0..n as u32).map(|i| 0x100 + i).collect(),
                ],
            );
        }
    }

    #[test]
    fn metadata_only_parse_matches() {
        let m = meta(3);
        let mut w = Writer::new(Vec::new(), &m).unwrap();
        w.push(&[1, 2, 3]).unwrap();
        let bytes = w.finish().unwrap();
        assert_eq!(parse_metadata(&bytes).unwrap(), m);
    }

    #[test]
    fn idle_run_is_one_byte_per_tick() {
        let m = Metadata {
            players: vec![PlayerMeta::default(); 4],
            ..Default::default()
        };
        let mut w = Writer::new(Vec::new(), &m).unwrap();
        let header_len = w.w.len();
        for _ in 0..1000 {
            w.push(&[0, 0, 0, 0]).unwrap();
        }
        let bytes = w.finish().unwrap();
        assert_eq!(bytes.len(), header_len + 1000 + 1);
    }

    #[test]
    fn truncated_tail_recovers_prefix() {
        let m = meta(4);
        let mut w = Writer::new(Vec::new(), &m).unwrap();
        for i in 0..10u32 {
            w.push(&[i & 0x3ff, (i * 3) & 0x3ff, (i * 7) & 0x3ff, (i * 11) & 0x3ff])
                .unwrap();
        }
        let mut bytes = w.finish().unwrap();
        bytes.truncate(bytes.len() - 4); // eat the sentinel + part of the last tick
        let parsed = Replay::parse(&bytes).unwrap();
        assert!(!parsed.is_complete);
        assert!(parsed.inputs.len() >= 8); // most of the stream survives
        for (i, row) in parsed.inputs.iter().enumerate() {
            let i = i as u32;
            assert_eq!(row.as_slice(), &[i & 0x3ff, (i * 3) & 0x3ff, (i * 7) & 0x3ff, (i * 11) & 0x3ff]);
        }
    }

    #[test]
    fn all_explicit_tick_avoids_sentinel_tag() {
        // Every player changes to a fresh nonzero value: no defaults apply
        // under either op, which must not emit a 0x00 tag mid-stream.
        roundtrip(4, &[vec![1, 2, 3, 4], vec![5, 6, 7, 8], vec![0, 0, 0, 0]]);
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(Replay::parse(b"NOTAREPLAY"), Err(Error::BadMagic) | Err(Error::Truncated)));
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.push(7); // bogus player count
        assert!(matches!(Replay::parse(&bytes), Err(Error::BadPlayerCount(7))));
    }
}
