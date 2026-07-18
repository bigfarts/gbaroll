// The gbaroll signaling protocol, TypeScript flavor: thin helpers over
// the protobuf-es code generated from the shared schema
// (../gbaroll-signaling/proto/signaling.proto — the single source of
// truth, also consumed by the Rust crate via prost). Regenerate with
// `pnpm gen`.

import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import type { MessageInitShape } from "@bufbuild/protobuf";
import {
  type ClientMessage,
  ClientMessageSchema,
  ErrorKind,
  type IceServer,
  IceServerSchema,
  PlayerInfoSchema,
  type ServerMessage,
  ServerMessageSchema,
  StartPlayerSchema,
} from "./gen/signaling_pb.js";

export * from "./gen/signaling_pb.js";

/// Bumped on any incompatible protocol change. Carried on the first
/// message (create/join); the server rejects mismatches.
export const PROTOCOL_VERSION = 5;

/** Most players a cable room holds — the size of a real multi-cable
 * chain. */
export const MAX_CABLE_PLAYERS = 4;

/** Most players a wireless room holds — one full RFU group: a host
 * plus its four clients. (The emulated airwaves underneath are
 * uncapped; this is room policy, and the knob to turn for union-room
 * experiments.) */
export const MAX_WIRELESS_PLAYERS = 5;

/** The capacity of a room with the given peripheral. */
export function maxPlayers(wireless: boolean): number {
  return wireless ? MAX_WIRELESS_PLAYERS : MAX_CABLE_PLAYERS;
}

/** Length of a generated room code. */
export const ROOM_CODE_LEN = 6;

/** Alphabet room codes are drawn from (unambiguous subset). */
export const ROOM_CODE_ALPHABET = "23456789ABCDEFGHJKMNPQRSTUVWXYZ";

/** Normalize a user-typed room code for lookup (ASCII uppercase, like
 * the Rust side's `to_ascii_uppercase`). */
export function normalizeRoomCode(code: string): string {
  return code.trim().replace(/[a-z]/g, (c) => c.toUpperCase());
}

export function encodeServerMessage(msg: ServerMessage): Uint8Array {
  return toBinary(ServerMessageSchema, msg);
}

export function encodeClientMessage(msg: ClientMessage): Uint8Array {
  return toBinary(ClientMessageSchema, msg);
}

/** Throws on undecodable bytes. A decoded message may still have no
 * `msg.case` (an empty or unknown-variant message) — servers treat that
 * as malformed. */
export function decodeClientMessage(bytes: Uint8Array): ClientMessage {
  return fromBinary(ClientMessageSchema, bytes);
}

export function decodeServerMessage(bytes: Uint8Array): ServerMessage {
  return fromBinary(ServerMessageSchema, bytes);
}

export function iceServer(urls: string[], username?: string, credential?: string): IceServer {
  return create(IceServerSchema, { urls, username, credential });
}

// Constructors so call sites don't spell out the oneof nesting,
// mirroring the Rust crate's helpers.

export function serverHello(iceServers: IceServer[]): ServerMessage {
  return create(ServerMessageSchema, { msg: { case: "hello", value: { iceServers } } });
}

export function serverRoomCreated(code: string): ServerMessage {
  return create(ServerMessageSchema, { msg: { case: "roomCreated", value: { code } } });
}

export function serverRoomJoined(code: string): ServerMessage {
  return create(ServerMessageSchema, { msg: { case: "roomJoined", value: { code } } });
}

export function serverRoster(
  players: MessageInitShape<typeof PlayerInfoSchema>[],
  yourIdx: number,
  wireless: boolean,
): ServerMessage {
  return create(ServerMessageSchema, {
    msg: { case: "roster", value: { players, yourIdx, wireless } },
  });
}

export function serverStarting(
  players: MessageInitShape<typeof StartPlayerSchema>[],
  yourIdx: number,
): ServerMessage {
  return create(ServerMessageSchema, {
    msg: { case: "starting", value: { players, yourIdx } },
  });
}

/** A signal relayed onward, stamped with the sender `from`. */
export function serverSignal(from: number, payload: Uint8Array): ServerMessage {
  return create(ServerMessageSchema, { msg: { case: "signal", value: { peer: from, payload } } });
}

export function serverPeerLeft(playerIdx: number): ServerMessage {
  return create(ServerMessageSchema, { msg: { case: "peerLeft", value: { playerIdx } } });
}

export function serverError(kind: ErrorKind, message: string): ServerMessage {
  return create(ServerMessageSchema, { msg: { case: "error", value: { kind, message } } });
}

export function clientCreateRoom(
  nick: string,
  romCrc32: number,
  romTitle: string,
  wireless: boolean,
  protocolVersion = PROTOCOL_VERSION,
): ClientMessage {
  return create(ClientMessageSchema, {
    msg: { case: "createRoom", value: { protocolVersion, nick, romCrc32, romTitle, wireless } },
  });
}

export function clientJoinRoom(
  code: string,
  nick: string,
  romCrc32: number,
  romTitle: string,
  protocolVersion = PROTOCOL_VERSION,
): ClientMessage {
  return create(ClientMessageSchema, {
    msg: { case: "joinRoom", value: { protocolVersion, code, nick, romCrc32, romTitle } },
  });
}

export function clientSetReady(ready: boolean): ClientMessage {
  return create(ClientMessageSchema, { msg: { case: "setReady", value: { ready } } });
}

/** Position 0 only, cable rooms only: link the room up (again). */
export function clientStart(): ClientMessage {
  return create(ClientMessageSchema, { msg: { case: "start", value: {} } });
}

/** A signal for the server to relay to player `to`. */
export function clientSignal(to: number, payload: Uint8Array): ClientMessage {
  return create(ClientMessageSchema, { msg: { case: "signal", value: { peer: to, payload } } });
}

export function clientLeave(): ClientMessage {
  return create(ClientMessageSchema, { msg: { case: "leave", value: {} } });
}

/** Host only: throw the player holding `seat` (the stable roster
 * token, not the compacting position) out of the lobby. */
export function clientKickPlayer(seat: number): ClientMessage {
  return create(ClientMessageSchema, { msg: { case: "kickPlayer", value: { seat } } });
}
