//! The gbaroll signaling protocol: room-based rendezvous for 2–4 player
//! rollback sessions.
//!
//! The server's job is small: it hosts named rooms of up to
//! [`MAX_PLAYERS`], assigns player indices, broadcasts the session start
//! (with a shared wall clock every cart RTC is pinned to), and from then
//! on blindly relays opaque peer-to-peer signals (SDP descriptions/
//! candidates) so the peers can build a full WebRTC mesh. Game data —
//! including everyone's boot state — never touches the server; the peers
//! exchange it over the mesh once it is up.
//!
//! Each player brings their *own* ROM (each GBA on a real cable has its
//! own cart, and they need not be identical — think version pairings).
//! Every peer simulates every side, so what a client needs is a local
//! copy of every player's ROM; the roster carries each player's ROM
//! identity (CRC32 + title) and the *client* checks its library and
//! refuses to ready up until it has them all. The server never verifies
//! ROM possession — it can't.
//!
//! Transport: WebSocket binary messages, each one [`ClientMessage`] or
//! [`ServerMessage`] encoded with bincode.

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible protocol change. Carried on the first
/// message (create/join); the server rejects mismatches.
pub const PROTOCOL_VERSION: u32 = 3;

/// Most players a room holds — the size of a real multi-cable chain.
pub const MAX_PLAYERS: usize = 4;

/// Length of a generated room code.
pub const ROOM_CODE_LEN: usize = 6;

/// Alphabet room codes are drawn from (unambiguous subset).
pub const ROOM_CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClientMessage {
    /// Open a new room. The sender becomes player 0 (the host).
    CreateRoom {
        protocol_version: u32,
        nick: String,
        /// CRC32 of the ROM this player's own side will run.
        rom_crc32: u32,
        /// That ROM's header-internal title, for display.
        rom_title: String,
    },
    /// Join an existing room by code, bringing your own ROM (it need not
    /// match anyone else's — but every member needs a copy of it, which
    /// clients verify against their libraries before readying up).
    JoinRoom {
        protocol_version: u32,
        code: String,
        nick: String,
        rom_crc32: u32,
        rom_title: String,
    },
    /// Flip the ready flag.
    SetReady { ready: bool },
    /// Host only: lock the room and start the session. Requires at
    /// least 2 players and every non-host player ready (the host never
    /// readies up).
    Start,
    /// Relay an opaque peer signal (SDP/candidate) to another player in
    /// the room. Only meaningful once the room has started.
    Signal { to: u8, payload: Vec<u8> },
    /// Leave the room (also implied by disconnecting).
    Leave,
}

/// A STUN/TURN server the clients should use for the mesh, handed out
/// by the server in [`ServerMessage::Hello`] (the deployment knows its
/// own infrastructure; clients shouldn't need configuring).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IceServer {
    /// URLs in RFC 7064/7065 form (`stun:host:port`, `turn:host:port`).
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlayerInfo {
    pub nick: String,
    pub ready: bool,
    /// The ROM this player's side runs; other members need a local copy.
    pub rom_crc32: u32,
    /// Raw cartridge-header title. Clients should resolve `rom_crc32`
    /// through their own game-name database for presentation.
    pub rom_title: String,
}

/// One player's identity in the start broadcast. Boot payloads (each
/// side's live capture) travel peer-to-peer once the mesh is up, not
/// through the server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartPlayer {
    pub nick: String,
    /// The ROM this player's side runs (each peer resolves it from its
    /// own library).
    pub rom_crc32: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ErrorKind {
    ProtocolVersionMismatch,
    RoomNotFound,
    RoomFull,
    RoomAlreadyStarted,
    NotHost,
    NotEveryoneReady,
    Malformed,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ServerMessage {
    /// Sent once, immediately on connect: the ICE servers to build the
    /// mesh with.
    Hello { ice_servers: Vec<IceServer> },
    /// Reply to [`ClientMessage::CreateRoom`].
    RoomCreated { code: String },
    /// Reply to [`ClientMessage::JoinRoom`].
    RoomJoined { code: String },
    /// The room's occupancy, sent to every member whenever it changes.
    /// Slot order is player order; `your_idx` is the recipient's slot
    /// (indices compact downward when someone leaves the lobby). Any
    /// occupancy change resets every ready flag. Slot 0 (the host) is
    /// always reported ready.
    Roster {
        players: Vec<PlayerInfo>,
        your_idx: u8,
    },
    /// The room is locked and the session begins. Every peer builds the
    /// same link — `players[i]`'s capture (exchanged over the mesh) on
    /// side `i`, everyone's cart RTC pinned to `clock_unix_micros`.
    Starting {
        clock_unix_micros: u64,
        players: Vec<StartPlayer>,
        your_idx: u8,
    },
    /// A relayed peer signal.
    Signal { from: u8, payload: Vec<u8> },
    /// A player disconnected after the room started (lobby departures
    /// show up as a new [`Roster`](ServerMessage::Roster) instead).
    PeerLeft { player_idx: u8 },
    Error { kind: ErrorKind, message: String },
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("encode: {0}")]
    Encode(bincode::Error),
    #[error("decode: {0}")]
    Decode(bincode::Error),
}

pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, CodecError> {
    bincode::serialize(msg).map_err(CodecError::Encode)
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, CodecError> {
    bincode::deserialize(bytes).map_err(CodecError::Decode)
}

/// Normalize a user-typed room code for lookup.
pub fn normalize_room_code(code: &str) -> String {
    code.trim().to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_roundtrip() {
        let msgs = vec![
            ClientMessage::CreateRoom {
                protocol_version: PROTOCOL_VERSION,
                nick: "player".into(),
                rom_crc32: 0xdeadbeef,
                rom_title: "TESTGAME".into(),
            },
            ClientMessage::SetReady { ready: true },
            ClientMessage::Signal {
                to: 2,
                payload: vec![1, 2, 3],
            },
        ];
        for m in msgs {
            let bytes = encode(&m).unwrap();
            assert_eq!(decode::<ClientMessage>(&bytes).unwrap(), m);
        }

        let s = ServerMessage::Starting {
            clock_unix_micros: 123,
            players: vec![
                StartPlayer {
                    nick: "a".into(),
                    rom_crc32: 1,
                },
                StartPlayer {
                    nick: "b".into(),
                    rom_crc32: 2,
                },
            ],
            your_idx: 1,
        };
        let bytes = encode(&s).unwrap();
        assert_eq!(decode::<ServerMessage>(&bytes).unwrap(), s);
    }

    #[test]
    fn room_code_normalization() {
        assert_eq!(normalize_room_code(" abc234 "), "ABC234");
    }
}
