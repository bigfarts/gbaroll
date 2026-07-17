// End-to-end exercise of a running worker over the real wire protocol:
// create/join/ready/start/signal-relay/leave, plus the error paths and
// the text-ping keepalive.
//
//   pnpm dev                 # terminal 1
//   node scripts/smoke.ts    # terminal 2 (or: node scripts/smoke.ts wss://…)

// The `ws` package rather than node's built-in WebSocket: undici
// rejects Cloudflare's `client_max_window_bits=15` permessage-deflate
// answer (browsers accept it), so compression is turned off outright.
import { WebSocket } from "ws";

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
  decodeServerMessage,
  encodeClientMessage,
} from "../src/protocol.ts";

const URL = process.argv[2] ?? "ws://127.0.0.1:8787/";

type ServerCase = NonNullable<ServerMessage["msg"]["case"]>;
type Incoming = ServerMessage | "closed" | { text: string };

class Client {
  private ws: WebSocket;
  private pending: Incoming[] = [];
  private waiters: ((m: Incoming) => void)[] = [];

  private constructor(ws: WebSocket) {
    this.ws = ws;
    ws.binaryType = "arraybuffer";
    ws.addEventListener("message", (ev) => {
      if (typeof ev.data === "string") this.deliver({ text: ev.data });
      else this.deliver(decodeServerMessage(new Uint8Array(ev.data as ArrayBuffer)));
    });
    ws.addEventListener("close", () => this.deliver("closed"));
  }

  private deliver(m: Incoming): void {
    const w = this.waiters.shift();
    if (w) w(m);
    else this.pending.push(m);
  }

  static connect(url: string): Promise<Client> {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(url, { perMessageDeflate: false });
      const client = new Client(ws);
      ws.addEventListener("open", () => resolve(client), { once: true });
      ws.addEventListener("error", () => reject(new Error(`can't connect to ${url}`)), { once: true });
    });
  }

  next(): Promise<Incoming> {
    if (this.pending.length > 0) return Promise.resolve(this.pending.shift()!);
    return new Promise((resolve, reject) => {
      const t = setTimeout(() => reject(new Error("timed out waiting for a message")), 5000);
      this.waiters.push((m) => {
        clearTimeout(t);
        resolve(m);
      });
    });
  }

  async expect<C extends ServerCase>(c: C): Promise<Extract<ServerMessage["msg"], { case: C }>["value"]> {
    const m = await this.next();
    if (m === "closed" || "text" in m || m.msg.case !== c) {
      throw new Error(`expected ${c}, got ${JSON.stringify(m)}`);
    }
    return m.msg.value as Extract<ServerMessage["msg"], { case: C }>["value"];
  }

  async expectText(text: string): Promise<void> {
    const m = await this.next();
    if (m === "closed" || !("text" in m) || m.text !== text) {
      throw new Error(`expected text ${text}, got ${JSON.stringify(m)}`);
    }
  }

  async expectClosed(): Promise<void> {
    const m = await this.next();
    if (m !== "closed") throw new Error(`expected close, got ${JSON.stringify(m)}`);
  }

  send(msg: ClientMessage): void {
    this.ws.send(encodeClientMessage(msg));
  }

  sendText(text: string): void {
    this.ws.send(text);
  }

  close(): void {
    this.ws.close();
  }
}

function assert(cond: boolean, what: string): void {
  if (!cond) throw new Error(`assertion failed: ${what}`);
  console.log(`  ok: ${what}`);
}

console.log(`smoke-testing ${URL}`);

// --- create ---
const a = await Client.connect(URL);
const helloA = await a.expect("hello");
assert(helloA.iceServers.length > 0, "hello carries ice servers");
console.log(`  ice servers: ${JSON.stringify(helloA.iceServers.map((s) => s.urls).flat())}`);

// --- keepalive ---
a.sendText("ping");
await a.expectText("pong");
console.log("  ok: text ping answered with pong");

a.send(clientCreateRoom("alice", 0xaaaa, "GAME-A"));
const { code } = await a.expect("roomCreated");
assert(/^[2-9A-HJKMNP-Z]{6}$/.test(code), `room code well-formed (${code})`);
let roster = await a.expect("roster");
assert(roster.players.length === 1 && roster.yourIdx === 0, "host alone in the roster");

// --- premature start: a room can't start with a single player ---
a.send(clientStart());
const tooFew = await a.expect("error");
assert(tooFew.kind === ErrorKind.NOT_EVERYONE_READY, "start blocked with fewer than 2 players");

// --- join: wrong code, then right code (lowercased: join normalizes) ---
const b = await Client.connect(URL);
await b.expect("hello");
b.send(clientJoinRoom("222222", "bob", 0xbbbb, "GAME-B"));
const notFound = await b.expect("error");
assert(notFound.kind === ErrorKind.ROOM_NOT_FOUND, "unknown room rejected");
b.send(clientJoinRoom(code.toLowerCase(), "bob", 0xbbbb, "GAME-B"));
await b.expect("roomJoined");
roster = await b.expect("roster");
assert(roster.players.length === 2 && roster.yourIdx === 1, "joiner sees both, idx 1");
roster = await a.expect("roster");
assert(roster.players.length === 2 && roster.yourIdx === 0, "host sees both, idx 0");

// --- only the host starts; readying is legacy but still mirrored ---
b.send(clientStart());
const notHost = await b.expect("error");
assert(notHost.kind === ErrorKind.NOT_HOST, "non-host can't start");
b.send(clientSetReady(true));
roster = await a.expect("roster");
assert(roster.players[1].ready, "host still sees the legacy ready flag");
await b.expect("roster");
a.send(clientStart());
const startA = await a.expect("starting");
const startB = await b.expect("starting");
assert(startA.yourIdx === 0 && startB.yourIdx === 1, "start carries per-player indices");
assert(startA.players.length === 2, "start carries the full seating");

// --- the room record dies at start: the code no longer resolves ---
const c = await Client.connect(URL);
await c.expect("hello");
c.send(clientJoinRoom(code, "carol", 1, "X"));
const started = await c.expect("error");
assert(started.kind === ErrorKind.ROOM_NOT_FOUND, "room record deleted at session start");

// --- ...but the signal relay still works, room state and all gone ---
a.send(clientSignal(1, new Uint8Array([1, 2, 3])));
const sigB = await b.expect("signal");
assert(sigB.peer === 0 && [...sigB.payload].join() === "1,2,3", "signal relayed host → joiner");
b.send(clientSignal(0, new Uint8Array([9])));
const sigA = await a.expect("signal");
assert(sigA.peer === 1 && [...sigA.payload].join() === "9", "signal relayed joiner → host");

// --- protocol version mismatch closes the connection ---
c.send(clientCreateRoom("carol", 1, "X", 999));
const mismatch = await c.expect("error");
assert(mismatch.kind === ErrorKind.PROTOCOL_VERSION_MISMATCH, "old protocol rejected");
await c.expectClosed();
console.log("  ok: mismatched client disconnected");

// --- disconnect after start → PeerLeft with a frozen index ---
b.close();
const left = await a.expect("peerLeft");
assert(left.playerIdx === 1, "departure after start reported with a frozen index");

// --- explicit leave closes the socket ---
a.send(clientLeave());
await a.expectClosed();
console.log("  ok: leave closes the connection");

// --- a lobby (unstarted) dies with its last member ---
const d = await Client.connect(URL);
await d.expect("hello");
d.send(clientCreateRoom("dave", 1, "X"));
const lobby = await d.expect("roomCreated");
await d.expect("roster");
d.send(clientLeave());
await d.expectClosed();
const e = await Client.connect(URL);
await e.expect("hello");
e.send(clientJoinRoom(lobby.code, "eve", 1, "X"));
const gone = await e.expect("error");
assert(gone.kind === ErrorKind.ROOM_NOT_FOUND, "lobby deleted once its last member left");
e.close();

// --- the host kicks; nobody else does ---
const f = await Client.connect(URL);
await f.expect("hello");
f.send(clientCreateRoom("frank", 1, "X"));
const kickRoom = await f.expect("roomCreated");
roster = await f.expect("roster");
assert(roster.players[0].seat === 0, "host holds seat 0");
const g = await Client.connect(URL);
await g.expect("hello");
g.send(clientJoinRoom(kickRoom.code, "grace", 1, "X"));
await g.expect("roomJoined");
roster = await g.expect("roster");
const graceSeat = roster.players[1].seat;
assert(graceSeat === 1, "joiner assigned the next seat");
await f.expect("roster");
g.send(clientKickPlayer(0));
const notHostKick = await g.expect("error");
assert(notHostKick.kind === ErrorKind.NOT_HOST, "non-host can't kick");
f.send(clientKickPlayer(0));
const selfKick = await f.expect("error");
assert(selfKick.kind === ErrorKind.MALFORMED, "host can't kick themselves");
f.send(clientKickPlayer(99));
const wildKick = await f.expect("error");
assert(wildKick.kind === ErrorKind.MALFORMED, "unknown seat bounced");
f.send(clientKickPlayer(graceSeat));
const kicked = await g.expect("error");
assert(kicked.kind === ErrorKind.KICKED, "kicked player told why");
await g.expectClosed();
console.log("  ok: kicked player disconnected");
roster = await f.expect("roster");
assert(roster.players.length === 1, "host sees the seat freed");

// --- the room stays joinable after a kick; seats never get reused ---
const h = await Client.connect(URL);
await h.expect("hello");
h.send(clientJoinRoom(kickRoom.code, "heidi", 1, "X"));
await h.expect("roomJoined");
roster = await h.expect("roster");
assert(roster.players.length === 2 && roster.yourIdx === 1, "room joinable after a kick");
const heidiSeat = roster.players[1].seat;
assert(heidiSeat === 2, "the kicked seat isn't reissued");
await f.expect("roster");

// --- a kick racing a departure can't hit the wrong player: Heidi
// slides into Grace's old position, but Grace's seat just bounces ---
f.send(clientKickPlayer(graceSeat));
const staleKick = await f.expect("error");
assert(staleKick.kind === ErrorKind.MALFORMED, "stale kick bounces off the slid-in player");
f.send(clientKickPlayer(heidiSeat));
const kicked2 = await h.expect("error");
assert(kicked2.kind === ErrorKind.KICKED, "seat-addressed kick hits the intended player");
await h.expectClosed();
roster = await f.expect("roster");
assert(roster.players.length === 1, "host alone again");
f.send(clientLeave());
await f.expectClosed();

console.log("all good ✓");
