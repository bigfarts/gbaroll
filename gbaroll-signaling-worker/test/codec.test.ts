// Cross-implementation wire fixtures: these hex strings were produced
// by the Rust crate (prost) encoding the same messages. Decoding them
// here proves the generated TS codec reads Rust's bytes; the encode
// assertions additionally pin that both sides emit identical bytes
// (field-number order, defaults omitted — true for prost and
// protobuf-es today).

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  type ClientMessage,
  ErrorKind,
  type ServerMessage,
  clientCreateRoom,
  clientJoinRoom,
  clientKickPlayer,
  clientLeave,
  clientSetReady,
  clientSignal,
  clientStart,
  decodeClientMessage,
  decodeServerMessage,
  encodeClientMessage,
  encodeServerMessage,
  iceServer,
  serverError,
  serverHello,
  serverPeerLeft,
  serverRoomCreated,
  serverRoomJoined,
  serverRoster,
  serverSignal,
  serverStarting,
} from "../src/protocol.ts";

function hex(b: Uint8Array): string {
  return [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
}

function unhex(s: string): Uint8Array {
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) {
    out[i] = parseInt(s.slice(2 * i, 2 * i + 2), 16);
  }
  return out;
}

const CLIENT_FIXTURES: [string, string, ClientMessage][] = [
  [
    "create_room",
    "0a1c0805120672c3a93aceb118effdb6f50d22084147422d42544d4a2801",
    clientCreateRoom("ré:α", 0xdeadbeef, "AGB-BTMJ", true),
  ],
  [
    "join_room",
    "1216080512064142324333441a02703220848688082a0154",
    clientJoinRoom("AB2C3D", "p2", 0x01020304, "T"),
  ],
  ["set_ready_true", "1a020801", clientSetReady(true)],
  ["set_ready_false", "1a00", clientSetReady(false)],
  ["start", "2200", clientStart()],
  ["signal", "2a0808021204000102ff", clientSignal(2, new Uint8Array([0, 1, 2, 255]))],
  ["leave", "3200", clientLeave()],
  ["kick_player", "3a020802", clientKickPlayer(2)],
];

const SERVER_FIXTURES: [string, string, ServerMessage][] = [
  [
    "hello",
    "0a89010a1f0a1d7374756e3a7374756e2e636c6f7564666c6172652e636f6d3a333437380a660a2b7475726e3a7475726e2e636c6f7564666c6172652e636f6d3a333437383f7472616e73706f72743d7564700a2b7475726e733a7475726e2e636c6f7564666c6172652e636f6d3a3434333f7472616e73706f72743d7463701204757365721a0470617373",
    serverHello([
      iceServer(["stun:stun.cloudflare.com:3478"]),
      iceServer(
        [
          "turn:turn.cloudflare.com:3478?transport=udp",
          "turns:turn.cloudflare.com:443?transport=tcp",
        ],
        "user",
        "pass",
      ),
    ]),
  ],
  ["room_created", "12080a06414232433344", serverRoomCreated("AB2C3D")],
  ["room_joined", "1a080a06414232433344", serverRoomJoined("AB2C3D")],
  [
    "roster",
    "22280a180a04686f7374100118effdb6f50d22084147422d42544d4a0a080a0270321804280310011801",
    serverRoster(
      [
        { nick: "host", ready: true, romCrc32: 0xdeadbeef, romTitle: "AGB-BTMJ", seat: 0 },
        // A seat ahead of the position: two earlier joiners left.
        { nick: "p2", ready: false, romCrc32: 4, romTitle: "", seat: 3 },
      ],
      1,
      true,
    ),
  ],
  [
    "starting",
    "2a180a0c0a04686f737410effdb6f50d0a060a02703210041003",
    serverStarting(
      [
        { nick: "host", romCrc32: 0xdeadbeef },
        { nick: "p2", romCrc32: 4 },
      ],
      3,
    ),
  ],
  ["signal", "3200", serverSignal(0, new Uint8Array(0))],
  ["peer_left", "3a020802", serverPeerLeft(2)],
  ["error_kicked", "42020809", serverError(ErrorKind.KICKED, "")],
];

test("client messages match the Rust (prost) wire bytes", () => {
  for (const [name, bytes, msg] of CLIENT_FIXTURES) {
    assert.equal(hex(encodeClientMessage(msg)), bytes, `${name}: encode`);
    assert.deepEqual(decodeClientMessage(unhex(bytes)), msg, `${name}: decode`);
  }
});

test("server messages match the Rust (prost) wire bytes", () => {
  for (const [name, bytes, msg] of SERVER_FIXTURES) {
    assert.equal(hex(encodeServerMessage(msg)), bytes, `${name}: encode`);
    assert.deepEqual(decodeServerMessage(unhex(bytes)), msg, `${name}: decode`);
  }
});

test("malformed messages are rejected or empty", () => {
  // Truncated length-delimited field.
  assert.throws(() => decodeClientMessage(unhex("0a")));
  assert.throws(() => decodeClientMessage(unhex("0aff")));
  // An empty buffer is a valid protobuf but carries no variant — the
  // server treats msg.case === undefined as malformed.
  assert.equal(decodeClientMessage(new Uint8Array(0)).msg.case, undefined);
});
