//! Cross-implementation wire fixtures: the same hex strings live in the
//! worker's `test/codec.test.ts`. Encoding here (prost) and decoding
//! there (protobuf-es) — and vice versa — proves both codecs speak
//! byte-identical protobuf (field-number order, defaults omitted — true
//! for prost and protobuf-es today). Change a message, update BOTH
//! tables.

use gbaroll_signaling::*;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn client_fixtures() -> Vec<(&'static str, &'static str, ClientMessage)> {
    vec![
        (
            "create_room",
            "0a1c0805120672c3a93aceb118effdb6f50d22084147422d42544d4a2801",
            ClientMessage::create_room("ré:α", 0xdeadbeef, "AGB-BTMJ", true),
        ),
        (
            "join_room",
            "1216080512064142324333441a02703220848688082a0154",
            ClientMessage::join_room("AB2C3D", "p2", 0x01020304, "T"),
        ),
        ("set_ready_true", "1a020801", ClientMessage::set_ready(true)),
        ("set_ready_false", "1a00", ClientMessage::set_ready(false)),
        ("start", "2200", ClientMessage::start()),
        ("signal", "2a0808021204000102ff", ClientMessage::signal(2, vec![0, 1, 2, 255])),
        ("leave", "3200", ClientMessage::leave()),
        ("kick_player", "3a020802", ClientMessage::kick_player(2)),
    ]
}

fn server_fixtures() -> Vec<(&'static str, &'static str, ServerMessage)> {
    vec![
        (
            "roster",
            "22280a180a04686f7374100118effdb6f50d22084147422d42544d4a0a080a0270321804280310011801",
            ServerMessage::roster(
                vec![
                    PlayerInfo {
                        nick: "host".into(),
                        ready: true,
                        rom_crc32: 0xdeadbeef,
                        rom_title: "AGB-BTMJ".into(),
                        seat: 0,
                    },
                    // A seat ahead of the position: two earlier joiners left.
                    PlayerInfo {
                        nick: "p2".into(),
                        ready: false,
                        rom_crc32: 4,
                        rom_title: String::new(),
                        seat: 3,
                    },
                ],
                1,
                true,
            ),
        ),
        (
            "starting",
            "2a180a0c0a04686f737410effdb6f50d0a060a02703210041003",
            ServerMessage::starting(
                vec![
                    StartPlayer {
                        nick: "host".into(),
                        rom_crc32: 0xdeadbeef,
                    },
                    StartPlayer {
                        nick: "p2".into(),
                        rom_crc32: 4,
                    },
                ],
                3,
            ),
        ),
        ("signal", "3200", ServerMessage::signal(0, vec![])),
        ("peer_left", "3a020802", ServerMessage::peer_left(2)),
        ("error_kicked", "42020809", ServerMessage::error(ErrorKind::Kicked, "")),
    ]
}

#[test]
fn client_messages_match_the_pinned_wire_bytes() {
    for (name, bytes, msg) in client_fixtures() {
        assert_eq!(hex(&encode(&msg)), bytes, "{name}: encode");
        assert_eq!(decode::<ClientMessage>(&encode(&msg)).unwrap(), msg, "{name}: roundtrip");
    }
}

#[test]
fn server_messages_match_the_pinned_wire_bytes() {
    for (name, bytes, msg) in server_fixtures() {
        assert_eq!(hex(&encode(&msg)), bytes, "{name}: encode");
        assert_eq!(decode::<ServerMessage>(&encode(&msg)).unwrap(), msg, "{name}: roundtrip");
    }
}
