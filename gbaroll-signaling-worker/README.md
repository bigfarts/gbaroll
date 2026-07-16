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
flows peer-to-peer), so one object is plenty. Room state is kept
exactly as long as it's useful:

- A **lobby** lives in the object's storage (it must survive eviction)
  and dies with its last member; an hourly sweep collects lobbies whose
  close events were lost.
- The moment a room **starts**, its record is deleted and the code is
  free for reuse. Everything the brief post-start signal relay needs —
  the sender's frozen index and the peers' socket ids — is stamped into
  each socket's hibernation attachment, so a running session holds no
  server-side state beyond its open sockets.

Sockets use the hibernation API throughout, so idle lobbies don't keep
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
node scripts/smoke.ts  # end-to-end: create/join/ready/start/relay/leave/keepalive
node scripts/smoke.ts wss://gbaroll-signaling.farts.fyi/   # against production
```
