---
title: Connectivity
description: TURN relays, nearby players, ICE restarts, connection badges and what iPhones and iPads do to your connections.
---

Handshake only sets up connections. Game data goes straight between the host and each guest over WebRTC, or through a TURN relay when a direct path is not possible. This page covers how that works and what you need to configure.

## TURN is not optional

Players on cellular networks (carrier-grade NAT), strict corporate or school Wi-Fi, or guest Wi-Fi with client isolation often cannot connect to each other directly, even when they sit in the same room. A TURN server relays their traffic instead. Expect roughly 10 to 20% of internet sessions to need it. Without TURN those players simply fail to connect.

Handshake runs the TURN relay itself (see [Self-hosting](/guides/self-hosting/#turn)): one container, no coturn. It hands out short-lived TURN credentials over `/turn` and checks them when browsers use its relay.

## Turning it on

Three things must line up:

1. A `[turn]` section in the server config with the URLs to offer, `relay_ports` and `external_ip`.
2. `turn = true` on the app.
3. The relay ports and 3478 (UDP and TCP) reachable from the internet.

```toml
[turn]
urls = [
  "stun:turn.example.com:3478",
  "turn:turn.example.com:3478?transport=udp",
  "turn:turn.example.com:3478?transport=tcp",
  "turns:turn.example.com:443?transport=tcp",
]
relay_ports = "49160-49200"
external_ip = "203.0.113.10"

[apps.my-game]
origins = ["https://you.github.io"]
turn = true
```

Offer `turn:` over both UDP and TCP on 3478, plus `turns:` (TLS) on 443 for networks that block everything but HTTPS. Handshake does not do TLS itself: your reverse proxy terminates it and forwards plain TURN (see [TURN over TLS on 443 with Caddy](/guides/self-hosting/#turn-over-tls-on-443-with-caddy)). See the [config reference](/reference/config/) for every key.

`POST /session` tells the client whether TURN is available (`"turn": true` when the app has `turn = true` and `[turn]` is set). Without `relay_ports`, Handshake only mints credentials for an external coturn and needs `TURN_SECRET`; if that is missing, `/session` reports `"turn": false`, `/turn` answers `503 turn_unconfigured`, and the server logs a warning at startup.

## Credentials

Credentials follow coturn's `use-auth-secret` (TURN REST API) scheme, so the same ones work with the built-in relay and with coturn:

- **Username:** `<expiry unix time>:<app id>`, for example `1767225600:my-game`.
- **Credential:** base64 of HMAC-SHA1 over the username, keyed with the TURN secret (`TURN_SECRET`, or one Handshake makes up at startup).
- **Lifetime:** `ttl_secs`, default 1 hour.

The relay accepts a username only while it has not expired and only for an app with `turn = true`. Because the app ID is in the username, quotas and logs can be told apart per app.

The JS client fetches credentials from `POST /turn` before creating or joining a room, renews them about a minute before they expire, and pushes the new ones into open peer connections. A failed fetch is tried once more. If it fails again, the client logs a warning and carries on without TURN, except with `relayUnlessNearby` (below): then `createRoom` and `joinRoom` reject with `no_turn`, since players who are not nearby could never connect. See the [HTTP API](/reference/http/).

## Nearby players

The server marks a player **nearby** when their public IP matches the host's: the exact address for IPv4, the same /64 prefix for IPv6. That usually means "on the same home network". The host's own entry is always nearby. You see it as `peer.nearby` and in each entry of `room.members`.

:::caution
Behind a reverse proxy, the server reads the real client address from `X-Forwarded-For`, but only when the proxy's address is in `trusted_proxies` (loopback and private networks by default). If your proxy connects from a public address, add it there. Otherwise every player looks like the proxy and everyone is nearby. See [Self-hosting](/guides/self-hosting/#server-config).
:::

### relayUnlessNearby

```js
const hs = new Handshake({ server, app, version: 1, relayUnlessNearby: true });
```

With this option the client connects to any player who is **not** nearby only through TURN (`iceTransportPolicy: "relay"`). Neither side then sees the other's IP address, which matters when strangers can join from a public room. Nearby players still connect directly.

Two consequences:

- Every non-nearby connection is relayed, so it costs TURN bandwidth and needs TURN to work. Without TURN credentials (the app has `turn = false`, or `/turn` failed twice), `createRoom` and `joinRoom` reject with `HandshakeError` code `no_turn` instead of opening a room nobody far away can connect to. Show the player a message and let them try again.
- "Same public IP" is a network fact, not an identity check. Players behind the same carrier NAT can share a public IP and count as nearby.

## Network changes and ICE restart

When a phone switches between Wi-Fi and cellular, its peer connections break. The client restarts ICE on its own:

- when a peer connection's state becomes `failed`, and
- for every peer connection when the browser fires `online`.

The host makes a new offer with an ICE restart; a guest asks the host to. The new offer and candidates go over the signaling socket, which stays open for the whole session for exactly this reason. If the socket is down too, the messages wait until the client has resumed (see [Rooms](/guides/rooms/#lifecycle-and-resume)).

When an ICE restart is too late and a data channel closes (an iPad in the background for half a minute, say), the peer connection is gone: its `close` event fires and it leaves `room.peers`. A channel that closes before both channels opened counts too. The client then builds a new connection by itself:

- The host makes a new one to that guest at once, or when the guest comes back (`peerBack`) if its socket is down. A guest whose connections keep failing waits longer between tries (2, 5, then 15 seconds), until one opens.
- A guest asks the host for a new one (`{restart: true, rebuild: true}` in a signal), queued until its socket has resumed, and asks again when the host comes back (`hostBack`) if it still has none.

The first offer of every connection is marked `new: true`, so a guest that still holds an old connection replaces it. The new connection arrives as another `peer` event with the same peer id.

## Connection type badge

Each peer reports whether its connection is direct or relayed:

```js
room.on('peer', peer => {
  showBadge(peer.id, peer.connectionType);         // 'direct', 'relayed' or null
  peer.on('type', type => showBadge(peer.id, type)); // when it changes
});
```

The client reads `getStats()` when the connection opens and every 5 seconds after that. `connectionType` is `'relayed'` when either end of the selected candidate pair is a TURN relay, and `null` until a pair is selected. The helper is exported if you want to run it on your own stats:

```js
import { connectionType } from './handshake.js';
connectionType(await pc.getStats()); // 'direct' | 'relayed' | null
```

## iOS realities

Most players are on iPhones and iPads. Plan for them:

- **Backgrounding or locking the screen suspends JavaScript** and drops the connections. The client requests a [Screen Wake Lock](https://developer.mozilla.org/docs/Web/API/Screen_Wake_Lock_API) while you are in a room and the page is visible, asks again when the page becomes visible, and releases it when you leave. Pass `wakeLock: false` to turn that off. The browser can still refuse it (for example in Low Power Mode), so warn players not to switch apps mid-game, and rely on resume for when they do.
- **Sockets die quietly.** iOS can keep a dead WebSocket looking open; the client's heartbeat detects that and resumes.
- **Low Power Mode caps rendering at 30 fps**, and phones throttle when they get hot. The host does the most work, so prefer an iPad as host.
- **Plan for jitter spikes, not average latency.** Wi-Fi and cellular both stall now and then; see [Game networking](/guides/game-networking/) for how to absorb that.
