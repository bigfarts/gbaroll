// The gbaroll signaling server, Cloudflare Workers flavor: the same
// room rendezvous + opaque signal relay as `gbaroll-signaling-server`,
// speaking the identical wire protocol (protobuf, generated from the
// shared schema — see src/protocol.ts).
//
// Every room lives in one Durable Object instance: create/join arrive
// as the first websocket *message*, so there's no room code to route on
// at upgrade time — and signaling is rendezvous-only traffic (game data
// flows peer-to-peer), so one object is plenty. Sockets use the
// hibernation API, and room state is kept exactly as long as it's
// useful:
//
//   - A *lobby* lives in the object's storage (it must survive
//     eviction) and dies with its last member.
//   - The moment a room starts, its record is DELETED. Everything the
//     brief post-start signal relay needs — the sender's frozen index
//     and the peers' socket ids — is stamped into each socket's
//     hibernation attachment, so a running session holds no server-side
//     state at all beyond its open sockets.

import { DurableObject } from "cloudflare:workers";
import {
  type ClientMessage,
  ErrorKind,
  type IceServer,
  MAX_PLAYERS,
  PROTOCOL_VERSION,
  ROOM_CODE_ALPHABET,
  ROOM_CODE_LEN,
  type ServerMessage,
  decodeClientMessage,
  encodeServerMessage,
  normalizeRoomCode,
  serverError,
  serverHello,
  serverPeerLeft,
  serverRoomCreated,
  serverRoomJoined,
  serverRoster,
  serverSignal,
  serverStarting,
} from "./protocol.ts";
import { FALLBACK_ICE_SERVERS, TurnEnv, generateIceServers } from "./turn.ts";

export interface Env extends TurnEnv {
  ROOMS: DurableObjectNamespace<Rooms>;
}

/** A socket's identity, in its hibernation attachment. */
interface Attachment {
  id: string;
  /** The lobby this socket sits in, once a create/join is accepted.
   * Cleared when the session starts. */
  code?: string;
  /** Set when the session starts: everything the signal relay needs,
   * riding the socket so the room record can be deleted (the room code
   * means nothing from that moment — it's free for reuse). Indices are
   * frozen core indices — the relay addresses by them. */
  session?: {
    idx: number;
    /** Every player's socket id, in player order. */
    peerIds: string[];
  };
}

interface StoredPlayer {
  id: string;
  nick: string;
  ready: boolean;
  romCrc32: number;
  romTitle: string;
}

/** A lobby. (Started rooms don't exist in storage — see above.) */
interface StoredRoom {
  createdMs: number;
  players: StoredPlayer[];
}

const ROOM_PREFIX = "room:";
/** Lobbies die with their last socket; the hourly sweep only collects
 * ones whose close events were lost (an abandoned lobby is one with no
 * live sockets left). The age cap is a belt-and-braces bound on top. */
const SWEEP_PERIOD_MS = 60 * 60 * 1000;
const LOBBY_MAX_AGE_MS = 24 * 60 * 60 * 1000;
/** How long one minted TURN credential is re-handed to new connections.
 * Its ttl outlasts this by a day, so late joiners still get a
 * session's worth of validity. */
const ICE_CACHE_MS = 60 * 60 * 1000;
const ICE_RETRY_MS = 60 * 1000;

export class Rooms extends DurableObject<Env> {
  private ice: { servers: IceServer[]; freshUntilMs: number } | null = null;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    // The application-level keepalive: clients send a text "ping" and
    // get the reply without waking the object.
    this.ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  async fetch(request: Request): Promise<Response> {
    if (request.headers.get("Upgrade")?.toLowerCase() !== "websocket") {
      return new Response("expected a websocket", { status: 426 });
    }
    const pair = new WebSocketPair();
    const id = crypto.randomUUID();
    this.ctx.acceptWebSocket(pair[1], [`id:${id}`]);
    pair[1].serializeAttachment({ id } satisfies Attachment);
    // Greet with the ICE servers the mesh should use.
    this.send(pair[1], serverHello(await this.iceServers()));
    return new Response(null, { status: 101, webSocket: pair[0] });
  }

  async webSocketMessage(ws: WebSocket, message: ArrayBuffer | string): Promise<void> {
    if (typeof message === "string") {
      // "ping" is answered by the auto-response without reaching here.
      this.sendError(ws, ErrorKind.MALFORMED, "expected a binary message");
      return;
    }
    let msg: ClientMessage;
    try {
      msg = decodeClientMessage(new Uint8Array(message));
    } catch {
      this.sendError(ws, ErrorKind.MALFORMED, "undecodable message");
      return;
    }
    const m = (ws.deserializeAttachment() ?? {}) as Attachment;
    switch (msg.msg.case) {
      case "createRoom":
        return this.createRoom(ws, m, msg.msg.value);
      case "joinRoom":
        return this.joinRoom(ws, m, msg.msg.value);
      case "setReady":
        return this.setReady(m, msg.msg.value.ready);
      case "start":
        return this.start(ws, m);
      case "signal":
        return this.signal(ws, m, msg.msg.value.peer, msg.msg.value.payload);
      case "leave":
        await this.leave(ws, m);
        ws.close(1000, "left");
        return;
      default:
        this.sendError(ws, ErrorKind.MALFORMED, "undecodable message");
        return;
    }
  }

  async webSocketClose(ws: WebSocket): Promise<void> {
    await this.leave(ws, (ws.deserializeAttachment() ?? {}) as Attachment);
  }

  async webSocketError(ws: WebSocket, error: unknown): Promise<void> {
    // Not always followed by a close event; leaving twice is a no-op.
    console.warn("websocket error:", error);
    await this.leave(ws, (ws.deserializeAttachment() ?? {}) as Attachment);
  }

  private async createRoom(
    ws: WebSocket,
    m: Attachment,
    msg: { protocolVersion: number; nick: string; romCrc32: number; romTitle: string },
  ): Promise<void> {
    if (m.code !== undefined || m.session !== undefined) {
      this.sendError(ws, ErrorKind.MALFORMED, "already in a room");
      return;
    }
    if (msg.protocolVersion !== PROTOCOL_VERSION) {
      this.sendError(ws, ErrorKind.PROTOCOL_VERSION_MISMATCH, "update your client");
      ws.close(1000, "protocol version mismatch");
      return;
    }
    let code: string;
    do {
      code = randomCode();
    } while ((await this.getRoom(code)) !== undefined);
    const room: StoredRoom = {
      createdMs: Date.now(),
      players: [
        {
          id: m.id,
          nick: sanitizeNick(msg.nick),
          ready: false,
          romCrc32: msg.romCrc32,
          romTitle: msg.romTitle,
        },
      ],
    };
    await this.putRoom(code, room);
    ws.serializeAttachment({ id: m.id, code } satisfies Attachment);
    console.log(`created room ${code} (${msg.romTitle})`);
    this.send(ws, serverRoomCreated(code));
    this.broadcastRoster(room);
    if ((await this.ctx.storage.getAlarm()) === null) {
      await this.ctx.storage.setAlarm(Date.now() + SWEEP_PERIOD_MS);
    }
  }

  private async joinRoom(
    ws: WebSocket,
    m: Attachment,
    msg: { protocolVersion: number; code: string; nick: string; romCrc32: number; romTitle: string },
  ): Promise<void> {
    if (m.code !== undefined || m.session !== undefined) {
      this.sendError(ws, ErrorKind.MALFORMED, "already in a room");
      return;
    }
    if (msg.protocolVersion !== PROTOCOL_VERSION) {
      this.sendError(ws, ErrorKind.PROTOCOL_VERSION_MISMATCH, "update your client");
      ws.close(1000, "protocol version mismatch");
      return;
    }
    const code = normalizeRoomCode(msg.code);
    const room = await this.getRoom(code);
    if (room === undefined) {
      // Started rooms land here too: their records are deleted the
      // moment they start, indistinguishable from never existing.
      this.sendError(ws, ErrorKind.ROOM_NOT_FOUND, "no such room");
      return;
    }
    if (room.players.length >= MAX_PLAYERS) {
      this.sendError(ws, ErrorKind.ROOM_FULL, "room is full");
      return;
    }
    // Occupancy changed: whatever anyone agreed to ready up for no
    // longer describes the room, so ready state resets for everyone.
    for (const p of room.players) p.ready = false;
    room.players.push({
      id: m.id,
      nick: sanitizeNick(msg.nick),
      ready: false,
      romCrc32: msg.romCrc32,
      romTitle: msg.romTitle,
    });
    await this.putRoom(code, room);
    ws.serializeAttachment({ id: m.id, code } satisfies Attachment);
    console.log(`joined room ${code}`);
    this.send(ws, serverRoomJoined(code));
    this.broadcastRoster(room);
  }

  private async setReady(m: Attachment, ready: boolean): Promise<void> {
    const loc = await this.locate(m);
    if (loc === undefined) return;
    if (loc.idx === 0) {
      // The host doesn't ready up.
      return;
    }
    loc.room.players[loc.idx].ready = ready;
    await this.putRoom(loc.code, loc.room);
    this.broadcastRoster(loc.room);
  }

  private async start(ws: WebSocket, m: Attachment): Promise<void> {
    const loc = await this.locate(m);
    if (loc === undefined) return;
    const { code, room, idx } = loc;
    if (idx !== 0) {
      this.sendError(ws, ErrorKind.NOT_HOST, "only the host can start");
      return;
    }
    if (room.players.length < 2) {
      this.sendError(ws, ErrorKind.NOT_EVERYONE_READY, "need 2+ players");
      return;
    }
    // Stamp every member's socket with the relay routing; the room
    // record's job ends right here, so delete it — a running session
    // keeps no state but its sockets.
    const peerIds = room.players.map((p) => p.id);
    room.players.forEach((p, i) => {
      for (const peer of this.ctx.getWebSockets(`id:${p.id}`)) {
        peer.serializeAttachment({
          id: p.id,
          session: { idx: i, peerIds },
        } satisfies Attachment);
      }
    });
    await this.ctx.storage.delete(ROOM_PREFIX + code);
    const players = room.players.map((p) => ({ nick: p.nick, romCrc32: p.romCrc32 }));
    console.log(`room ${code} started with ${players.length} players; record deleted`);
    room.players.forEach((p, i) => {
      this.sendTo(p.id, serverStarting(players, i));
    });
  }

  private signal(ws: WebSocket, m: Attachment, to: number, payload: Uint8Array): void {
    const s = m.session;
    if (s === undefined) {
      if (m.code !== undefined) {
        this.sendError(ws, ErrorKind.MALFORMED, "room hasn't started");
      }
      return;
    }
    const target = s.peerIds[to];
    if (target === undefined || to === s.idx) return;
    // A departed target simply has no socket left; the signal drops.
    this.sendTo(target, serverSignal(s.idx, payload));
  }

  private async leave(ws: WebSocket, m: Attachment): Promise<void> {
    if (m.session !== undefined) {
      // Post-start there's no room state to update — just tell the
      // peers. Clear the session first so the close event that follows
      // a Leave doesn't broadcast twice.
      try {
        ws.serializeAttachment({ id: m.id } satisfies Attachment);
      } catch {
        // Socket already unusable; a duplicate PeerLeft is harmless
        // (clients treat it idempotently).
      }
      for (const peer of m.session.peerIds) {
        if (peer !== m.id) this.sendTo(peer, serverPeerLeft(m.session.idx));
      }
      return;
    }
    if (m.code === undefined) return;
    const room = await this.getRoom(m.code);
    if (room === undefined) return;
    const idx = room.players.findIndex((p) => p.id === m.id);
    if (idx < 0) return;
    room.players.splice(idx, 1);
    if (room.players.length > 0) {
      for (const p of room.players) p.ready = false;
      await this.putRoom(m.code, room);
      this.broadcastRoster(room);
    } else {
      await this.ctx.storage.delete(ROOM_PREFIX + m.code);
      console.log(`room ${m.code} closed`);
    }
  }

  /** Collect lobbies whose close events were lost: no live sockets
   * (or, belt-and-braces, just too old). */
  async alarm(): Promise<void> {
    const rooms = await this.ctx.storage.list<StoredRoom>({ prefix: ROOM_PREFIX });
    const now = Date.now();
    let live = 0;
    for (const [key, room] of rooms) {
      const abandoned = room.players.every((p) => this.ctx.getWebSockets(`id:${p.id}`).length === 0);
      if (!abandoned && now - room.createdMs <= LOBBY_MAX_AGE_MS) {
        live += 1;
        continue;
      }
      await this.ctx.storage.delete(key);
      console.log(`room ${key.slice(ROOM_PREFIX.length)} swept`);
      for (const p of room.players) {
        for (const ws of this.ctx.getWebSockets(`id:${p.id}`)) {
          ws.close(1000, "room expired");
        }
      }
    }
    if (live > 0) {
      await this.ctx.storage.setAlarm(now + SWEEP_PERIOD_MS);
    }
  }

  private getRoom(code: string): Promise<StoredRoom | undefined> {
    return this.ctx.storage.get<StoredRoom>(ROOM_PREFIX + code);
  }

  private putRoom(code: string, room: StoredRoom): Promise<void> {
    return this.ctx.storage.put(ROOM_PREFIX + code, room);
  }

  /** Where this attachment sits in its lobby: the room and the
   * *current* index (departures compact indices downward, so it's
   * derived fresh from the stored room every time). */
  private async locate(
    m: Attachment,
  ): Promise<{ code: string; room: StoredRoom; idx: number } | undefined> {
    if (m.code === undefined) return undefined;
    const room = await this.getRoom(m.code);
    if (room === undefined) return undefined;
    const idx = room.players.findIndex((p) => p.id === m.id);
    if (idx < 0) return undefined;
    return { code: m.code, room, idx };
  }

  private send(ws: WebSocket, msg: ServerMessage): void {
    try {
      ws.send(encodeServerMessage(msg));
    } catch {
      // A closing socket; its close event does the cleanup.
    }
  }

  private sendError(ws: WebSocket, kind: ErrorKind, detail: string): void {
    // The wire carries only the kind — clients own the user-facing
    // words; the detail stays here in the logs.
    console.log(`error ${ErrorKind[kind]}: ${detail}`);
    this.send(ws, serverError(kind, ""));
  }

  private sendTo(playerId: string, msg: ServerMessage): void {
    for (const ws of this.ctx.getWebSockets(`id:${playerId}`)) {
      this.send(ws, msg);
    }
  }

  /** Send each member the roster, stamped with their own index. */
  private broadcastRoster(room: StoredRoom): void {
    const players = room.players.map((p, i) => ({
      nick: p.nick,
      // The host never readies up; their seat always reads ready.
      ready: i === 0 || p.ready,
      romCrc32: p.romCrc32,
      romTitle: p.romTitle,
    }));
    room.players.forEach((p, i) => {
      this.sendTo(p.id, serverRoster(players, i));
    });
  }

  /** The ICE servers handed to every client: Cloudflare TURN when a key
   * is configured, a STUN-only fallback otherwise. Minted credentials
   * are reused across connections for a while (they stay valid far
   * longer; see ICE_CACHE_MS). */
  private async iceServers(): Promise<IceServer[]> {
    const now = Date.now();
    if (this.ice !== null && now < this.ice.freshUntilMs) return this.ice.servers;
    try {
      const servers = await generateIceServers(this.env);
      this.ice = {
        servers: servers ?? FALLBACK_ICE_SERVERS,
        freshUntilMs: now + ICE_CACHE_MS,
      };
    } catch (e) {
      console.error("handing out STUN-only fallback:", e);
      this.ice = { servers: FALLBACK_ICE_SERVERS, freshUntilMs: now + ICE_RETRY_MS };
    }
    return this.ice.servers;
  }
}

function sanitizeNick(nick: string): string {
  // [...s] iterates Unicode scalar values, matching the Rust side's
  // chars().take(24).
  const n = [...nick.trim()].slice(0, 24).join("");
  return n === "" ? "player" : n;
}

function randomCode(): string {
  const out: string[] = [];
  const buf = new Uint8Array(2 * ROOM_CODE_LEN);
  while (out.length < ROOM_CODE_LEN) {
    crypto.getRandomValues(buf);
    for (const b of buf) {
      // Rejection-sampled 5-bit draws: no modulo bias over the
      // 31-letter alphabet.
      const v = b & 0x1f;
      if (v < ROOM_CODE_ALPHABET.length && out.length < ROOM_CODE_LEN) {
        out.push(ROOM_CODE_ALPHABET[v]);
      }
    }
  }
  return out.join("");
}

export default {
  async fetch(request, env): Promise<Response> {
    if (request.headers.get("Upgrade")?.toLowerCase() !== "websocket") {
      return new Response("gbaroll signaling server; connect with a websocket\n", {
        status: 200,
        headers: { "content-type": "text/plain" },
      });
    }
    return env.ROOMS.get(env.ROOMS.idFromName("rooms")).fetch(request);
  },
} satisfies ExportedHandler<Env>;
