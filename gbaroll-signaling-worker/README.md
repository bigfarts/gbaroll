# gbaroll-signaling-worker

The gbaroll signaling server: room rendezvous + opaque signal relay for
2–4 player sessions, as a TypeScript Cloudflare Worker. The wire
protocol is protobuf over WebSocket, generated from the shared schema
in `../gbaroll-signaling/proto/signaling.proto` (the same schema the
Rust client builds its types from via prost). Cross-implementation
byte fixtures in `test/codec.test.ts` pin the two codecs together.

Deployed at `wss://gbaroll-signaling.farts.fyi`.

## How it lives

All rooms sit in a single Durable Object: create/join arrive as the
first websocket *message*, so there's no room code to route on at
upgrade time — and signaling is rendezvous-only traffic (game data
flows peer-to-peer), so one object is plenty.

A room is a persistent rendezvous with **dynamic membership**: players
join and leave at any time, and the room **merges** (broadcasts
`Starting`) repeatedly over its life. When a merge fires depends on the
room's peripheral: **wireless** rooms merge on their own whenever 2+
members all report ready with a membership the last merge doesn't
cover (RFU games handle members drifting in and out), while **cable**
rooms gather until position 0 sends `Start` — a cable game enumerates
its chain at its link menu and can't absorb consoles mid-game — and
`Start` fires again later to fold late joiners in. The record lives in
the object's storage (it must survive eviction) and dies with its last
member; an hourly sweep collects rooms whose close events were lost.
Each merge stamps the relay routing — the sender's merge index and the
peers' socket ids — into every member's hibernation attachment, so
relaying a signal never reads the record.

Sockets use the hibernation API throughout, so idle rooms don't keep
the object pinned. Clients may send a text `"ping"` as a keepalive; a
configured auto-response answers `"pong"` without waking the object.

## Deploy

```sh
pnpm install
pnpm deploy    # workers.dev + the gbaroll-signaling.farts.fyi custom domain
```

## Cloudflare TURN

The worker mints short-lived TURN credentials from the
[Cloudflare TURN API](https://developers.cloudflare.com/realtime/turn/)
and hands them out in every `Hello` greeting. It needs a TURN key
(dashboard → Realtime → TURN server) provided as secrets:

```sh
pnpm exec wrangler secret put CLOUDFLARE_TURN_TOKEN_ID
pnpm exec wrangler secret put CLOUDFLARE_TURN_API_TOKEN
```

Credential lifetime defaults to 24h (TURN allocations die when the
credential expires, so it must outlast a whole session); override with
a `TURN_CRED_TTL` var (seconds). Without the secrets — or if minting
fails — clients get a STUN-only list instead. For local dev, put the
secrets in `.dev.vars`.

## Codegen

`src/gen/` is generated from the shared schema and committed:

```sh
pnpm gen    # after touching ../gbaroll-signaling/proto/signaling.proto
```

## Tests

```sh
pnpm test              # codec fixtures, byte-exact against the Rust (prost) encoding
pnpm dev               # then, in another terminal:
node scripts/smoke.ts  # end-to-end: the dynamic room life — merges, re-merges, kicks, leaves
node scripts/smoke.ts wss://gbaroll-signaling.farts.fyi/   # against production
```
