# Built-in TURN — Design

Oct 8, 2026 · @Seth Miller

## Why

Players on cellular (carrier-grade NAT) and strict corporate Wi-Fi cannot connect to each other directly; their game
traffic has to go through a TURN relay. Until now Handshake only minted credentials for a separate coturn server, so
every deployment needed a second service, a shared `TURN_SECRET` and a hand-written `turnserver.conf`. Handshake now
runs the TURN relay itself: one container, no shared secret, no coturn.

TLS stays out of Handshake. `turns:` on TCP 443 (the only thing the strictest networks let through) is terminated by
the reverse proxy that already owns 443 for the host's other sites: Caddy with the
[caddy-l4](https://github.com/mholt/caddy-l4) listener wrapper matches the TURN hostname by SNI, terminates TLS with a
certificate it manages, and forwards plain TURN over TCP with a PROXY protocol v2 header. A Docker spike (Caddy
v2.11.7 + caddy-l4, Oct 8, 2026) confirmed all three parts: other HTTPS sites on :443 are unaffected, the PROXY header
arrives with the real client address, and a real TURN Allocate over TLS succeeds through it.

## Scope

In:

- STUN Binding (Handshake doubles as the STUN server; no Google STUN needed).
- TURN (RFC 8656) as browsers use it: Allocate, Refresh, CreatePermission, ChannelBind, Send and Data indications,
  ChannelData; long-term credentials; UDP relaying of IPv4.
- Client transports: UDP and TCP on one port (default 3478). PROXY protocol v2 on TCP from `trusted_proxies`.
- Quotas, bandwidth caps, forbidden peer ranges, metrics.
- Docs: built-in TURN, the Caddy `turns:` setup, router/firewall ports, migrating from coturn.

Out:

- TLS or DTLS inside Handshake (the proxy does TLS; browsers do not use DTLS TURN).
- TCP relaying (RFC 6062), IPv6 relay addresses, port reservations (EVEN-PORT with R=1, RESERVATION-TOKEN). Browsers do
  not ask for them. EVEN-PORT without R (coturn's test client sends it) gets an even port.
- Room-scoped relaying (only relay between members of one room). A good follow-up: TURN now lives in the process
  that knows the rooms.
- Client library changes. The client already takes its ICE servers from `/turn`.

## Configuration

`[turn]` keeps `urls` and `ttl_secs`. The built-in server starts when `relay_ports` is set; without it Handshake
behaves as before (credentials for an external coturn, `TURN_SECRET` required), so existing deployments keep working.

```toml
[turn]
urls = [
  "stun:turn.example.com:3478",
  "turn:turn.example.com:3478?transport=udp",
  "turn:turn.example.com:3478?transport=tcp",
  "turns:turn.example.com:443?transport=tcp",   # through the proxy
]
ttl_secs = 3600
listen = "0.0.0.0:3478"            # UDP and TCP; default
relay_ports = "49160-49200"        # UDP, one per allocation; enables the built-in server
external_ip = "home.example.com"   # IPv4 address or hostname, re-resolved every 60 s; required with relay_ports
max_allocations = 500              # whole server
allocations_per_ip = 64            # per client IP (a relay-only host needs about 3 per guest: udp, tcp, turns)
kbps_per_allocation = 2000         # relayed traffic per allocation, each direction; excess is dropped
allowed_peers = []                 # peer ranges exempt from the forbidden list below (tests, LAN setups)
```

- **`external_ip`** is the address put in XOR-RELAYED-ADDRESS: the host's public IPv4. A hostname suits a home
  connection with a changing WAN address kept in DNS. Startup fails if it cannot be resolved once; later failures keep
  the last address and log a warning.
- **Secret.** `TURN_SECRET` is optional with the built-in server: unset, Handshake generates a random one at startup
  (credentials die with the process, as rooms already do). When set it is used, so a coturn setup can migrate without
  changing anything else.
- **`/session`** reports `"turn": true` when the app has `turn = true`, `[turn]` exists, and there is a secret
  (configured or generated).
- **Ports.** The relay range must be reachable from the internet over UDP, along with `listen` over UDP and TCP.
  Docker publishes a range of ~40 ports fine; host networking is not required.

## Protocol

### Messages

STUN messages (RFC 8489): 20-byte header (type, length, magic cookie `0x2112A442`, 12-byte transaction ID), then
4-byte-aligned TLV attributes. Encoded and decoded by pure functions in `turn/stun.rs`:

| Attribute | Use |
|---|---|
| XOR-MAPPED-ADDRESS | Binding and Allocate responses: the client's address as seen (from the PROXY header when present) |
| XOR-RELAYED-ADDRESS | Allocate response: `external_ip` + relay port |
| XOR-PEER-ADDRESS | CreatePermission, ChannelBind, Send, Data |
| LIFETIME | Allocate/Refresh; requested value clamped to 600 s–3600 s (RFC 8656: below the default means the default), default 600 s; 0 deletes on Refresh |
| REQUESTED-TRANSPORT | Must be UDP (17), else 442 |
| REQUESTED-ADDRESS-FAMILY | IPv4 only, else 440 |
| CHANNEL-NUMBER | 0x4000–0x4FFF |
| DATA | Send/Data indications |
| USERNAME, REALM, NONCE, MESSAGE-INTEGRITY, FINGERPRINT | Auth and integrity |
| ERROR-CODE, SOFTWARE | Responses |
| DONT-FRAGMENT | Accepted and ignored |
| EVEN-PORT | R=0: an even relay port; R=1 (reserve the next port too): 508 |
| any other comprehension-required (< 0x8000) | 420 with UNKNOWN-ATTRIBUTES |

Every response carries FINGERPRINT (CRC32 xor `0x5354554E`). Requests with a FINGERPRINT are checked.

ChannelData: 2-byte channel number (0x4000–0x4FFF), 2-byte length, data; padded to 4 bytes over TCP. The first two
bits tell the framings apart (`00` STUN, `01` ChannelData).

### Authentication

Long-term credentials with a realm of `handshake`. Every TURN request except Binding needs them.

1. No MESSAGE-INTEGRITY → `401` with REALM and NONCE.
2. NONCE is stateless: base64 of (expiry, HMAC-SHA256 over expiry + client address, keyed with a per-process random
   key), valid 10 minutes. Expired → `438 Stale Nonce` with a fresh one. Wrong address or bad MAC → `401`.
3. USERNAME must be `<expiry unix>:<app id>` with the expiry in the future and an app with `turn = true`; the password
   is `base64(HMAC-SHA1(secret, username))`, exactly what `/turn` mints. Otherwise `401`.
4. Key = `MD5(username ":" realm ":" password)`; MESSAGE-INTEGRITY (HMAC-SHA1) must verify, else `401`.
5. Within an allocation the USERNAME must not change (`441 Wrong Credentials`).

Responses to authenticated requests carry MESSAGE-INTEGRITY with the same key. Usernames are ASCII, so SASLprep is
not needed.

### Allocations

Keyed by the 5-tuple: for UDP the client address plus the listening socket; for TCP the connection. One per key.

- **Allocate**: auth, then 437 if one exists (unless it is a retransmission: the same transaction ID gets the cached
  response), quota checks (486 per IP, 508 server-wide or no free port), bind a UDP socket on a free port in
  `relay_ports`, reply with XOR-RELAYED-ADDRESS, XOR-MAPPED-ADDRESS and LIFETIME.
- **Refresh**: new LIFETIME; 0 deletes. 437 if there is no allocation.
- **CreatePermission**: one or more XOR-PEER-ADDRESS; each peer IP gets a 300 s permission. A forbidden peer → 403
  and no permissions from that request.
- **ChannelBind**: binds channel ↔ peer address for 600 s (also installs/refreshes the permission). A channel already
  bound to another peer, or a peer already bound to another channel → 400.
- **Send indication** / **ChannelData** from the client: dropped unless the peer IP has a permission; otherwise sent
  from the relay socket.
- **Relay socket receives**: dropped unless the source IP has a permission; sent to the client as ChannelData when the
  source address has a channel, else as a Data indication.
- **Expiry**: a sweep every few seconds drops expired allocations, permissions and channels. Closing a TCP connection
  deletes its allocation at once.

### Limits and safety

- **Forbidden peers** (403, never relayed to; `external_ip` itself is fine): loopback, unspecified, private (10/8, 172.16/12, 192.168/16),
  carrier-grade NAT (100.64/10), link-local (169.254/16, includes cloud metadata), multicast, broadcast, reserved
  (240/4), 0/8. `allowed_peers` exempts ranges. Without this, the relay is a path into the server's own network.
- **Amplification**: an unauthenticated request gets an error response no bigger than it needs (401 is small);
  nothing is relayed before auth.
- **Bandwidth**: per allocation, a token bucket per direction at `kbps_per_allocation`; packets over it are dropped.
- **Quotas**: `max_allocations`, `allocations_per_ip` (by client IP from the PROXY header when present).
- **TCP**: at most `max_allocations` + 256 connections at once (more are closed on accept); a connection must send a complete first message within 10 s (and, from a trusted proxy, its PROXY header);
  without an allocation it is closed after 30 s idle; at most 64 KiB buffered per frame.
- **PROXY protocol v2**: honored only from peers in `trusted_proxies`, and optional there: a trusted peer's connection
  that starts with the v2 signature (first byte `0x0D`; STUN and ChannelData never start with it) is read as coming
  from the address in the header, anything else as a direct client. The default `trusted_proxies` covers LAN ranges,
  so requiring the header would cut off LAN players using TCP TURN directly. From untrusted peers the header is never
  parsed (the connection fails as an invalid frame). v1 (text) is not supported.
- **Relay to relay on this server**: a Send/ChannelData to `external_ip` at a port in `relay_ports` that belongs to a
  live allocation is delivered in-process, as if that relay socket had received it from the sender's relay address.
  Two relayed players are common (both on cellular), and the alternative depends on the router hairpinning its own
  WAN address. Any other port on `external_ip` is never sent to: permissions are per IP, so a permission for
  another allocation's relay address would otherwise reach every service on the host.

## Code layout

| Unit | Job |
|---|---|
| `src/turn/mod.rs` | `TurnServer`: binds UDP + TCP, spawns tasks, exposes `Stats`; config validation helpers |
| `src/turn/stun.rs` | Message codec, attributes, MESSAGE-INTEGRITY, FINGERPRINT, CRC32, ChannelData; pure |
| `src/turn/auth.rs` | Nonce mint/check, username + password check, long-term key |
| `src/turn/proxy.rs` | PROXY protocol v2 header parser; pure |
| `src/turn/allocation.rs` | Allocation table: create/refresh/permissions/channels/expiry/quotas; no I/O beyond the relay socket |
| `src/turn/server.rs` | Request dispatch, UDP loop, TCP connection tasks, relay socket tasks |

`lib.rs` keeps `Config`/`TurnConfig` (new fields), `/session`, `/turn` and `/metrics`; `main.rs` starts the TURN server
next to the HTTP server when `relay_ports` is set. New dependency: `md-5` (RustCrypto, like `sha1`).

## Metrics

| Metric | Type |
|---|---|
| `handshake_turn_allocations` | gauge |
| `handshake_turn_allocations_total` | counter |
| `handshake_turn_relayed_bytes_total{direction="in"\|"out"}` | counter (in: peer → client, out: client → peer) |
| `handshake_turn_auth_failures_total` | counter |
| `handshake_turn_quota_rejections_total` | counter |

## Testing

- **Unit** (`stun.rs`, `proxy.rs`, `auth.rs`): RFC 5769 test vectors (sample request, IPv4 response, long-term
  auth request with the post-SASLprep strings); round trips; malformed input (truncated, bad lengths, bad padding,
  random bytes) never panics; PROXY v2 IPv4/IPv6/LOCAL/garbage; nonce expiry and address binding.
- **Integration** (`tests/turn.rs`, in-process, a small Rust TURN client in the test): over UDP and TCP — Binding
  without auth; 401 → Allocate; Send → UDP echo peer → Data indication; ChannelBind → ChannelData both ways;
  permissions required both ways; relay to relay in-process; Refresh 0 deletes; retransmitted Allocate; 437; stale nonce 438; wrong password,
  expired username, app with `turn = false` → 401; forbidden peer 403; `allowed_peers`; quotas 486; TCP frames split
  across writes and several per write; PROXY header from trusted vs untrusted peers, trusted peer without a header; `/session` says `turn: true` with
  a generated secret.
- **Interop**: coturn's `turnutils_uclient` (Docker) allocates and relays through Handshake using `/turn`-style
  credentials (its `-W` REST-secret mode).
- **End to end** (`client/e2e`): two Chromium pages forced to relay only (`iceTransportPolicy: "relay"`), once over
  `transport=udp` and once over `transport=tcp`; data flows both ways and `connectionType` is `relayed`.
- **Not covered by tests**: real carrier NAT and corporate middleboxes, load. Validate on a staging deploy with a phone
  on cellular and a corporate laptop.

## Docs

- `guides/connectivity.md`: TURN is built in; how to turn it on; credentials unchanged.
- `guides/self-hosting.md`: no coturn; ports to open/forward; `external_ip`; the Caddy `turns:` setup (xcaddy build,
  listener wrapper, cert site block, `trusted_proxies`), from the spike; migrating from coturn.
- Config reference (generated schema), `config.example.toml`, `README.md`, `Dockerfile` (`EXPOSE 3478/udp 3478/tcp`),
  `docs/specs/handshake-server.md` (scope and deployment sections).
