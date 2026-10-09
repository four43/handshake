# Built-in TURN Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Handshake runs its own TURN relay (UDP + TCP, PROXY protocol v2 from trusted proxies) so a deployment needs no coturn; `turns:` on 443 comes from Caddy + caddy-l4.

**Architecture:** A new `src/turn/` module tree: pure codecs (`stun.rs`, `proxy.rs`), credential checks (`auth.rs`), allocation state (`allocation.rs`) and the socket layer (`server.rs`, `mod.rs`). `lib.rs` gains config fields, a shared `turn::Stats` for `/metrics`, and `/session` support for a generated secret; `main.rs` starts the TURN server next to HTTP when `relay_ports` is set.

**Tech Stack:** Rust 1.91 (tokio, axum 0.7), RustCrypto `hmac`/`sha1`/`sha2` + new `md-5`; tests via `scripts/test.sh` (cargo in Docker); client e2e with Playwright + Chromium; coturn's `turnutils_uclient` for interop.

**Spec:** `docs/specs/builtin-turn.md` (design, wire details, limits, tests). Read it first: this plan names the work, the spec holds the protocol rules.

## Global Constraints

- External coturn deployments keep working: without `relay_ports` nothing new starts and `TURN_SECRET` behaves as before.
- No TLS/DTLS in Handshake. No TCP relaying, no IPv6 relay addresses, no EVEN-PORT/RESERVATION-TOKEN.
- One new runtime dependency: `md-5`. CRC32 is hand-written.
- Run Rust tests with `scripts/test.sh` (filter: `scripts/test.sh turn`). Never install cargo on the host. `UPDATE_SCHEMAS=1 scripts/test.sh schemas_are_current` after config changes.
- Commits: conventional (`feat:`, `test:`, `docs:` …), **no `Co-Authored-By` and no Claude attribution**.
- `.gitignore` ignores every `config.toml`; test configs have other names. Files mounted into the container need mode 644.
- Match the surrounding style: doc comments on public items and config fields (they become the config reference), section banners in long files, small free functions.

## File Structure

| Path | Change | Responsibility |
|---|---|---|
| `Cargo.toml` | modify | `md-5 = "0.10"` |
| `src/lib.rs` | modify | `TurnConfig` fields + `PortRange`; `pub mod turn`; `App::turn_stats`; `/session` turn flag; TURN lines in `/metrics`; `turn_credential` moves to `turn::auth::password` |
| `src/main.rs` | modify | Generate a secret when built-in and `TURN_SECRET` unset; start `turn::TurnServer` |
| `src/turn/mod.rs` | create | `TurnServer::start`, `Stats`, settings validation |
| `src/turn/stun.rs` | create | STUN/TURN message + ChannelData codec, integrity, fingerprint, TCP framing |
| `src/turn/proxy.rs` | create | PROXY v2 parser |
| `src/turn/auth.rs` | create | Nonces, username/password, long-term key |
| `src/turn/allocation.rs` | create | Allocation state: permissions, channels, lifetimes, buckets, port pool, forbidden peers |
| `src/turn/server.rs` | create | Dispatch, UDP loop, TCP connections, relay tasks, sweeper, resolver |
| `tests/turn.rs` | create | In-process TURN integration tests with a small test client |
| `tests/server.rs` | modify | `/session` with built-in TURN; `/metrics` TURN lines |
| `site/schemas/config.json` | regenerate | Config reference |
| `client/e2e/*`, `client/playwright.config.mjs` | modify | Relay-only e2e over UDP and TCP |
| `scripts/turn-interop.sh`, `.github/workflows/docker.yml` | create/modify | coturn `turnutils_uclient` against the image |
| `Dockerfile`, `config.example.toml`, `README.md`, `site/src/content/docs/guides/{connectivity,self-hosting}.md`, `docs/specs/handshake-server.md` | modify | Docs |

---

### Task 1: Config and secret

**Files:** `Cargo.toml`, `src/lib.rs`, `src/main.rs`, `tests/server.rs`, `site/schemas/config.json`, `src/turn/mod.rs` (stub `Stats`)

**Produces:**
- `pub struct PortRange { pub first: u16, pub last: u16 }`, deserialized from `"49160-49200"` (first ≤ last, both > 0), `JsonSchema` as string (like `Cidr`).
- `TurnConfig` gains `listen: String` (default `"0.0.0.0:3478"`), `relay_ports: Option<PortRange>`, `external_ip: Option<String>`, `max_allocations: usize` (500), `allocations_per_ip: usize` (64), `kbps_per_allocation: u32` (2000), `allowed_peers: Vec<Cidr>` (empty). `TurnConfig::builtin(&self) -> bool` = `relay_ports.is_some()`.
- `turn::Stats` (atomics: `allocations`, `allocations_total`, `bytes_in`, `bytes_out`, `auth_failures`, `quota_rejections`); `App::turn_stats(&self) -> Arc<turn::Stats>`.
- `main.rs`: built-in and no `TURN_SECRET` → 32 random bytes; the warning about a missing secret only for non-built-in.

- [ ] Tests (lib unit): `PortRange` parses `"1-2"`, rejects `"2-1"`, `"0-5"`, `"x"`; `TurnConfig` defaults.
- [ ] Test (`tests/server.rs`): built-in `[turn]` + key from caller → `/session` `turn: true`, `/turn` returns the configured URLs.
- [ ] Implement; `UPDATE_SCHEMAS=1 scripts/test.sh schemas_are_current`; full `scripts/test.sh` green.
- [ ] Commit `feat(turn): config for the built-in TURN server`.

### Task 2: STUN codec (`stun.rs`)

**Produces:**
```rust
pub const MAGIC: u32 = 0x2112A442;
pub type TxId = [u8; 12];
pub enum Class { Request, Indication, Success, Error }
pub mod method { BINDING, ALLOCATE, REFRESH, SEND, DATA, CREATE_PERMISSION, CHANNEL_BIND }  // u16 consts
pub mod attr { USERNAME, MESSAGE_INTEGRITY, ERROR_CODE, UNKNOWN_ATTRIBUTES, CHANNEL_NUMBER, LIFETIME, XOR_PEER_ADDRESS, DATA,
               REALM, NONCE, XOR_RELAYED_ADDRESS, REQUESTED_ADDRESS_FAMILY, EVEN_PORT, REQUESTED_TRANSPORT, DONT_FRAGMENT,
               XOR_MAPPED_ADDRESS, RESERVATION_TOKEN, SOFTWARE, FINGERPRINT }  // u16 consts
pub struct Message<'a> { pub method: u16, pub class: Class, pub tx: TxId, .. }
impl<'a> Message<'a> {
    pub fn parse(buf: &'a [u8]) -> Option<Message<'a>>;          // exactly one message; FINGERPRINT checked if present
    pub fn get(&self, attr: u16) -> Option<&'a [u8]>;              // first occurrence, before MESSAGE-INTEGRITY
    pub fn all(&self, attr: u16) -> impl Iterator<Item = &'a [u8]>;
    pub fn unknown_required(&self) -> Vec<u16>;                   // < 0x8000 and not in KNOWN
    pub fn has_integrity(&self) -> bool;
    pub fn check_integrity(&self, key: &[u8]) -> bool;
}
pub struct Builder { .. }
impl Builder {
    pub fn new(method: u16, class: Class, tx: TxId) -> Self;
    pub fn attr(self, t: u16, v: &[u8]) -> Self;
    pub fn xor_addr(self, t: u16, addr: SocketAddr) -> Self;
    pub fn error(self, code: u16, reason: &str) -> Self;
    pub fn finish(self, key: Option<&[u8]>) -> Vec<u8>;          // + MESSAGE-INTEGRITY if key, + FINGERPRINT always
}
pub fn xor_addr(v: &[u8], tx: &TxId) -> Option<SocketAddr>;
pub fn channel_data(ch: u16, data: &[u8], pad: bool) -> Vec<u8>;
pub fn parse_channel_data(buf: &[u8]) -> Option<(u16, &[u8])>;
pub enum Frame { Need, Len(usize), Bad }
pub fn frame(buf: &[u8]) -> Frame;                                 // TCP framing: STUN or padded ChannelData
pub fn crc32(data: &[u8]) -> u32;
```
- [ ] Tests: RFC 5769 §2.1 request (integrity with short-term key `VOkJxbRl1RmTxUk/WvJxBt`, fingerprint), §2.2 IPv4 response (XOR-MAPPED-ADDRESS 192.0.2.1:32853), §2.4 long-term request (key = MD5 of `マトリックス:example.org:TheMatrIX`); Builder → parse round trip incl. integrity; tampered byte fails integrity/fingerprint; IPv6 xor address; channel data pad/parse; `frame` on partial, STUN, ChannelData (odd length padded), bad leading bits; random/truncated bytes never panic (10k iterations of seeded random + truncations of valid messages).
- [ ] Implement; tests green; commit `feat(turn): STUN message codec`.

### Task 3: PROXY v2 (`proxy.rs`) and auth (`auth.rs`)

**Produces:**
```rust
// proxy.rs
pub const SIGNATURE: [u8; 12];
pub enum Proxy { Need, Header { src: Option<SocketAddr>, len: usize }, Bad }   // src None for LOCAL / UNSPEC
pub fn parse_v2(buf: &[u8]) -> Proxy;
// auth.rs
pub const REALM: &str = "handshake";
pub fn password(secret: &[u8], username: &str) -> String;       // base64(HMAC-SHA1) — lib.rs /turn uses it
pub fn long_term_key(username: &str, realm: &str, password: &str) -> [u8; 16];
pub enum NonceCheck { Valid, Stale, Bad }
pub struct Auth { .. }
impl Auth {
    pub fn new(secret: Vec<u8>, apps: HashSet<String>) -> Self;  // apps with turn = true
    pub fn nonce(&self, client: IpAddr, now: u64) -> String;     // valid NONCE_SECS = 600
    pub fn check_nonce(&self, nonce: &[u8], client: IpAddr, now: u64) -> NonceCheck;
    pub fn user_key(&self, username: &str, now: u64) -> Option<[u8; 16]>;  // None: bad format, expired, unknown app
}
```
- [ ] Tests: PROXY TCP4 / TCP6 / LOCAL / UNSPEC / partial / bad signature / bad version / length past buffer; nonce valid, stale after 600 s, bound to IP, tampered → Bad; `user_key` for valid, expired, unknown app, app without turn, missing colon, non-numeric expiry; `password` equals the old `turn_credential` (test vector from `tests/server.rs::turn_credentials`).
- [ ] Implement; `lib.rs` `/turn` uses `turn::auth::password`; commit `feat(turn): PROXY v2 parser and TURN credential checks`.

### Task 4: Allocation state (`allocation.rs`)

**Produces:**
```rust
pub const PERMISSION_LIFETIME: Duration = 300 s; pub const CHANNEL_LIFETIME: Duration = 600 s;
pub fn lifetime(requested: Option<u32>) -> Duration;              // clamp to 600..=3600 s
pub fn peer_allowed(ip: IpAddr, allowed: &[Cidr]) -> bool;       // spec "Forbidden peers"
pub struct Bucket { .. } impl Bucket { pub fn new(kbps: u32, now: Instant) -> Self; pub fn take(&mut self, bytes: usize, now: Instant) -> bool; }
pub struct State {                                                // per allocation, behind a Mutex
    pub expires: Instant, pub last: Option<(TxId, Vec<u8>)>, pub up: Bucket, pub down: Bucket, ..
}
impl State {
    pub fn new(lifetime: Duration, kbps: u32, now: Instant) -> Self;
    pub fn permit(&mut self, ip: IpAddr, now: Instant);
    pub fn permitted(&self, ip: IpAddr, now: Instant) -> bool;
    pub fn bind(&mut self, ch: u16, peer: SocketAddr, now: Instant) -> bool;   // false: conflict; also permits
    pub fn channel_peer(&self, ch: u16, now: Instant) -> Option<SocketAddr>;
    pub fn peer_channel(&self, peer: SocketAddr, now: Instant) -> Option<u16>;
    pub fn expire(&mut self, now: Instant);
}
pub struct PortPool { .. } impl PortPool { pub fn new(r: PortRange) -> Self; pub fn bind(&mut self) -> Option<(std::net::UdpSocket, u16)>; pub fn release(&mut self, port: u16); }
```
- [ ] Tests: lifetime clamp (None, 0→600? no: 0 is handled by Refresh before; 30→600, 1200, 99999→3600); every forbidden range and a public IP, `allowed_peers` override; bucket allows a burst of one second then refills; permissions expire at 300 s; bind conflicts both ways, rebind same pair refreshes, channel expiry; PortPool hands out distinct ports, None when exhausted, reuses released.
- [ ] Implement; commit `feat(turn): allocation state, quotas and forbidden peers`.

### Task 5: UDP server (`server.rs`, `mod.rs`, `main.rs`)

**Produces:**
```rust
pub struct TurnServer { .. }
impl TurnServer {
    pub async fn start(cfg: &Config, secret: Vec<u8>, stats: Arc<Stats>) -> io::Result<TurnServer>; // binds UDP+TCP on [turn].listen (same port; port 0 picks one)
    pub fn local_addr(&self) -> SocketAddr;
}   // Drop aborts its tasks
```
Dispatch rules, errors and limits: spec "Protocol". Allocations are `Arc<Alloc>` in a table keyed `Key::{Udp(SocketAddr), Tcp(u64)}`, indexed by relay port for relay-to-relay delivery; each relay socket has a task holding a `Weak<Alloc>`.

- [ ] `tests/turn.rs` harness: start `App` + `TurnServer` on `127.0.0.1:0`, `external_ip = "127.0.0.1"`, `allowed_peers = ["127.0.0.0/8"]`, `relay_ports` a high range; a test client (raw `stun` Builder/parse) with `allocate()` doing the 401 dance using `/turn`-style credentials from `turn::auth::password`; a UDP echo peer.
- [ ] Tests (UDP): Binding unauthenticated; Allocate → 401 with REALM/NONCE → success with relayed 127.0.0.1:port in range + mapped + LIFETIME 600; Send without permission dropped; CreatePermission → Send → echo → Data indication from peer; ChannelBind → ChannelData both ways; peer without permission can't reach client; relay to relay between two allocations; Refresh 0 deletes (then Send 437 on Refresh); retransmitted Allocate gets identical response; second Allocate new tx → 437; stale nonce → 438 (Auth with short validity via test-only constructor or a forged expired nonce); wrong password, expired username, app with `turn = false` → 401; forbidden peer (no `allowed_peers`) → 403; `allocations_per_ip = 1` → 486; REQUESTED-TRANSPORT TCP → 442; unknown comprehension-required attribute → 420.
- [ ] Implement UDP path, sweeper, resolver, `main.rs` start; tests green; commit `feat(turn): built-in TURN server over UDP`.

### Task 6: TCP and PROXY protocol

- [ ] Tests: allocate + permission + Send/Data + ChannelData (padded) over TCP; two messages in one write, one message split across writes; closing the connection frees the allocation (port reusable, `Stats.allocations` back to 0); trusted peer with PROXY header → XOR-MAPPED-ADDRESS is the header's address and quota counts it; trusted peer without header works; untrusted peer sending a PROXY header is closed; idle connection without allocation closed (test-only short timeout constant via `#[cfg(test)]`-free config: use `tokio::time::pause` where possible, else a `TurnServer::start_with_timeouts` test helper).
- [ ] Implement; commit `feat(turn): TCP transport and PROXY protocol v2`.

### Task 7: Metrics

- [ ] Test (`tests/server.rs` or `tests/turn.rs`): after one allocation, `/metrics` contains `handshake_turn_allocations 1`, `handshake_turn_allocations_total 1`, `handshake_turn_relayed_bytes_total{direction="out"}` > 0 after a Send; nothing TURN-specific when built-in is off.
- [ ] Implement; commit `feat(turn): TURN metrics`.

### Task 8: End to end and interop

- [ ] `client/e2e/e2e.toml`: built-in `[turn]` (`listen = "0.0.0.0:3478"`, `relay_ports = "49160-49169"`, `external_ip = "127.0.0.1"`, `allowed_peers = ["127.0.0.0/8"]`, urls `turn:127.0.0.1:3478?transport=udp|tcp`); app `turn = true`. `run.sh` publishes `127.0.0.1:3478:3478/udp`, `/tcp`.
- [ ] `page.html`: `?relay=udp|tcp` wraps `RTCPeerConnection` (constructor and `setConfiguration`) with `iceTransportPolicy: 'relay'` and only that transport's URLs.
- [ ] `two-peers.spec.mjs`: new test per transport: connect relay-only, data both ways on both channels, `connectionType === 'relayed'`. Existing test still sees `direct`.
- [ ] `scripts/turn-interop.sh`: run the image with built-in TURN and a known `TURN_SECRET` on a Docker network; run `coturn/coturn`'s `turnutils_uclient` with REST-secret auth (`-W <secret>`, user = app id) against it over UDP and TCP; exit non-zero on failure. Add as a CI step after the e2e test.
- [ ] Run `npm run e2e` and the interop script; commit `test(turn): relay-only e2e and coturn interop`.

### Task 9: Docs

- [ ] `Dockerfile` `EXPOSE 8080 3478/udp 3478/tcp`; `config.example.toml` built-in `[turn]`; `README.md` (Configure/Deploy: no coturn; TURN_SECRET optional).
- [ ] `guides/connectivity.md`: TURN is built in; turning it on; what `/session` reports; `turns:` via proxy.
- [ ] `guides/self-hosting.md`: architecture without coturn; compose with published ports; firewall/router ports; `external_ip` (hostname for dynamic WAN IPs); section "TURN over TLS on 443 with Caddy" with the spike-verified xcaddy Dockerfile, global `servers :443 { listener_wrappers { layer4 … } tls }` block, cert site block, PROXY v2 + `trusted_proxies`, and what was verified; migrating from coturn.
- [ ] `docs/specs/handshake-server.md`: scope and deployment reflect built-in TURN, link `builtin-turn.md`.
- [ ] `cd site && npm run build` succeeds; commit `docs: built-in TURN and turns: through Caddy`.
