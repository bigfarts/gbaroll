//! The peer-to-peer wire protocol. Each mesh edge carries two channels:
//!
//! * `gbaroll-ctl` (stream 0, reliable/ordered) — bincode [`PeerControl`]
//!   messages: the version handshake, the boot-payload exchange that
//!   plugs the cable in, and the deliberate quit.
//! * `gbaroll-data` (stream 1, unreliable/unordered) — [`rennet`]
//!   frames of [`Input`] elements (one per local tick, seq = tick) with
//!   a per-frame [`Meta`] carrying clock sync + the newest settled-state
//!   checkpoint for cross-peer desync detection.

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change to the peer protocol.
pub const NET_VERSION: u32 = 3;

/// Rollback horizon: how far ahead of a missing input the streams will
/// buffer before declaring the peer unrecoverable (~10s at 60fps).
pub const HORIZON: u32 = 600;

/// One player's joypad for one tick (10 bits used).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Input(pub u16);

impl rennet::Codec for Input {
    fn encode<W: std::io::Write>(&self, w: &mut W) -> std::io::Result<()> {
        w.write_all(&self.0.to_le_bytes())
    }

    fn decode<R: std::io::Read>(r: &mut R) -> std::io::Result<Option<Self>> {
        let mut b = [0u8; 2];
        // First byte: 0 bytes read = clean EOF at the run boundary.
        if r.read(&mut b[..1])? == 0 {
            return Ok(None);
        }
        // Second byte: EOF here is a truncated element.
        r.read_exact(&mut b[1..])?;
        Ok(Some(Input(u16::from_le_bytes(b))))
    }
}

/// Per-frame side channel: the sender's clock-sync half plus its newest
/// settled checkpoint (tick 0 = none yet — real checkpoints are 1-based).
/// Receivers compare the digest against their own settled state at that
/// tick; a mismatch is a desync and ends the session loudly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    pub tick_advantage: i16,
    pub checkpoint_tick: u32,
    pub checkpoint_digest: u32,
}

impl rennet::Codec for Meta {
    fn encode<W: std::io::Write>(&self, w: &mut W) -> std::io::Result<()> {
        rennet::write_svarint(w, self.tick_advantage as i64)?;
        rennet::write_uvarint(w, self.checkpoint_tick as u64)?;
        w.write_all(&self.checkpoint_digest.to_le_bytes())
    }

    fn decode<R: std::io::Read>(r: &mut R) -> std::io::Result<Option<Self>> {
        // The meta is required: a short read errors (never a clean `None`).
        let tick_advantage = i16::try_from(rennet::read_svarint(r)?)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "tick advantage out of range"))?;
        let checkpoint_tick = u32::try_from(rennet::read_uvarint(r)?)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "checkpoint tick out of range"))?;
        let mut digest = [0u8; 4];
        r.read_exact(&mut digest)?;
        Ok(Some(Meta {
            tick_advantage,
            checkpoint_tick,
            checkpoint_digest: u32::from_le_bytes(digest),
        }))
    }
}

/// The rennet protocol marker for the data channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proto;

impl rennet::Protocol for Proto {
    type Element = Input;
    type Meta = Meta;
    const MAX_RUN: usize = HORIZON as usize;
}

pub type Frame = rennet::Frame<Proto>;
pub type OutStream = rennet::OutStream<Proto>;
pub type InStream = rennet::InStream<Proto>;

/// Control-plane messages (reliable/ordered channel).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PeerControl {
    Hello { net_version: u32 },
    Quit,
    /// Announces this side's encoded [`BootBlob`]; exactly `len` bytes of
    /// [`BootChunk`](PeerControl::BootChunk)s follow on the same (ordered)
    /// channel.
    Boot { len: u32 },
    BootChunk(Vec<u8>),
}

/// One side's boot payload: everything a peer needs to reconstruct that
/// side's live machine when the cable plugs in. Travels peer-to-peer as
/// [`PeerControl::Boot`]+chunks, and rides in replays so playback can
/// boot the same link.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BootBlob {
    /// Serialized core state ([`mgba_siolink::Link::capture_boot_state`]).
    pub state: Vec<u8>,
    /// SRAM/flash image at capture time (core states don't carry
    /// savedata).
    pub save: Option<Vec<u8>>,
    /// The capturing side's wall clock. Every side ships one, but only
    /// the host's (player 0) seeds the link's shared RTC — clock
    /// agreement is part of the peer protocol, not the signaling
    /// server's business.
    pub clock_unix_micros: u64,
}

/// Chunk size for the boot exchange, comfortably under the negotiated
/// datachannel message cap (256 KiB).
pub const BOOT_CHUNK: usize = 64 * 1024;

/// Sanity cap on an encoded boot payload (a core state is ~400 KiB and
/// saves top out at 128 KiB, both before compression).
pub const MAX_BOOT_SIZE: usize = 4 * 1024 * 1024;

impl BootBlob {
    /// bincode + zstd; GBA RAM images compress hard, and these cross the
    /// wire once per peer at plug-in time.
    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        Ok(zstd::encode_all(&bincode::serialize(self)?[..], 0)?)
    }

    pub fn decode(bytes: &[u8]) -> anyhow::Result<BootBlob> {
        Ok(bincode::deserialize(&zstd::decode_all(bytes)?)?)
    }
}

/// Opaque signal relayed through the signaling server while building
/// the mesh: SDP descriptions and trickled ICE candidates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PeerSignal {
    Description { sdp_type: String, sdp: String },
    Candidate { candidate: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use rennet::Codec;

    #[test]
    fn meta_roundtrips() {
        for meta in [
            Meta::default(),
            Meta {
                tick_advantage: -37,
                checkpoint_tick: 123456,
                checkpoint_digest: 0xdeadbeef,
            },
        ] {
            let mut bytes = Vec::new();
            meta.encode(&mut bytes).unwrap();
            assert_eq!(Meta::decode(&mut &bytes[..]).unwrap(), Some(meta));
        }
    }

    #[test]
    fn boot_blob_roundtrips_and_compresses() {
        let blob = BootBlob {
            state: vec![0u8; 400 * 1024],
            save: Some(vec![0xffu8; 32 * 1024]),
            clock_unix_micros: 1_700_000_000_000_000,
        };
        let encoded = blob.encode().unwrap();
        assert!(encoded.len() < blob.state.len() / 4);
        assert_eq!(BootBlob::decode(&encoded).unwrap(), blob);
    }

    #[test]
    fn frame_roundtrips() {
        let f = Frame::new(
            100,
            98,
            Meta {
                tick_advantage: 3,
                checkpoint_tick: 90,
                checkpoint_digest: 42,
            },
            vec![Input(0x3ff), Input(0), Input(0x155)],
        );
        let bytes = f.to_vec();
        assert_eq!(Frame::decode(&mut &bytes[..]).unwrap(), f);
    }
}
