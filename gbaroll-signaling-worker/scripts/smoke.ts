// End-to-end exercise of a running worker over the real wire protocol:
// the dynamic-membership room life — create/join/ready/merge, joins and
// re-merges mid-session, the void-and-retry dance, kicks, leaves — plus
// the error paths and the text-ping keepalive.
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

a.send(clientCreateRoom("alice", 0xaaaa, "GAME-A", true));
const { code } = await a.expect("roomCreated");
assert(/^[2-9A-HJKMNP-Z]{6}$/.test(code), `room code well-formed (${code})`);
let roster = await a.expect("roster");
assert(roster.players.length === 1 && roster.yourIdx === 0, "creator alone in the roster");
assert(roster.wireless, "roster carries the room's peripheral");
assert(!roster.players[0].ready, "position 0 readies like everyone else now");

// --- a lone ready member doesn't merge ---
a.send(clientSetReady(true));
roster = await a.expect("roster");
assert(roster.players[0].ready, "position 0's ready bit is real");

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
assert(!roster.players[0].ready, "occupancy change reset every ready bit");
roster = await a.expect("roster");
assert(roster.players.length === 2 && roster.yourIdx === 0, "creator sees both, idx 0");

// --- the room merges on convergence: 2+ members, everyone ready ---
a.send(clientSetReady(true));
await a.expect("roster");
await b.expect("roster");
b.send(clientSetReady(true));
await a.expect("roster");
await b.expect("roster");
const startA = await a.expect("starting");
const startB = await b.expect("starting");
assert(startA.yourIdx === 0 && startB.yourIdx === 1, "merge carries per-player indices");
assert(startA.players.length === 2, "merge carries the full seating");

// --- the signal relay rides the merge routing ---
a.send(clientSignal(1, new Uint8Array([1, 2, 3])));
const sigB = await b.expect("signal");
assert(sigB.peer === 0 && [...sigB.payload].join() === "1,2,3", "signal relayed 0 → 1");
b.send(clientSignal(0, new Uint8Array([9])));
const sigA = await a.expect("signal");
assert(sigA.peer === 1 && [...sigA.payload].join() === "9", "signal relayed 1 → 0");

// --- the room record lives on: the code stays joinable mid-session ---
const c = await Client.connect(URL);
await c.expect("hello");
c.send(clientJoinRoom(code, "carol", 0xcccc, "GAME-C"));
await c.expect("roomJoined");
roster = await c.expect("roster");
assert(roster.players.length === 3 && roster.yourIdx === 2, "mid-session join lands in the roster");
await a.expect("roster");
await b.expect("roster");

// --- a non-member's withdrawn ready leaves the standing merge alone ---
c.send(clientSetReady(false));
await a.expect("roster");
await b.expect("roster");
await c.expect("roster");

// --- an unmerged member's departure sends no PeerLeft ---
c.send(clientLeave());
await c.expectClosed();
roster = await a.expect("roster");
assert(roster.players.length === 2, "unmerged member's departure shrinks the roster only");
await b.expect("roster");

// --- unchanged membership doesn't re-merge... ---
a.send(clientSetReady(true));
await a.expect("roster");
await b.expect("roster");
b.send(clientSetReady(true));
await a.expect("roster");
await b.expect("roster");
a.send(clientSignal(1, new Uint8Array([7])));
const noRemerge = await b.expect("signal");
assert(noRemerge.payload[0] === 7, "all-ready with unchanged membership stays merged (no Starting)");

// --- ...until a merged member voids it: the withdraw-and-retry dance ---
b.send(clientSetReady(false));
await a.expect("roster");
await b.expect("roster");
b.send(clientSetReady(true));
await a.expect("roster");
await b.expect("roster");
const retryA = await a.expect("starting");
await b.expect("starting");
assert(retryA.players.length === 2, "a voided merge re-fires for unchanged membership");

// --- a fresh joiner folds in through a re-merge ---
const d = await Client.connect(URL);
await d.expect("hello");
d.send(clientJoinRoom(code, "dave", 0xdddd, "GAME-D"));
await d.expect("roomJoined");
roster = await d.expect("roster");
const daveSeat = roster.players[2].seat;
assert(daveSeat === 3, "seats never get reused (carol's is retired)");
await a.expect("roster");
await b.expect("roster");
a.send(clientSetReady(true));
await a.expect("roster");
await b.expect("roster");
await d.expect("roster");
b.send(clientSetReady(true));
await a.expect("roster");
await b.expect("roster");
await d.expect("roster");
d.send(clientSetReady(true));
await a.expect("roster");
await b.expect("roster");
await d.expect("roster");
const remergeA = await a.expect("starting");
await b.expect("starting");
const remergeD = await d.expect("starting");
assert(remergeA.players.length === 3 && remergeD.yourIdx === 2, "the newcomer merges in at idx 2");

// --- the relay re-stamped: position 0 reaches the newcomer ---
a.send(clientSignal(2, new Uint8Array([42])));
const sigD = await d.expect("signal");
assert(sigD.peer === 0 && sigD.payload[0] === 42, "relay routing follows the newest merge");

// --- only position 0 kicks; kicks address seats ---
b.send(clientKickPlayer(daveSeat));
const notHost = await b.expect("error");
assert(notHost.kind === ErrorKind.NOT_HOST, "only position 0 can kick");
a.send(clientKickPlayer(0));
const selfKick = await a.expect("error");
assert(selfKick.kind === ErrorKind.MALFORMED, "position 0 can't kick themselves");
a.send(clientKickPlayer(99));
const wildKick = await a.expect("error");
assert(wildKick.kind === ErrorKind.MALFORMED, "unknown seat bounced");
a.send(clientKickPlayer(daveSeat));
const kicked = await d.expect("error");
assert(kicked.kind === ErrorKind.KICKED, "kicked player told why");
await d.expectClosed();
console.log("  ok: kicked player disconnected");
let left = await a.expect("peerLeft");
assert(left.playerIdx === 2, "a merged member's kick reaches the peers as PeerLeft");
await b.expect("peerLeft");
roster = await a.expect("roster");
assert(roster.players.length === 2, "position 0 sees the seat freed");
await b.expect("roster");

// --- protocol version mismatch closes the connection ---
const e = await Client.connect(URL);
await e.expect("hello");
e.send(clientCreateRoom("eve", 1, "X", false, 999));
const mismatch = await e.expect("error");
assert(mismatch.kind === ErrorKind.PROTOCOL_VERSION_MISMATCH, "old protocol rejected");
await e.expectClosed();
console.log("  ok: mismatched client disconnected");

// --- a merged member's disconnect → PeerLeft + a shrunken roster ---
b.close();
left = await a.expect("peerLeft");
assert(left.playerIdx === 1, "merged member's disconnect reported to the peers");
roster = await a.expect("roster");
assert(roster.players.length === 1, "the roster shrinks with the disconnect");

// --- the room dies with its last member ---
a.send(clientLeave());
await a.expectClosed();
console.log("  ok: leave closes the connection");
const f = await Client.connect(URL);
await f.expect("hello");
f.send(clientJoinRoom(code, "fern", 1, "X"));
const gone = await f.expect("error");
assert(gone.kind === ErrorKind.ROOM_NOT_FOUND, "room deleted once its last member left");
f.close();

// === cable rooms gather until position 0 links them up ===

/** Drain one roster broadcast from every connected member. */
async function allRosters(clients: Client[]): Promise<void> {
  for (const c of clients) await c.expect("roster");
}

const p0 = await Client.connect(URL);
await p0.expect("hello");
p0.send(clientCreateRoom("gina", 1, "X", false)); // cable
const cable = await p0.expect("roomCreated");
roster = await p0.expect("roster");
assert(!roster.wireless, "cable room advertised as such");

const p1 = await Client.connect(URL);
await p1.expect("hello");
p1.send(clientJoinRoom(cable.code, "hugh", 1, "X"));
await p1.expect("roomJoined");
await allRosters([p1, p0]);

// Everyone ready — and the room just sits there gathering.
p0.send(clientSetReady(true));
await allRosters([p0, p1]);
p1.send(clientSetReady(true));
await allRosters([p0, p1]);
p1.send(clientStart());
const notHostStart = await p1.expect("error");
assert(notHostStart.kind === ErrorKind.NOT_HOST, "only position 0 links a cable room up");

// A third player joins the gathered, all-ready room: still no merge —
// this is the 4-player fix, nothing fires until position 0 says so.
const p2 = await Client.connect(URL);
await p2.expect("hello");
p2.send(clientJoinRoom(cable.code, "iris", 1, "X"));
await p2.expect("roomJoined");
await allRosters([p2, p0, p1]);
p0.send(clientStart());
const notReady = await p0.expect("error");
assert(notReady.kind === ErrorKind.NOT_EVERYONE_READY, "cable start needs everyone ready");
p0.send(clientSetReady(true));
await allRosters([p0, p1, p2]);
p1.send(clientSetReady(true));
await allRosters([p0, p1, p2]);
p2.send(clientSetReady(true));
await allRosters([p0, p1, p2]);

// The explicit start merges everyone gathered so far.
p0.send(clientStart());
const cableStart0 = await p0.expect("starting");
await p1.expect("starting");
const cableStart2 = await p2.expect("starting");
assert(
  cableStart0.players.length === 3 && cableStart2.yourIdx === 2,
  "cable room gathered three before linking once",
);
p0.send(clientSignal(2, new Uint8Array([5])));
const cableSig = await p2.expect("signal");
assert(cableSig.peer === 0 && cableSig.payload[0] === 5, "cable relay rides the merge routing");

// A late fourth joiner sits in the roster (no auto-merge) until
// position 0 re-links — 4-player, assembled across two starts.
const p3 = await Client.connect(URL);
await p3.expect("hello");
p3.send(clientJoinRoom(cable.code, "jules", 1, "X"));
await p3.expect("roomJoined");
await allRosters([p3, p0, p1, p2]);
for (const c of [p0, p1, p2, p3]) {
  c.send(clientSetReady(true));
  await allRosters([p0, p1, p2, p3]);
}
p0.send(clientStart());
const relink0 = await p0.expect("starting");
const relink3 = await p3.expect("starting");
await p1.expect("starting");
await p2.expect("starting");
assert(
  relink0.players.length === 4 && relink3.yourIdx === 3,
  "re-link folds the late joiner in: 4 players linked",
);

// --- a cable chain holds exactly four ---
const p4 = await Client.connect(URL);
await p4.expect("hello");
p4.send(clientJoinRoom(cable.code, "kate", 1, "X"));
const chainFull = await p4.expect("error");
assert(chainFull.kind === ErrorKind.ROOM_FULL, "a fifth GBA doesn't fit a cable chain");
p4.close();
for (const c of [p0, p1, p2, p3]) c.close();

// === a wireless room seats one full RFU group: five players ===
const w0 = await Client.connect(URL);
await w0.expect("hello");
w0.send(clientCreateRoom("host", 1, "X", true));
const group = await w0.expect("roomCreated");
await w0.expect("roster");
const flock: Client[] = [w0];
for (let n = 1; n < 5; n++) {
  const c = await Client.connect(URL);
  await c.expect("hello");
  c.send(clientJoinRoom(group.code, `p${n}`, 1, "X"));
  await c.expect("roomJoined");
  flock.push(c);
  for (const member of flock) {
    const r = await member.expect("roster");
    if (n === 4 && member === c) {
      assert(r.players.length === 5, "the fifth player seats (host + 4 clients)");
    }
  }
}
const w5 = await Client.connect(URL);
await w5.expect("hello");
w5.send(clientJoinRoom(group.code, "p5", 1, "X"));
const groupFull = await w5.expect("error");
assert(groupFull.kind === ErrorKind.ROOM_FULL, "a sixth doesn't fit the group");
w5.close();
for (const c of flock) c.close();

// === wireless rooms refuse the explicit start ===
const w = await Client.connect(URL);
await w.expect("hello");
w.send(clientCreateRoom("wren", 1, "X", true));
await w.expect("roomCreated");
await w.expect("roster");
w.send(clientStart());
const wStart = await w.expect("error");
assert(wStart.kind === ErrorKind.MALFORMED, "wireless rooms merge on their own — start bounced");
w.close();

console.log("all good ✓");
