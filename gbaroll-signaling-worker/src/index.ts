// The gbaroll signaling server, Cloudflare Workers flavor: the same
// room rendezvous + opaque signal relay as `gbaroll-signaling-server`,
// speaking the identical wire protocol (protobuf, generated from the
// shared schema — see src/protocol.ts).
//
// A room is a persistent rendezvous with dynamic membership — players
// join and leave at any time, and there is no teardown at session
// start: the record lives, and stays joinable, until its last member
// goes. When the room *merges* (broadcasts Starting) depends on its
// peripheral: wireless rooms merge on their own whenever 2+ members
// all report ready with a membership that differs from the last merge
// (RFU games handle members drifting in and out); cable rooms gather
// until position 0 sends Start — a cable game enumerates its chain at
// its link menu and can't absorb consoles mid-game — and Start fires
// again later to fold late joiners in.
//
// Every room lives in one Durable Object instance: create/join arrive
// as the first websocket *message*, so there's no room code to route on
// at upgrade time — and signaling is rendezvous-only traffic (game data
// flows peer-to-peer), so one object is plenty. Sockets use the
// hibernation API. The room record (it must survive eviction) holds the
// roster; each merge additionally stamps every member's socket
// attachment with the relay routing — the sender's merge index and the
// peers' socket ids — so relaying a signal never needs the record.

import { DurableObject } from "cloudflare:workers";
import {
  type ClientMessage,
  ErrorKind,
  type IceServer,
  PROTOCOL_VERSION,
  ROOM_CODE_ALPHABET,
  ROOM_CODE_LEN,
  type ServerMessage,
  decodeClientMessage,
  encodeServerMessage,
  maxPlayers,
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
  /** The room this socket sits in, once a create/join is accepted.
   * Held for the whole membership — merges don't clear it. */
  code?: string;
  /** Set at each merge: everything the signal relay needs, riding the
   * socket so relaying never reads the room record. Indices are that
   * merge's player indices — the relay addresses by them, and a new
   * merge overwrites them wholesale. */
  session?: {
    idx: number;
    /** Every merged player's socket id, in player order. */
    peerIds: string[];
  };
}

interface StoredPlayer {
  id: string;
  nick: string;
  ready: boolean;
  romCrc32: number;
  romTitle: string;
  /** The stable roster token (see PlayerInfo.seat in the schema):
   * positions compact on departure, seats never move or get reused, so
   * kicks addressed by seat can't land on the wrong player. */
  seat: number;
}

interface StoredRoom {
  createdMs: number;
  players: StoredPlayer[];
  /** The next unused seat token. */
  nextSeat: number;
  /** The room's link peripheral, from CreateRoom; echoed on rosters. */
  wireless: boolean;
  /** Socket ids of the last merge, in player order. Blocks a pointless
   * re-merge of unchanged membership — until a member of it withdraws
   * ready, which voids the merge (the retry path; see setReady). */
  lastMerge?: string[];
}

const ROOM_PREFIX = "room:";
/** Rooms die with their last socket; the hourly sweep only collects
 * ones whose close events were lost (an abandoned room is one with no
 * live sockets left). The age cap is a belt-and-braces bound on top. */
const SWEEP_PERIOD_MS = 60 * 60 * 1000;
const ROOM_MAX_AGE_MS = 24 * 60 * 60 * 1000;
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
        return this.startRoom(ws, m);
      case "signal":
        return this.signal(ws, m, msg.msg.value.peer, msg.msg.value.payload);
      case "kickPlayer":
        return this.kickPlayer(ws, m, msg.msg.value.seat);
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
    msg: {
      protocolVersion: number;
      nick: string;
      romCrc32: number;
      romTitle: string;
      wireless: boolean;
    },
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
          seat: 0,
        },
      ],
      nextSeat: 1,
      wireless: msg.wireless,
    };
    await this.putRoom(code, room);
    ws.serializeAttachment({ id: m.id, code } satisfies Attachment);
    console.log(`created room ${code} (${msg.romTitle}, ${msg.wireless ? "wireless" : "cable"})`);
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
      this.sendError(ws, ErrorKind.ROOM_NOT_FOUND, "no such room");
      return;
    }
    if (room.players.length >= maxPlayers(room.wireless)) {
      this.sendError(ws, ErrorKind.ROOM_FULL, "room is full");
      return;
    }
    // Occupancy changed: whatever anyone agreed to ready up for no
    // longer describes the room, so ready state resets for everyone.
    // Members mid-session simply stay merged until everyone re-asserts,
    // then the merge that folds the newcomer in fires.
    for (const p of room.players) p.ready = false;
    room.players.push({
      id: m.id,
      nick: sanitizeNick(msg.nick),
      ready: false,
      romCrc32: msg.romCrc32,
      romTitle: msg.romTitle,
      seat: room.nextSeat++,
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
    loc.room.players[loc.idx].ready = ready;
    // Withdrawing readiness from a member of the last merge voids it:
    // that merge failed for them (or its session died under them), so
    // the next all-ready convergence must fire a fresh Starting even
    // for unchanged membership. Withdrawals from anyone else — a
    // newcomer missing a ROM — leave the standing merge alone.
    if (!ready && loc.room.lastMerge?.includes(m.id)) {
      loc.room.lastMerge = undefined;
    }
    await this.putRoom(loc.code, loc.room);
    this.broadcastRoster(loc.room);
    await this.tryMerge(loc.code, loc.room);
  }

  /** Wireless rooms merge on their own when they converge: 2+ members,
   * every ready bit set, and a membership the last merge doesn't
   * already cover. Cable rooms never auto-merge — a cable game can't
   * absorb consoles mid-game, so they gather until position 0 sends
   * Start. */
  private async tryMerge(code: string, room: StoredRoom): Promise<void> {
    if (!room.wireless) {
      return;
    }
    if (room.players.length < 2 || !room.players.every((p) => p.ready)) {
      return;
    }
    const ids = room.players.map((p) => p.id);
    if (room.lastMerge !== undefined && ids.join() === room.lastMerge.join()) {
      return;
    }
    await this.merge(code, room);
  }

  /** Position 0 links a cable room up — including again later, folding
   * late joiners in once everyone is back at a link menu. */
  private async startRoom(ws: WebSocket, m: Attachment): Promise<void> {
    const loc = await this.locate(m);
    if (loc === undefined) return;
    if (loc.room.wireless) {
      // Wireless rooms merge on their own; there's nothing to request.
      this.sendError(ws, ErrorKind.MALFORMED, "wireless rooms merge on their own");
      return;
    }
    if (loc.idx !== 0) {
      this.sendError(ws, ErrorKind.NOT_HOST, "only position 0 can link the room up");
      return;
    }
    if (loc.room.players.length < 2 || !loc.room.players.every((p) => p.ready)) {
      this.sendError(ws, ErrorKind.NOT_EVERYONE_READY, "need 2+ players, all ready");
      return;
    }
    await this.merge(loc.code, loc.room);
  }

  /** Broadcast a merge: stamp the relay routing onto every member's
   * socket and send Starting — repeatedly over a room's life. */
  private async merge(code: string, room: StoredRoom): Promise<void> {
    const ids = room.players.map((p) => p.id);
    room.lastMerge = ids;
    await this.putRoom(code, room);
    room.players.forEach((p, i) => {
      for (const peer of this.ctx.getWebSockets(`id:${p.id}`)) {
        peer.serializeAttachment({
          id: p.id,
          code,
          session: { idx: i, peerIds: ids },
        } satisfies Attachment);
      }
    });
    const players = room.players.map((p) => ({ nick: p.nick, romCrc32: p.romCrc32 }));
    console.log(`room ${code} merging ${players.length} players`);
    room.players.forEach((p, i) => {
      this.sendTo(p.id, serverStarting(players, i));
    });
  }

  private signal(ws: WebSocket, m: Attachment, to: number, payload: Uint8Array): void {
    const s = m.session;
    if (s === undefined) {
      if (m.code !== undefined) {
        this.sendError(ws, ErrorKind.MALFORMED, "the room hasn't merged");
      }
      return;
    }
    const target = s.peerIds[to];
    if (target === undefined || to === s.idx) return;
    // A departed target simply has no socket left; the signal drops.
    this.sendTo(target, serverSignal(s.idx, payload));
  }

  private async kickPlayer(ws: WebSocket, m: Attachment, seat: number): Promise<void> {
    const loc = await this.locate(m);
    if (loc === undefined) return;
    if (loc.idx !== 0) {
      this.sendError(ws, ErrorKind.NOT_HOST, "only position 0 can kick");
      return;
    }
    // Kicks address seats, not positions, so one racing a departure
    // can't land on whoever slid into the vacated slot — a gone seat
    // just bounces. So does position 0: the sender can't kick
    // themselves.
    const target = loc.room.players.findIndex((p) => p.seat === seat);
    if (target <= 0) {
      this.sendError(ws, ErrorKind.MALFORMED, "no such player to kick");
      return;
    }
    const [kicked] = loc.room.players.splice(target, 1);
    for (const p of loc.room.players) p.ready = false;
    await this.putRoom(loc.code, loc.room);
    console.log(`kicked seat ${seat} from room ${loc.code}`);
    for (const kws of this.ctx.getWebSockets(`id:${kicked.id}`)) {
      // A mid-merge kick: peers still building that merge's mesh must
      // stop waiting for the kicked edge (established sessions notice
      // on their own).
      const km = (kws.deserializeAttachment() ?? {}) as Attachment;
      if (km.session !== undefined) {
        for (const peer of km.session.peerIds) {
          if (peer !== kicked.id) this.sendTo(peer, serverPeerLeft(km.session.idx));
        }
      }
      // Detach from the room before closing so the close event's
      // leave() is a clean no-op.
      try {
        kws.serializeAttachment({ id: kicked.id } satisfies Attachment);
      } catch {
        // Socket already unusable; it's out of the room regardless.
      }
      this.send(kws, serverError(ErrorKind.KICKED, ""));
      kws.close(1000, "kicked");
    }
    this.broadcastRoster(loc.room);
  }

  private async leave(ws: WebSocket, m: Attachment): Promise<void> {
    // Detach first, so the close event that follows an explicit Leave
    // runs this at most once.
    try {
      ws.serializeAttachment({ id: m.id } satisfies Attachment);
    } catch {
      // Socket already unusable; the departure proceeds regardless. A
      // duplicate PeerLeft is harmless (clients treat it idempotently).
    }
    if (m.session !== undefined) {
      // Tell the newest merge's peers directly: an established session
      // notices the loss on its own (the transport is peer-to-peer),
      // but a peer still building that merge's mesh must stop waiting
      // for the departed edge.
      for (const peer of m.session.peerIds) {
        if (peer !== m.id) this.sendTo(peer, serverPeerLeft(m.session.idx));
      }
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

  /** Collect rooms whose close events were lost: no live sockets
   * (or, belt-and-braces, just too old). */
  async alarm(): Promise<void> {
    const rooms = await this.ctx.storage.list<StoredRoom>({ prefix: ROOM_PREFIX });
    const now = Date.now();
    let live = 0;
    for (const [key, room] of rooms) {
      const abandoned = room.players.every((p) => this.ctx.getWebSockets(`id:${p.id}`).length === 0);
      if (!abandoned && now - room.createdMs <= ROOM_MAX_AGE_MS) {
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

  private async getRoom(code: string): Promise<StoredRoom | undefined> {
    const room = await this.ctx.storage.get<StoredRoom>(ROOM_PREFIX + code);
    if (room === undefined) return undefined;
    // Rooms stored by older deploys (hibernating sockets outlive
    // deploys): backfill fields they predate. Pre-seat positions were
    // stable for the room's whole life so far, so they make faithful
    // seat tokens; a pre-wireless room was necessarily a cable one.
    if (typeof room.nextSeat !== "number") {
      room.players.forEach((p, i) => (p.seat = i));
      room.nextSeat = room.players.length;
    }
    if (typeof room.wireless !== "boolean") {
      room.wireless = false;
    }
    return room;
  }

  private putRoom(code: string, room: StoredRoom): Promise<void> {
    return this.ctx.storage.put(ROOM_PREFIX + code, room);
  }

  /** Where this attachment sits in its room: the record and the
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
    const players = room.players.map((p) => ({
      nick: p.nick,
      ready: p.ready,
      romCrc32: p.romCrc32,
      romTitle: p.romTitle,
      seat: p.seat,
    }));
    room.players.forEach((p, i) => {
      this.sendTo(p.id, serverRoster(players, i, room.wireless));
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
