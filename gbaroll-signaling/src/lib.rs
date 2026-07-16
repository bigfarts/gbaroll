//! The gbaroll signaling protocol: room-based rendezvous for 2–4 player
//! rollback sessions.
//!
//! The wire format is protobuf, defined once in `proto/signaling.proto`
//! (which carries the full protocol documentation) and generated into
//! Rust here and into TypeScript in `gbaroll-signaling-worker` — the
//! schema is the single source of truth, shared by client and both
//! server implementations.
//!
//! Transport: WebSocket binary messages, each one [`ClientMessage`] or
//! [`ServerMessage`]. Clients may also send a text `"ping"` as a
//! keepalive; servers reply with a text `"pong"`.

mod proto {
    include!(concat!(env!("OUT_DIR"), "/gbaroll.signaling.rs"));
}
pub use proto::*;

/// Bumped on any incompatible protocol change. Carried on the first
/// message (create/join); the server rejects mismatches.
pub const PROTOCOL_VERSION: u32 = 4;

/// Most players a room holds — the size of a real multi-cable chain.
pub const MAX_PLAYERS: usize = 4;

/// Length of a generated room code.
pub const ROOM_CODE_LEN: usize = 6;

/// Alphabet room codes are drawn from (unambiguous subset).
pub const ROOM_CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

pub fn encode<T: prost::Message>(msg: &T) -> Vec<u8> {
    msg.encode_to_vec()
}

pub fn decode<T: prost::Message + Default>(bytes: &[u8]) -> Result<T, prost::DecodeError> {
    T::decode(bytes)
}

/// Normalize a user-typed room code for lookup.
pub fn normalize_room_code(code: &str) -> String {
    code.trim().to_ascii_uppercase()
}

// Constructors so call sites don't spell out the oneof nesting.
impl ClientMessage {
    pub fn create_room(nick: impl Into<String>, rom_crc32: u32, rom_title: impl Into<String>) -> Self {
        Self {
            msg: Some(client_message::Msg::CreateRoom(CreateRoom {
                protocol_version: PROTOCOL_VERSION,
                nick: nick.into(),
                rom_crc32,
                rom_title: rom_title.into(),
            })),
        }
    }

    pub fn join_room(
        code: impl Into<String>,
        nick: impl Into<String>,
        rom_crc32: u32,
        rom_title: impl Into<String>,
    ) -> Self {
        Self {
            msg: Some(client_message::Msg::JoinRoom(JoinRoom {
                protocol_version: PROTOCOL_VERSION,
                code: code.into(),
                nick: nick.into(),
                rom_crc32,
                rom_title: rom_title.into(),
            })),
        }
    }

    pub fn set_ready(ready: bool) -> Self {
        Self {
            msg: Some(client_message::Msg::SetReady(SetReady { ready })),
        }
    }

    pub fn start() -> Self {
        Self {
            msg: Some(client_message::Msg::Start(Start {})),
        }
    }

    /// A signal for the server to relay to player `to`.
    pub fn signal(to: u32, payload: Vec<u8>) -> Self {
        Self {
            msg: Some(client_message::Msg::Signal(Signal { peer: to, payload })),
        }
    }

    pub fn leave() -> Self {
        Self {
            msg: Some(client_message::Msg::Leave(Leave {})),
        }
    }
}

impl ServerMessage {
    pub fn hello(ice_servers: Vec<IceServer>) -> Self {
        Self {
            msg: Some(server_message::Msg::Hello(Hello { ice_servers })),
        }
    }

    pub fn room_created(code: impl Into<String>) -> Self {
        Self {
            msg: Some(server_message::Msg::RoomCreated(RoomCreated { code: code.into() })),
        }
    }

    pub fn room_joined(code: impl Into<String>) -> Self {
        Self {
            msg: Some(server_message::Msg::RoomJoined(RoomJoined { code: code.into() })),
        }
    }

    pub fn roster(players: Vec<PlayerInfo>, your_idx: u32) -> Self {
        Self {
            msg: Some(server_message::Msg::Roster(Roster { players, your_idx })),
        }
    }

    pub fn starting(players: Vec<StartPlayer>, your_idx: u32) -> Self {
        Self {
            msg: Some(server_message::Msg::Starting(Starting { players, your_idx })),
        }
    }

    /// A signal relayed onward, stamped with the sender `from`.
    pub fn signal(from: u32, payload: Vec<u8>) -> Self {
        Self {
            msg: Some(server_message::Msg::Signal(Signal { peer: from, payload })),
        }
    }

    pub fn peer_left(player_idx: u32) -> Self {
        Self {
            msg: Some(server_message::Msg::PeerLeft(PeerLeft { player_idx })),
        }
    }

    pub fn error(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            msg: Some(server_message::Msg::Error(Error {
                kind: kind.into(),
                message: message.into(),
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_roundtrip() {
        let msgs = vec![
            ClientMessage::create_room("player", 0xdeadbeef, "TESTGAME"),
            ClientMessage::set_ready(true),
            ClientMessage::signal(2, vec![1, 2, 3]),
        ];
        for m in msgs {
            assert_eq!(decode::<ClientMessage>(&encode(&m)).unwrap(), m);
        }

        let s = ServerMessage::starting(
            vec![
                StartPlayer {
                    nick: "a".into(),
                    rom_crc32: 1,
                },
                StartPlayer {
                    nick: "b".into(),
                    rom_crc32: 2,
                },
            ],
            1,
        );
        assert_eq!(decode::<ServerMessage>(&encode(&s)).unwrap(), s);
    }

    #[test]
    fn room_code_normalization() {
        assert_eq!(normalize_room_code(" abc234 "), "ABC234");
    }
}
