# gbaroll

A generic GBA **link-cable rollback netplay** client. Any link-capable
game works — there is no per-game code. 2 to 4 players per session.

## How it works

Every GBA on the emulated cable runs locally in one
`mgba_siolink::Link` (mgba cores joined through the lockstep SIO
driver). The link is the rollback unit: the only true inputs are the
joypads, everything on the wire is derived deterministically. Each peer
runs the same link and one `getgud` rollback session, feeding confirmed
local + predicted remote keys per tick and rolling the whole link back
when a prediction misses.

- **Transport**: full WebRTC mesh (one `datachannel-wrapper` /
  libdatachannel connection per peer pair), rendezvoused through the
  bundled signaling server. The data protocol is
  [`rennet`](../tango/rennet) frames — reliable-ordered input streams
  over an unreliable, unordered datachannel, with proactive redundancy.
  Frames also piggyback settled-state digests, so cross-peer desyncs are
  detected on the wire.
- **ROMs**: each player brings their *own* ROM (like carts on a real
  cable — version pairings work). Every peer simulates every side, so
  everyone needs a local **copy** of every player's ROM; the lobby
  checks your library against the roster and won't let you ready up
  until you have them all. Saves are committed with the ready flag and
  distributed at session start.
- **Replays**: every netplay session records a roundless
  `gbaroll-replay` (`.gbrr`) file — boot configuration (per-side ROM
  identity + save) plus the confirmed input stream, nothing else. The
  built-in player supports pause, speeds, per-player view switching, and
  instant scrubbing (background prefetch fills a keyframe snapshot
  store; a rewind ring keeps recent exact frames; seeks chase
  asynchronously from the nearest snapshot).

## Workspace

- `gbaroll` — the client (iced UI, SDL3 audio + gamepad, wgpu
  framebuffer shader).
- `gbaroll-signaling` — the signaling protocol (bincode over
  WebSocket).
- `gbaroll-signaling-server` — the room/rendezvous server
  (`cargo run -p gbaroll-signaling-server -- --listen 0.0.0.0:1984`).
- `gbaroll-replay` — the replay container.

The engine crates (`mgba-siolink`, `getgud`, `rennet`) come from the
tango workspace, expected as a sibling checkout at `../tango`.

## Running

```sh
# the server (somewhere reachable):
cargo run --release -p gbaroll-signaling-server

# the client:
cargo run --release -p gbaroll
```

Point the client at the server in Settings (`ws://host:1984`), drop
`.gba` files into the ROMs directory, pick one on the Play tab, and
host/join a room. Local sessions (all sides on one machine) and replay
playback need no server.
