# Handshake — Signaling Server Spec

Oct 5, 2026 · @Seth Miller

## Overview

Handshake is a small self-hosted Rust server that matches players into rooms and relays WebRTC signaling, then gets out of the way. Game traffic flows peer-to-peer over WebRTC data channels, host-authoritative in a star topology. The server never sees game state.

Target games are Rapier + three.js browser games played on iPhones and iPads, hosted statically on GitHub Pages. Players may share a LAN or be on entirely different networks, including cellular.

In scope:

- Room registry: create, join by code or QR link, public listing, host lock, lifecycle
- Signaling relay: opaque SDP/ICE blobs routed between host and peers
- Per-app namespacing and lightweight auth (origin allowlist + short-lived session tokens)
- Short-lived TURN credential minting for a companion coturn server
- A shared plain-JavaScript client library used by every game

Out of scope:

- Relaying game traffic (that is TURN's job when direct connections fail)
- Persistence, accounts, matchmaking by skill, horizontal scaling
- Offline / no-internet play

## Architecture and deployment

&#91;embedded content: deployment · GitHub Pages, players, one Docker host\]

The game is static on GitHub Pages; one self-hosted Docker host runs Caddy, the Rust server and coturn. Only signaling and TURN credentials touch the Rust server.

- **Caddy** terminates TLS for `https://signal.<domain>` with automatic certificates and proxies `/session`, `/turn` and `/ws` to the Rust container on port 8080. `/metrics` is not proxied.
- **handshake** is a single Rust binary (axum + tokio) in a distroless image. It reads `config.toml` and secrets from environment variables.
- **coturn** runs with `network_mode: host`, because large UDP port ranges map badly through Docker. Set `external-ip` if the box is behind NAT, narrow the relay range (for example 49160–49200), and reuse Caddy's certificate for `turns:` on 443.

```yaml
services:
  caddy:
    image: caddy:2
    ports: ["80:80", "443:443"]
    volumes: ["./Caddyfile:/etc/caddy/Caddyfile", "caddy_data:/data"]
  signal:
    build: ./handshake
    environment:
      SESSION_SECRET: ${SESSION_SECRET}
      TURN_SECRET: ${TURN_SECRET}
    volumes: ["./config.toml:/etc/handshake/config.toml:ro"]
  coturn:
    image: coturn/coturn
    network_mode: host
```

If coturn needs port 443 for `turns:` on the same IP as Caddy, give coturn a second IP or hostname, or use 5349 and accept that some firewalls block it.

## Apps and auth

Auth keeps casual third parties off the server; it is not strong identity. A static GitHub Pages app cannot hold a secret, so anything embedded in its JavaScript is public.

### App registry

Apps are declared in `config.toml`, mounted into the container. Each app has an ID, allowed origins and limits:

```toml
[apps.pig-pens]
origins = ["https://seth.github.io"]
max_players = 8
max_rooms = 50
public_rooms = true
turn = true

[apps.skerry-island]
origins = ["https://seth.github.io"]
max_players = 4
max_rooms = 20
public_rooms = false
turn = true

[apps.tractor-pickup]
origins = ["https://four43.com"]
max_players = 4
max_rooms = 20
public_rooms = true   # joinable with the code alone
list = "none"         # but never listed: the code is the only way in
turn = true
```

`list` controls what `list` returns for the app: `"all"` (default) returns its public rooms as described under Public listing; `"none"` returns an empty list, so a public room's code stays secret. Phase 2 adds `"knock"` (see Knock to join).

### Layers

1. **Origin allowlist.** `POST /session` and the WebSocket upgrade both require an `Origin` header matching the app's list. Browsers cannot forge `Origin`; scripts can.
2. **Session token.** `/session` returns an HMAC-SHA256-signed token carrying app ID, protocol version and expiry (default 15 minutes). The WebSocket `hello` and `/turn` both require it.
3. **Rate limits.** Per-IP limits on session minting and join attempts blunt scripted abuse and code brute-forcing. A per-app limit on failed joins and peeks (`not_found`, `bad_key`) across all IPs stops a guesser with many addresses; when it is hit, joins and peeks for that app return `rate_limited` until the minute rolls over. The client IP is the socket address, or the last untrusted `X-Forwarded-For` entry when the socket comes from `trusted_proxies` (loopback and private networks by default), so per-IP limits hold with or without a proxy in front and cannot be dodged by sending the header directly.
4. **TURN quota.** TURN usernames encode expiry and app ID, so coturn logs attribute relay usage per app. Cloudflare Turnstile before `/session` is the upgrade path if abuse appears.

### Path-based apps share an origin

Most apps live at different paths on `https://seth.github.io`. `Origin` never carries a path, and cross-origin `Referer` is trimmed to the origin by default. The app ID is therefore a routing label, not a security boundary: any page on that origin can claim any app ID. That is acceptable because every app there is Seth's. Do not add per-app secrets or permissions that assume otherwise; give an app its own domain if it ever needs isolation.

### Secrets

- `SESSION_SECRET` (required) signs session tokens. `SESSION_SECRET_PREV` is also accepted for verification, so keys rotate without logging everyone out.
- `TURN_SECRET` must match coturn's `static-auth-secret`.
- `allow_localhost = true` in config accepts `http://localhost:*` and `http://127.0.0.1:*` for every app. Use it for development only.

## Rooms

A room is one host plus its joined peers, namespaced by app and pinned to a game protocol version. Rooms live in memory only; a server restart drops them all.

### Creating and joining

- **Create.** The host sends `create` with `public`, an optional `name`, `max_players` and `meta`. The server returns a 5-character join code, a private key, the host's peer ID and a resume token.
- **Codes.** Codes use an unambiguous alphabet (no 0/O, 1/I/L) and are unique within an app.
- **Private rooms** require the key. The share URL is `<game url>?r=<code>&k=<key>`, shown as a QR code for in-person play and passed to `navigator.share()` for remote friends. The QR is generated client-side.
- **Public rooms** can be joined with the code alone and appear in the app's lobby listing, unless the app sets `list = "none"`.
- **Peek.** `peek` looks at a room by code without joining it, so a game can show "Join this room?" first. It returns the room's name, player count, limit and whether it is locked or full. A public room needs only the code; a private room also needs its key, exactly as `join` does (missing or wrong key: `bad_key`). A peek counts as a join attempt for rate limits and metrics, and its `not_found` and `bad_key` count as failed joins, so it cannot guess codes or keys faster than `join`.
- **Version pinning.** A join from a different protocol version is rejected with `version_mismatch`, so cached old builds cannot join newer netcode.
- **Topology.** Star only: the server forwards signaling between the host and each peer, never peer to peer.

### Public listing

`list` returns the app's public rooms for the caller's version that are unlocked and not full, capped at 50. Each entry has `nearby: true` when the host's public IP matches the caller's (IPv4 exact, IPv6 by /64 prefix). Rooms whose host is away are left out. Nearby rooms sort first, then newest. Room names are capped at 32 characters.

### Host controls

- `lock` stops new joins, for example once a match starts.
- `meta` replaces the room's metadata blob (JSON, max 1 KB): map, mode, in-progress, anything the lobby shows. The server never interprets it.
- `kick` removes a peer. Kicked peers can rejoin unless the host has locked the room.

### Lifecycle and resume

Every member, host included, gets a resume token. iPhones drop sockets constantly, so a disconnect starts a grace period instead of removing anyone.

| Event | Effect |
| --- | --- |
| Peer socket drops | Host gets `peer_away`; slot held for 30 s |
| Peer resumes in time | Host gets `peer_back` |
| Peer grace expires or peer sends `leave` | Peer removed; members get `peer_left` |
| Host socket drops | Members get `host_away`; room held for 30 s |
| Host resumes in time | Members get `host_back` |
| Host grace expires or host sends `leave` | Room closed; members get `room_closed` |
| Room older than 12 h | Room closed |
| Host alone for 30 min | Room closed |

Host migration is the game's job. The server only announces `host_away`; the game decides whether a peer takes over and creates a new room.

## Signaling protocol

Three HTTP endpoints plus one WebSocket carrying JSON messages tagged by a `t` field. Signaling protocol version is 1, separate from each game's protocol version.

### HTTP

| Endpoint | Auth | Request | Response |
| --- | --- | --- | --- |
| `POST /session` | Origin | `{app, version}` | `{token, expires_in, turn}` |
| `POST /turn` | `Authorization: Bearer <token>` | empty | `{ice_servers: [{urls, username, credential}], ttl}` |
| `GET /ws` | Origin, then `hello` | WebSocket upgrade | message stream |
| `GET /healthz` | none |  | `ok` |
| `GET /metrics` | none (keep internal) |  | Prometheus text |

### Connection flow

1. Client calls `/session` with its app ID and game protocol version.
2. Client calls `/turn` and builds its `RTCPeerConnection` config.
3. Client opens `wss://…/ws` and sends `hello` within 10 s. The token goes in this message, not the URL, so it stays out of proxy logs.
4. Server replies `welcome`; the client can now `create`, `join`, `resume` or `list`.
5. On `peer_joined`, the host creates an offer for that peer and exchanges `signal` messages until the data channels open.
6. The socket stays open for the whole session, for ICE restarts, host-away notices and resume.

### Client to server

| t | Fields | Who |
| --- | --- | --- |
| `hello` | `v`, `token` | anyone, first message |
| `create` | `public`, `name?` (room), `player?`, `max_players?`, `meta?` | not in a room |
| `join` | `code`, `key?`, `player?` | not in a room |
| `resume` | `token` | not in a room |
| `peek` | `code`, `key?` (private rooms) | not in a room |
| `list` |  | anyone |
| `signal` | `to`, `data` | host ↔ peer only |
| `lock` | `locked` | host |
| `meta` | `meta` | host |
| `kick` | `peer` | host |
| `leave` |  | member |

### Server to client

| t | Fields | Sent when |
| --- | --- | --- |
| `welcome` | `v` | after a valid `hello` |
| `joined` | `room` (see below), `resumed` | after `create`, `join` or `resume` |
| `rooms` | `rooms: [{code, name, players, max_players, meta, nearby}]` | reply to `list` |
| `room_info` | `code`, `name`, `players`, `max_players`, `locked`, `full` | reply to `peek` |
| `peer_joined` | `peer`, `name`, `nearby` | a peer joined |
| `peer_away` / `peer_back` | `peer` | a peer's socket dropped / resumed |
| `peer_left` | `peer`, `reason` | a peer was removed |
| `host_away` | `grace_secs` | host socket dropped |
| `host_back` |  | host resumed |
| `signal` | `from`, `data` | relayed SDP/ICE |
| `room_meta` | `meta`, `locked` | host changed meta or lock |
| `room_closed` | `reason` | room ended |
| `kicked` |  | you were kicked |
| `error` | `code`, `message` | a request failed |

The `room` object in `joined` holds `code`, `name`, `public`, `max_players`, `locked`, `meta`, `host` (peer ID), `you` (your peer ID), `is_host`, `peers: [{id, name, away, nearby}]`, `resume`, and `key` (host only).

Error codes: `bad_message`, `bad_token`, `origin`, `rate_limited`, `already_in_room`, `not_in_room`, `not_host`, `not_found`, `bad_key`, `version_mismatch`, `locked`, `full`, `too_many_rooms`, `public_disabled`, `peer_unavailable`, `meta_too_large`.

`nearby` on a peer is true when that peer's public IP matches the host's (same rule as Public listing); the host's own entry is always `true`. Games use it to force relayed connections between players who are not nearby, so neither learns the other's IP.

`replaced` is sent to an old socket as `{t: "error", code: "replaced"}` when the same session resumes on a new one; the old socket stays open but is no longer in the room. When the host sends `leave`, it also receives `room_closed`. A peer that resumes while its old socket is still open causes `peer_back` without an earlier `peer_away`. HTTP endpoints return `{error}` with `rate_limited`, `not_found`, `origin`, `bad_token`, `turn_disabled` or `turn_unconfigured`.

## TURN and connectivity

TURN is essential, not a fallback: players on cellular carrier-grade NAT or strict corporate Wi-Fi often cannot connect directly. Expect roughly 10–20% of internet sessions to relay.

- **Credentials** follow coturn's `use-auth-secret` scheme. Username is `<expiry unix>:<app id>`; credential is base64 HMAC-SHA1 of the username keyed by `TURN_SECRET`. Default TTL is 1 hour.
- **URLs** offered, all from config: `stun:` and `turn:` on 3478 (UDP and TCP), plus `turns:` on 443 for networks that block everything but HTTPS.
- **Per-app quotas** come from coturn (`user-quota`, `total-quota`, `max-bps`), attributed by the app ID in the username.
- **Client isolation** on guest and office Wi-Fi blocks device-to-device traffic even on one LAN; TURN covers it.
- **Network changes.** When an iPhone hops between Wi-Fi and cellular, the client calls `restartIce()` and re-signals over the still-open WebSocket.
- **Diagnostics.** The client reads `getStats()` and reports whether the selected candidate pair is direct or relayed, so games can show a connection badge.

## Game networking guidance

Games built on Handshake should be host-authoritative with snapshot interpolation. These are recommendations for the games, not server features.

### Model

- The host runs the authoritative Rapier world at a fixed 60 Hz and broadcasts snapshots at 20–30 Hz.
- Clients send inputs, interpolate remote bodies behind real time, and predict their own player with reconciliation against host corrections.
- The interpolation buffer adapts to measured jitter: about 100 ms on a LAN, more over the internet (30–150 ms latency is typical).
- Lockstep with `@dimforge/rapier3d-deterministic` is possible but less forgiving of Wi-Fi jitter. Start with snapshots.

### Replicated objects: keyframes and diffs

Shared state should be a registry of replicated objects, not one hand-written message per feature (this is the Unreal "replicated actor" / Unity `NetworkObject` / Quake snapshot pattern):

- Each **kind** of object declares its fields (with quantization) and one **authority**: the host for world objects, the owning player for that player's avatar or vehicle. Only the authority changes an object.
- Each object has an id and an **ownership number** that increases on every change of owner; receivers ignore data older than what they have.
- The authority sends a **keyframe** (full state) on the reliable channel every couple of seconds and to every new joiner, and **diffs** (only changed objects, each with all its fields) on the unreliable channel in between. Diffs carry absolute values, so a lost diff needs no acknowledgement or resend: the next diff or keyframe heals it.
- Requests (claim an object, hit something, deliver) are **events** on the reliable channel. Their result always comes back as replicated state, never as a separate answer, so a lost answer cannot leave peers disagreeing.
- Each peer repairs its view at every keyframe (anything it is not the authority for takes the keyframe's value). Agreement after any loss is bounded by the keyframe interval.

### Channels and encoding

- Two data channels per peer: `state` (unordered, `maxRetransmits: 0`) for snapshots and inputs, `events` (reliable, ordered) for joins, scoring and chat.
- Binary ArrayBuffers, never JSON. Quantize positions and quaternions; send only awake or changed bodies.
- `world.takeSnapshot()` is for late joiners only; it is too large to send every tick.
- Budget: about 20 quantized bodies at 30 Hz is roughly 100 kbps per peer, so 7 peers is about 0.7 Mbps of host upload. Relayed sessions push the same traffic through the TURN server.

### iOS realities

- Backgrounding or locking the screen suspends JS and drops connections. Request a Screen Wake Lock, warn players, and lean on resume.
- Low Power Mode caps rendering at 30 fps and phones throttle thermally. The host does the most work, so prefer an iPad as host.
- Plan for jitter spikes, not average latency.

## JS client library

One plain-JavaScript ES module, `handshake.js`, shared by every game, no build step and no dependencies beyond the browser. Types are documented with JSDoc so editors still autocomplete. Games import it directly from GitHub Pages or vendor a copy.

```js
import { Handshake } from "./handshake.js";

const hs = new Handshake({
  server: "https://signal.example.com",
  app: "pig-pens",
  version: 3,
  name: "Seth",                 // your display name
});

const room = await hs.createRoom({ public: false, maxPlayers: 6, name: "Seth's farm" });
room.shareUrl;          // this page + ?r=K7MX2&k=... for a QR code or navigator.share()

room.on("peer", (peer) => {
  peer.name;                                 // the player's display name
  peer.send(snapshotBuffer);                 // state channel, unreliable, binary only
  peer.send({ type: "score" }, { reliable: true }); // events channel
  peer.on("message", (data, { reliable }) => {});
  peer.connectionType;  // "direct" | "relayed"
});
room.on("peerLeft", (id, reason) => {});
room.on("hostAway", (graceSecs) => {});
room.on("closed", (reason) => {});

// Host controls resolve once applied; they reject with not_host, a server error, network, timeout or closed
await room.lock(true);
await room.setMeta({ map: "farm" });
await room.kick(peerId);

// Look before joining
const info = await hs.peek(code, key);       // key only for a private room; {code, name, players, maxPlayers, locked, full}

// Joining
const joined = await hs.joinFromUrl() ?? await hs.joinRoom(code, key); // joinFromUrl: null without ?r=
const rooms = await hs.listRooms();          // public, nearby first; [{code, name, players, maxPlayers, meta, nearby}]
```

The library owns everything every game would otherwise repeat:

- session token fetch and refresh, TURN credential fetch
- WebSocket connect, `hello`, heartbeat and automatic `resume` after drops
- `RTCPeerConnection` setup, both data channels, offer/answer and ICE exchange
- ICE restart on network change, `visibilitychange` handling, Wake Lock request
- connection-type detection through `getStats()`
- host controls (`lock`, `setMeta`, `kick`) that return promises settling on the server's answer
- relay-only connections (`iceTransportPolicy: "relay"`) to peers that are not `nearby`, when the game passes `relayUnlessNearby: true`

Reliable messages that are plain objects are JSON-encoded; ArrayBuffers pass through untouched on either channel.

## Limits, operations and testing

### Defaults

| Setting | Default | Config key |
| --- | --- | --- |
| WebSocket message size | 16 KB | fixed |
| Hello deadline | 10 s | fixed |
| Idle socket timeout (no frames) | 60 s | fixed |
| Outbound queue per socket | 64 messages, then disconnect | fixed |
| Session token TTL | 900 s | `limits.session_ttl_secs` |
| Resume grace | 30 s | `limits.grace_secs` |
| Max room age | 12 h | `limits.room_max_age_secs` |
| Host-alone timeout | 30 min | `limits.idle_room_secs` |
| Session mints per IP | 30 / min | `limits.sessions_per_min` |
| Join attempts per IP | 20 / min | `limits.joins_per_min` |
| Failed joins and peeks per app (all IPs) | 200 / min | `limits.app_failed_joins_per_min` |
| Expiry sweep (grace, idle, age) | every 5 s | fixed |
| Room meta size | 1 KB | fixed |

### Operations

- Logs: `tracing` JSON output, tagged with app and room code.
- `/healthz` for the Docker healthcheck; `/metrics` with sessions issued, TURN credentials issued, joins rejected, live sockets and rooms per app. Keep `/metrics` off the public proxy.
- Image: multi-stage build into distroless, about 15–20 MB, runs as non-root.
- Single instance with in-memory state. Redis and multiple instances only if ever needed.

### Testing

- Rust: tokio integration tests driving a host and joiners over real sockets, covering grace, resume and lock.
- JS: Playwright with two browser contexts completing a full WebRTC connection through a local server.

### Open questions

- Should kicked peers be barred from rejoining? Currently only locking the room prevents it.
- Does the lobby need room-name filtering beyond the 32-character cap?
- ~~Distribution of `handshake.js`~~: resolved, each game vendors a copy (the source lives in this repo at `client/handshake.js`).

## Phase 2: Knock to join

For apps with `list = "knock"`, `list` returns nearby rooms only, without their codes: each entry has an opaque `ref` instead of `code`. A caller joins such a room by knocking; the host decides.

| t | Fields | Direction |
| --- | --- | --- |
| `knock` | `ref` | client to server, not in a room; counts as a join attempt |
| `knock` | `knock` (id) | server to host |
| `admit` / `deny` | `knock` | host to server |
| `knock_wait` | `knock` | server to the knocker |
| `joined` / `denied` | as `join` / none | server to the knocker |

A knock expires after 60 s. Joining with the code never needs a knock. The game shows the knock to the host behind a 2-second hold, so a child cannot admit by accident.
