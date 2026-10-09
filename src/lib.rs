//! handshake: WebRTC signaling + room registry for browser games.
//!
//! The server matches players into rooms and relays opaque SDP/ICE blobs
//! between a room's host and its peers. Game traffic never touches it.
//! See the "Handshake — Signaling Server Spec" doc for the protocol.
//!
//! The binary (`main.rs`) loads config and secrets and calls [`serve`];
//! everything else lives here so tests can run the server in-process.

use std::{
    collections::HashMap,
    fmt::Write as _,
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, State,
    },
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{
    engine::general_purpose::URL_SAFE_NO_PAD,
    Engine,
};
use futures_util::{stream::SplitStream, SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use rand::{Rng, RngCore};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::sync::mpsc;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::{info, warn};

pub mod turn;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const SIGNAL_VERSION: u32 = 1;
const MAX_WS_MESSAGE: usize = 16 * 1024;
const HELLO_DEADLINE: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const PING_EVERY: Duration = Duration::from_secs(20);
const SWEEP_EVERY: Duration = Duration::from_secs(5);
const OUTBOUND_QUEUE: usize = 64;
const MAX_META_BYTES: usize = 1024;
const MAX_NAME_CHARS: usize = 32;
const MAX_LISTED: usize = 50;
const CODE_LEN: usize = 5;
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789"; // no 0/O, 1/I/L

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// The server's `config.toml`. Secrets never go here: they come from the
/// environment (`SESSION_SECRET`, `SESSION_SECRET_PREV`, `TURN_SECRET`).
#[derive(Deserialize, Clone)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Config {
    /// Address and port to listen on.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Reverse proxies whose `X-Forwarded-For` is believed, as IP ranges. From any other peer the header is
    /// ignored and the socket address is the client, so per-IP limits hold with or without a proxy in front.
    /// The default covers a proxy on the same host or a private (Docker) network.
    #[serde(default = "default_trusted_proxies")]
    pub trusted_proxies: Vec<Cidr>,
    /// Replaced by `trusted_proxies`; still read so the server can warn about it.
    #[cfg_attr(test, schemars(skip))]
    #[serde(default)]
    pub trust_proxy: Option<bool>,
    /// Accept `http://localhost:*` and `http://127.0.0.1:*` origins for every app. For local development only.
    #[serde(default)]
    pub allow_localhost: bool,
    /// Token lifetimes, grace periods, room ages and rate limits.
    #[serde(default)]
    pub limits: Limits,
    /// TURN relay offered to clients through `POST /turn`. Leave out to disable TURN for every app.
    pub turn: Option<TurnConfig>,
    /// One `[apps.<id>]` table per game. The id is what the client passes as `app`.
    #[serde(default)]
    pub apps: HashMap<String, AppConfig>,
}

/// Timeouts and rate limits, shared by all apps.
#[derive(Deserialize, Clone)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(default)]
pub struct Limits {
    /// Lifetime of a session token from `POST /session`, in seconds. The client renews it before it expires.
    pub session_ttl_secs: u64,
    /// How long a member who dropped keeps their slot (and a host keeps the room) waiting for a resume, in seconds.
    pub grace_secs: u64,
    /// A room older than this is closed, in seconds.
    pub room_max_age_secs: u64,
    /// A room whose host has been alone this long is closed, in seconds.
    pub idle_room_secs: u64,
    /// Session tokens minted per client IP per minute.
    pub sessions_per_min: u32,
    /// Join and peek attempts per client IP per minute.
    pub joins_per_min: u32,
    /// Failed joins and peeks (`not_found`, `bad_key`) per client IP (IPv6: per /64) per app per minute.
    pub ip_failed_joins_per_min: u32,
    /// Failed joins and peeks per app per minute, across all IPs: a ceiling against guessing from many addresses.
    /// Keep it well above `ip_failed_joins_per_min`, so a few guessing IPs cannot lock out every join.
    pub app_failed_joins_per_min: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            session_ttl_secs: 900,
            grace_secs: 30,
            room_max_age_secs: 12 * 3600,
            idle_room_secs: 1800,
            sessions_per_min: 30,
            joins_per_min: 20,
            ip_failed_joins_per_min: 10,
            app_failed_joins_per_min: 2000,
        }
    }
}

/// The TURN relay. With `relay_ports` set, Handshake runs it itself (docs/specs/builtin-turn.md); without, it mints
/// credentials for an external coturn whose `static-auth-secret` equals `TURN_SECRET`.
#[derive(Deserialize, Clone)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct TurnConfig {
    /// `stun:`, `turn:` and `turns:` URLs handed to clients, e.g. `turn:turn.example.com:3478?transport=udp`.
    pub urls: Vec<String>,
    /// Lifetime of a TURN credential, in seconds.
    #[serde(default = "default_turn_ttl")]
    pub ttl_secs: u64,
    /// Built-in server: address and port for TURN over UDP and TCP.
    #[serde(default = "default_turn_listen")]
    pub listen: String,
    /// Built-in server: UDP ports for relay addresses, one per allocation, e.g. `"49160-49200"`. Setting it starts
    /// the built-in server.
    #[serde(default)]
    pub relay_ports: Option<PortRange>,
    /// Built-in server: the public IPv4 address clients and peers reach the relay ports on, or a hostname that
    /// resolves to it (re-resolved every minute, for a home connection whose address changes). Required with
    /// `relay_ports`.
    #[serde(default)]
    pub external_ip: Option<String>,
    /// Built-in server: most allocations at once (at least 1).
    #[serde(default = "default_max_allocations")]
    pub max_allocations: usize,
    /// Built-in server: most allocations per client IP (at least 1). A relay-only host needs about three per guest (UDP, TCP, TLS).
    #[serde(default = "default_allocations_per_ip")]
    pub allocations_per_ip: usize,
    /// Built-in server: relayed traffic per allocation and direction, in kilobits per second (at least 1). Packets over
    /// it are dropped; a burst of up to a second's worth, and never less than 64 KiB, passes at once.
    #[serde(default = "default_kbps_per_allocation")]
    pub kbps_per_allocation: u32,
    /// Built-in server: peer ranges the relay may send to although they are private, loopback or otherwise forbidden.
    #[serde(default)]
    pub allowed_peers: Vec<Cidr>,
    /// Built-in server: TCP peers whose PROXY protocol v2 header is believed, as IP ranges: the reverse proxy that
    /// terminates `turns:`. Empty by default, so no client can claim another address; from any peer outside these
    /// ranges the header is refused.
    #[serde(default)]
    pub proxy_protocol_from: Vec<Cidr>,
}

impl TurnConfig {
    /// True when Handshake runs the TURN server itself.
    pub fn builtin(&self) -> bool {
        self.relay_ports.is_some()
    }
}

/// A range of ports such as `49160-49200`, both ends included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortRange {
    pub first: u16,
    pub last: u16,
}

impl std::str::FromStr for PortRange {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("not a port range: {s:?} (expected e.g. \"49160-49200\")");
        let (a, b) = s.split_once('-').ok_or_else(bad)?;
        let (first, last): (u16, u16) = (a.trim().parse().map_err(|_| bad())?, b.trim().parse().map_err(|_| bad())?);
        if first == 0 || first > last {
            return Err(bad());
        }
        Ok(PortRange { first, last })
    }
}

impl std::fmt::Display for PortRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.first, self.last)
    }
}

impl<'de> Deserialize<'de> for PortRange {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
impl serde::Serialize for PortRange {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

#[cfg(test)]
impl schemars::JsonSchema for PortRange {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "PortRange".into()
    }
    fn inline_schema() -> bool {
        true
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "string" })
    }
}

/// What `list` returns for an app.
#[derive(Deserialize, Clone, Copy, Default, PartialEq, Eq, Debug)]
#[cfg_attr(test, derive(schemars::JsonSchema, serde::Serialize))] // Serialize: the schema shows the default
pub enum ListMode {
    /// Public rooms for the caller's version, nearby first (docs/specs/handshake-server.md "Public listing").
    #[default]
    #[serde(rename = "all")]
    All,
    /// Nothing: a public room is joinable with its code, but its code is never listed.
    #[serde(rename = "none")]
    Hidden,
}

/// One game.
#[derive(Deserialize, Clone)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct AppConfig {
    /// Page origins allowed to use this app, e.g. `https://example.github.io`. An origin has no path.
    pub origins: Vec<String>,
    /// Most members in a room, host included. A room can ask for fewer.
    #[serde(default = "default_max_players")]
    pub max_players: u8,
    /// Most rooms open at once for this app.
    #[serde(default = "default_max_rooms")]
    pub max_rooms: usize,
    /// Allow public rooms, which can be joined with the code alone. Otherwise `create` with `public: true` fails with `public_disabled`.
    #[serde(default)]
    pub public_rooms: bool,
    /// What `list` returns: `"all"` lists public rooms, `"none"` lists nothing.
    #[serde(default)]
    pub list: ListMode,
    /// Hand out TURN credentials to this app's sessions. Needs a top-level `[turn]` table.
    #[serde(default)]
    pub turn: bool,
}

/// An IP range such as `10.0.0.0/8` or `::1/128`. A bare address is a single host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    net: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn contains(&self, ip: IpAddr) -> bool {
        // Dual-stack sockets report IPv4 peers as IPv4-mapped IPv6.
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        let (net, addr, bits) = match (self.net, ip) {
            (IpAddr::V4(n), IpAddr::V4(a)) => (u32::from(n) as u128, u32::from(a) as u128, 32),
            (IpAddr::V6(n), IpAddr::V6(a)) => (u128::from(n), u128::from(a), 128),
            _ => return false,
        };
        self.prefix == 0 || (net ^ addr) >> (bits - self.prefix as u32) == 0
    }
}

impl std::str::FromStr for Cidr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("not an IP range: {s:?} (expected e.g. \"10.0.0.0/8\")");
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let net: IpAddr = addr.parse().map_err(|_| bad())?;
        let max = if net.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => max,
            Some(p) => p.parse::<u8>().ok().filter(|p| *p <= max).ok_or_else(bad)?,
        };
        Ok(Cidr { net, prefix })
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.net, self.prefix)
    }
}

impl<'de> Deserialize<'de> for Cidr {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

// The schema test renders the default list, so it needs these; the server does not.
#[cfg(test)]
impl serde::Serialize for Cidr {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

#[cfg(test)]
impl schemars::JsonSchema for Cidr {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Cidr".into()
    }
    fn inline_schema() -> bool {
        true
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "string" })
    }
}

fn default_trusted_proxies() -> Vec<Cidr> {
    ["127.0.0.0/8", "::1/128", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7"]
        .iter()
        .map(|s| s.parse().expect("valid default range"))
        .collect()
}

pub fn default_listen() -> String {
    "0.0.0.0:8080".into()
}
fn default_turn_ttl() -> u64 {
    3600
}
fn default_turn_listen() -> String {
    "0.0.0.0:3478".into()
}
fn default_max_allocations() -> usize {
    500
}
fn default_allocations_per_ip() -> usize {
    64
}
fn default_kbps_per_allocation() -> u32 {
    2000
}
fn default_max_players() -> u8 {
    8
}
fn default_max_rooms() -> usize {
    50
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

type PeerId = String;
type ConnId = u64;

/// Shared server state. Build with [`App::new`], then hand to [`serve`].
pub struct App {
    cfg: Config,
    session_keys: Vec<Vec<u8>>, // current first, then previous (verify only)
    turn_key: Option<Vec<u8>>,
    inner: Mutex<Inner>,
    metrics: Metrics,
    turn_stats: Arc<turn::Stats>,
    next_conn: AtomicU64,
}

#[derive(Default)]
struct Metrics {
    sessions: AtomicU64,
    turn: AtomicU64,
    joins_rejected: AtomicU64,
    sockets: AtomicU64,
}

type RateMap = HashMap<(IpAddr, &'static str), (Instant, u32)>;
type FailMap = HashMap<String, (Instant, u32)>; // app id, or app id + IP group (fail_keys) -> (window start, failures)

#[derive(Default)]
struct Inner {
    conns: Conns,
    rooms: HashMap<String, HashMap<String, Room>>, // app id -> code -> room
    resume: HashMap<String, (String, String, PeerId)>, // token -> (app, code, peer)
    rate: RateMap,
    app_fails: FailMap,
}

/// Live, authenticated sockets. Sends never block: a socket whose queue is
/// full is marked dead and disconnected after the current operation.
#[derive(Default)]
struct Conns {
    map: HashMap<ConnId, Conn>,
    dead: Vec<ConnId>,
}

struct Conn {
    tx: mpsc::Sender<String>,
    app: String,
    version: u32,
    ip: IpAddr,
    ip_group: String,
    room: Option<(String, PeerId)>, // (code, my peer id)
}

struct Room {
    code: String,
    key: String,
    name: String,
    public: bool,
    version: u32,
    max_players: u8,
    locked: bool,
    meta: Value,
    host: PeerId,
    members: Vec<Member>, // join order; host is first
    host_ip_group: String,
    created: Instant,
    alone_since: Option<Instant>,
}

struct Member {
    id: PeerId,
    name: String,
    resume: String,
    conn: Option<ConnId>,
    away_since: Option<Instant>,
    ip_group: String, // net_group at join time; compared with the room's host_ip_group for `nearby`
}

#[derive(Deserialize)]
struct Claims {
    a: String, // app id
    v: u32,    // game protocol version
    e: u64,    // expiry, unix seconds
}

// ---------------------------------------------------------------------------
// Client -> server messages
// ---------------------------------------------------------------------------

/// A message from client to server, tagged by `t`.
#[derive(Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(tag = "t", rename_all = "snake_case")]
enum In {
    /// First message on a new socket, within 10 s. Answered by `welcome`.
    Hello {
        /// Signaling protocol version, currently `1`.
        #[allow(dead_code)]
        v: u32,
        /// Session token from `POST /session`.
        #[allow(dead_code)]
        token: String,
    },
    /// Create a room and become its host. Answered by `joined`. Not while in a room.
    Create {
        /// Public rooms can be joined with the code alone and may be listed. Needs `public_rooms` on the app.
        #[serde(default)]
        public: bool,
        /// Room name shown in listings, cut to 32 characters. Defaults to `Room`.
        #[serde(default)]
        name: Option<String>,
        /// The host's display name, cut to 32 characters. Defaults to `Host`.
        #[serde(default)]
        player: Option<String>,
        /// Room size, host included, from 2 up to the app's `max_players` (the default).
        #[serde(default)]
        max_players: Option<u8>,
        /// Any JSON the lobby shows (map, mode...), at most 1 KB. The server never reads it.
        #[serde(default)]
        meta: Option<Value>,
    },
    /// Join a room by code. Answered by `joined`. Not while in a room.
    Join {
        /// The 5-character room code. Case-insensitive.
        code: String,
        /// The room key. Needed for a private room.
        #[serde(default)]
        key: Option<String>,
        /// Your display name, cut to 32 characters. Defaults to `Player`.
        #[serde(default)]
        player: Option<String>,
    },
    /// Take your place back after a dropped socket, within the grace period. Answered by `joined` with `resumed: true`.
    Resume {
        /// The `resume` token from the last `joined`.
        token: String,
    },
    /// Look at a room without joining it. Answered by `room_info`. Counts as a join attempt for rate limits.
    Peek {
        /// The 5-character room code.
        code: String,
        /// The room key. Needed for a private room.
        #[serde(default)]
        key: Option<String>,
    },
    /// List the app's open public rooms for your version, nearby first. Answered by `rooms`.
    List,
    /// Relay WebRTC signaling (SDP or ICE) to another member. Only between the host and a peer.
    Signal {
        /// The member's peer id.
        to: PeerId,
        /// Opaque signaling data, passed through untouched.
        data: Value,
    },
    /// Host only: stop (or allow) new joins.
    Lock {
        /// `true` to lock.
        locked: bool,
    },
    /// Host only: replace the room's metadata. Members get `room_meta`.
    Meta {
        /// Any JSON, at most 1 KB.
        meta: Value,
    },
    /// Host only: remove a peer. They can rejoin unless the room is locked.
    Kick {
        /// The peer id to remove.
        peer: PeerId,
    },
    /// Leave the room. When the host leaves, the room closes.
    Leave,
}

// ---------------------------------------------------------------------------
// Conns / Room helpers
// ---------------------------------------------------------------------------

impl In {
    /// The message's `t`, echoed as `re` in an error that answers it.
    fn kind(&self) -> &'static str {
        match self {
            In::Hello { .. } => "hello",
            In::Create { .. } => "create",
            In::Join { .. } => "join",
            In::Resume { .. } => "resume",
            In::Peek { .. } => "peek",
            In::List => "list",
            In::Signal { .. } => "signal",
            In::Lock { .. } => "lock",
            In::Meta { .. } => "meta",
            In::Kick { .. } => "kick",
            In::Leave => "leave",
        }
    }
}

impl Conns {
    fn send(&mut self, id: ConnId, msg: &Value) {
        if let Some(c) = self.map.get(&id) {
            if c.tx.try_send(msg.to_string()).is_err() && !self.dead.contains(&id) {
                self.dead.push(id);
            }
        }
    }

    fn error(&mut self, id: ConnId, code: &str, message: &str) {
        self.send(id, &json!({ "t": "error", "code": code, "message": message }));
    }

    /// An error in answer to a request: `re` is that request's `t`, so a client with several requests in flight
    /// can tell which one failed.
    fn reply(&mut self, id: ConnId, re: &str, code: &str, message: &str) {
        self.send(id, &json!({ "t": "error", "code": code, "message": message, "re": re }));
    }

    fn bind(&mut self, id: ConnId, code: &str, peer: &str) {
        if let Some(c) = self.map.get_mut(&id) {
            c.room = Some((code.to_string(), peer.to_string()));
        }
    }

    fn unbind(&mut self, id: ConnId) {
        if let Some(c) = self.map.get_mut(&id) {
            c.room = None;
        }
    }
}

impl Room {
    fn member(&self, id: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.id == id)
    }

    fn member_mut(&mut self, id: &str) -> Option<&mut Member> {
        self.members.iter_mut().find(|m| m.id == id)
    }

    fn host_present(&self) -> bool {
        self.member(&self.host).is_some_and(|m| m.conn.is_some())
    }

    fn broadcast(&self, conns: &mut Conns, msg: &Value, except: Option<&str>) {
        for m in &self.members {
            if Some(m.id.as_str()) == except {
                continue;
            }
            if let Some(c) = m.conn {
                conns.send(c, msg);
            }
        }
    }

    fn send_host(&self, conns: &mut Conns, msg: &Value) {
        if let Some(c) = self.member(&self.host).and_then(|m| m.conn) {
            conns.send(c, msg);
        }
    }

    fn meta_msg(&self) -> Value {
        json!({ "t": "room_meta", "meta": self.meta, "locked": self.locked })
    }

    /// The `joined` message for one member.
    fn view(&self, you: &str, resumed: bool) -> Value {
        let is_host = you == self.host;
        let resume = self.member(you).map(|m| m.resume.as_str()).unwrap_or("");
        let peers: Vec<Value> = self
            .members
            .iter()
            .map(|m| {
                json!({ "id": m.id, "name": m.name, "away": m.away_since.is_some(), "nearby": m.ip_group == self.host_ip_group })
            })
            .collect();
        json!({
            "t": "joined",
            "resumed": resumed,
            "room": {
                "code": self.code,
                "name": self.name,
                "public": self.public,
                "max_players": self.max_players,
                "locked": self.locked,
                "meta": self.meta,
                "host": self.host,
                "you": you,
                "is_host": is_host,
                "peers": peers,
                "resume": resume,
                "key": if is_host { Some(&self.key) } else { None },
            }
        })
    }
}

fn remove_member(
    room: &mut Room,
    peer: &str,
    conns: &mut Conns,
    resume: &mut HashMap<String, (String, String, PeerId)>,
    reason: &str,
) {
    let Some(i) = room.members.iter().position(|m| m.id == peer) else {
        return;
    };
    let m = room.members.remove(i);
    resume.remove(&m.resume);
    if let Some(c) = m.conn {
        conns.unbind(c);
        if reason == "kicked" {
            conns.send(c, &json!({ "t": "kicked" }));
        }
    }
    room.broadcast(conns, &json!({ "t": "peer_left", "peer": peer, "reason": reason }), None);
    if room.members.len() == 1 {
        room.alone_since = Some(Instant::now());
    }
    info!(room = %room.code, peer, reason, "peer removed");
}

fn close_room(
    room: Room,
    conns: &mut Conns,
    resume: &mut HashMap<String, (String, String, PeerId)>,
    reason: &str,
) {
    let msg = json!({ "t": "room_closed", "reason": reason });
    for m in &room.members {
        resume.remove(&m.resume);
        if let Some(c) = m.conn {
            conns.unbind(c);
            conns.send(c, &msg);
        }
    }
    info!(room = %room.code, reason, "room closed");
}

/// Resolve the caller's room and check they host it, sending errors otherwise.
fn host_room<'r>(
    app_rooms: &'r mut HashMap<String, Room>,
    bound: &Option<(String, PeerId)>,
    conns: &mut Conns,
    conn: ConnId,
    re: &str,
) -> Option<&'r mut Room> {
    let Some((code, me)) = bound else {
        conns.reply(conn, re, "not_in_room", "join a room first");
        return None;
    };
    let Some(room) = app_rooms.get_mut(code) else {
        conns.reply(conn, re, "not_in_room", "room no longer exists");
        return None;
    };
    if &room.host != me {
        conns.reply(conn, re, "not_host", "only the host can do that");
        return None;
    }
    Some(room)
}

// ---------------------------------------------------------------------------
// Core logic
// ---------------------------------------------------------------------------

impl App {
    /// `session_keys`: current secret first, then previous ones (verify only).
    pub fn new(cfg: Config, session_keys: Vec<Vec<u8>>, turn_key: Option<Vec<u8>>) -> Self {
        assert!(!session_keys.is_empty(), "at least one session key is required");
        Self {
            cfg,
            session_keys,
            turn_key,
            inner: Mutex::new(Inner::default()),
            metrics: Metrics::default(),
            turn_stats: Arc::default(),
            next_conn: AtomicU64::new(1),
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// The built-in TURN server's counters, rendered by `/metrics`; hand them to [`turn::TurnServer::start`].
    pub fn turn_stats(&self) -> Arc<turn::Stats> {
        self.turn_stats.clone()
    }

    fn grace(&self) -> Duration {
        Duration::from_secs(self.cfg.limits.grace_secs)
    }

    // ---- auth ---------------------------------------------------------

    fn origin_ok(&self, app: &AppConfig, origin: &str) -> bool {
        app.origins.iter().any(|o| o == origin) || (self.cfg.allow_localhost && is_localhost(origin))
    }

    fn any_origin_ok(&self, origin: &str) -> bool {
        self.cfg.apps.values().any(|a| self.origin_ok(a, origin))
    }

    fn mint_token(&self, app: &str, version: u32) -> (String, u64) {
        let ttl = self.cfg.limits.session_ttl_secs;
        let claims = json!({ "a": app, "v": version, "e": now_unix() + ttl });
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        let sig = hmac_sha256(&self.session_keys[0], payload.as_bytes());
        (format!("{payload}.{}", URL_SAFE_NO_PAD.encode(sig)), ttl)
    }

    fn verify_token(&self, token: &str) -> Option<Claims> {
        let (payload, sig) = token.split_once('.')?;
        let sig = URL_SAFE_NO_PAD.decode(sig).ok()?;
        let valid = self.session_keys.iter().any(|key| {
            let mut mac = <Hmac<Sha256>>::new_from_slice(key).expect("hmac accepts any key length");
            mac.update(payload.as_bytes());
            mac.verify_slice(&sig).is_ok()
        });
        if !valid {
            return None;
        }
        let claims: Claims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
        (claims.e > now_unix() && self.cfg.apps.contains_key(&claims.a)).then_some(claims)
    }

    fn client_ip(&self, headers: &HeaderMap, addr: SocketAddr) -> IpAddr {
        let xff = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok());
        forwarded_client(&self.cfg.trusted_proxies, xff, addr.ip())
    }

    // ---- socket lifecycle ----------------------------------------------

    fn disconnect(&self, inner: &mut Inner, conn: ConnId) {
        let Some(c) = inner.conns.map.remove(&conn) else {
            return;
        };
        self.metrics.sockets.fetch_sub(1, Ordering::Relaxed);
        let Some((code, peer)) = c.room else {
            return;
        };
        let Inner { conns, rooms, .. } = inner;
        let Some(room) = rooms.get_mut(&c.app).and_then(|r| r.get_mut(&code)) else {
            return;
        };
        let Some(m) = room.member_mut(&peer) else {
            return;
        };
        if m.conn != Some(conn) {
            return; // already resumed on a newer socket
        }
        m.conn = None;
        m.away_since = Some(Instant::now());
        if peer == room.host {
            let msg = json!({ "t": "host_away", "grace_secs": self.cfg.limits.grace_secs });
            room.broadcast(conns, &msg, Some(&peer));
        } else {
            room.send_host(conns, &json!({ "t": "peer_away", "peer": peer }));
        }
    }

    fn reap_dead(&self, inner: &mut Inner) {
        while let Some(id) = inner.conns.dead.pop() {
            warn!(conn = id, "outbound queue full; dropping socket");
            self.disconnect(inner, id);
        }
    }

    fn sweep(&self, inner: &mut Inner) {
        let now = Instant::now();
        let grace = self.grace();
        let max_age = Duration::from_secs(self.cfg.limits.room_max_age_secs);
        let idle = Duration::from_secs(self.cfg.limits.idle_room_secs);
        let Inner { conns, rooms, resume, rate, app_fails } = inner;

        for app_rooms in rooms.values_mut() {
            let mut to_close: Vec<(String, &str)> = Vec::new();
            for (code, room) in app_rooms.iter_mut() {
                let host_away = room.member(&room.host).and_then(|m| m.away_since);
                if host_away.is_some_and(|t| now - t >= grace) {
                    to_close.push((code.clone(), "host_gone"));
                    continue;
                }
                if now - room.created >= max_age {
                    to_close.push((code.clone(), "expired"));
                    continue;
                }
                if room.alone_since.is_some_and(|t| now - t >= idle) {
                    to_close.push((code.clone(), "idle"));
                    continue;
                }
                let timed_out: Vec<PeerId> = room
                    .members
                    .iter()
                    .filter(|m| m.id != room.host && m.away_since.is_some_and(|t| now - t >= grace))
                    .map(|m| m.id.clone())
                    .collect();
                for peer in timed_out {
                    remove_member(room, &peer, conns, resume, "timeout");
                }
            }
            for (code, reason) in to_close {
                if let Some(room) = app_rooms.remove(&code) {
                    close_room(room, conns, resume, reason);
                }
            }
        }
        rooms.retain(|_, r| !r.is_empty());
        rate.retain(|_, (t, _)| now - *t < Duration::from_secs(60));
        app_fails.retain(|_, (t, _)| now - *t < Duration::from_secs(60));
        self.reap_dead(inner);
    }

    // ---- message handling ----------------------------------------------

    /// Checks shared by `join` and `peek`, in order: not already in a room,
    /// the per-IP attempt budget, then the IP's and the app's failed-attempt budgets.
    fn join_gate(
        &self,
        bound: &Option<(String, PeerId)>,
        rate: &mut RateMap,
        app_fails: &mut FailMap,
        ip: IpAddr,
        fails: &[String; 2],
    ) -> Result<(), (&'static str, &'static str)> {
        if bound.is_some() {
            return Err(("already_in_room", "leave your current room first"));
        }
        if !rate_allow(rate, ip, "join", self.cfg.limits.joins_per_min) {
            return Err(("rate_limited", "too many join attempts; wait a minute"));
        }
        if app_fails_over(app_fails, &fails[1], self.cfg.limits.ip_failed_joins_per_min) {
            return Err(("rate_limited", "too many failed join attempts; wait a minute"));
        }
        if app_fails_over(app_fails, &fails[0], self.cfg.limits.app_failed_joins_per_min) {
            return Err(("rate_limited", "too many failed join attempts for this game; wait a minute"));
        }
        Ok(())
    }

    fn handle(&self, inner: &mut Inner, conn: ConnId, msg: In) {
        let re = msg.kind();
        let Inner { conns, rooms, resume, rate, app_fails } = inner;
        let Some(c) = conns.map.get(&conn) else {
            return;
        };
        let (app_id, version, ip, ip_group, bound) =
            (c.app.clone(), c.version, c.ip, c.ip_group.clone(), c.room.clone());
        let near = net_group(ip); // for `nearby`; ip_group stays per address for the rate limits
        let Some(app) = self.cfg.apps.get(&app_id) else {
            return;
        };
        let app_rooms = rooms.entry(app_id.clone()).or_default();
        let fails = fail_keys(&app_id, &ip_group);

        match msg {
            In::Hello { .. } => conns.reply(conn, re, "bad_message", "already said hello"),

            In::Create { public, name, player, max_players, meta } => {
                if bound.is_some() {
                    return conns.reply(conn, re, "already_in_room", "leave your current room first");
                }
                if public && !app.public_rooms {
                    return conns.reply(conn, re, "public_disabled", "public rooms are disabled for this app");
                }
                if app_rooms.len() >= app.max_rooms {
                    return conns.reply(conn, re, "too_many_rooms", "room limit reached for this app");
                }
                let meta = meta.unwrap_or(Value::Null);
                if meta.to_string().len() > MAX_META_BYTES {
                    return conns.reply(conn, re, "meta_too_large", "meta must be 1 KB or less");
                }
                let code = loop {
                    let candidate = random_code();
                    if !app_rooms.contains_key(&candidate) {
                        break candidate;
                    }
                };
                let host = random_id();
                let host_resume = random_token(24);
                let cap = app.max_players.max(2);
                let room = Room {
                    code: code.clone(),
                    key: random_token(12),
                    name: clean_name(name, "Room"),
                    public,
                    version,
                    max_players: max_players.unwrap_or(cap).clamp(2, cap),
                    locked: false,
                    meta,
                    host: host.clone(),
                    members: vec![Member {
                        id: host.clone(),
                        name: clean_name(player, "Host"),
                        resume: host_resume.clone(),
                        conn: Some(conn),
                        away_since: None,
                        ip_group: near.clone(),
                    }],
                    host_ip_group: near.clone(),
                    created: Instant::now(),
                    alone_since: Some(Instant::now()),
                };
                resume.insert(host_resume, (app_id.clone(), code.clone(), host.clone()));
                conns.bind(conn, &code, &host);
                conns.send(conn, &room.view(&host, false));
                info!(app = %app_id, room = %code, public, "room created");
                app_rooms.insert(code, room);
            }

            In::Join { code, key, player } => {
                let reject = |conns: &mut Conns, code: &str, message: &str| {
                    self.metrics.joins_rejected.fetch_add(1, Ordering::Relaxed);
                    conns.reply(conn, re, code, message);
                };
                if let Err((code, message)) = self.join_gate(&bound, rate, app_fails, ip, &fails) {
                    return reject(conns, code, message);
                }
                let code = code.trim().to_ascii_uppercase();
                let Some(room) = app_rooms.get_mut(&code) else {
                    app_fail(app_fails, &fails);
                    return reject(conns, "not_found", "no room with that code");
                };
                if room.version != version {
                    return reject(conns, "version_mismatch", "that room runs a different game version");
                }
                if !room.public && !key.as_deref().is_some_and(|k| ct_eq(k, &room.key)) {
                    app_fail(app_fails, &fails);
                    return reject(conns, "bad_key", "invalid room key");
                }
                if room.locked {
                    return reject(conns, "locked", "the host has locked this room");
                }
                if room.members.len() >= room.max_players as usize {
                    return reject(conns, "full", "room is full");
                }
                let peer = random_id();
                let peer_resume = random_token(24);
                let name = clean_name(player, "Player");
                room.members.push(Member {
                    id: peer.clone(),
                    name: name.clone(),
                    resume: peer_resume.clone(),
                    conn: Some(conn),
                    away_since: None,
                    ip_group: near.clone(),
                });
                let nearby = near == room.host_ip_group;
                room.alone_since = None;
                resume.insert(peer_resume, (app_id.clone(), code.clone(), peer.clone()));
                conns.bind(conn, &code, &peer);
                conns.send(conn, &room.view(&peer, false));
                room.broadcast(
                    conns,
                    &json!({ "t": "peer_joined", "peer": peer, "name": name, "nearby": nearby }),
                    Some(&peer),
                );
                info!(app = %app_id, room = %code, peer = %peer, "peer joined");
            }

            In::Resume { token } => {
                if bound.is_some() {
                    return conns.reply(conn, re, "already_in_room", "leave your current room first");
                }
                let Some((r_app, code, peer)) = resume.get(&token).cloned() else {
                    return conns.reply(conn, re, "not_found", "resume token expired");
                };
                if r_app != app_id {
                    return conns.reply(conn, re, "not_found", "resume token expired");
                }
                let Some(room) = app_rooms.get_mut(&code) else {
                    return conns.reply(conn, re, "not_found", "room no longer exists");
                };
                let Some(m) = room.member_mut(&peer) else {
                    return conns.reply(conn, re, "not_found", "no longer in that room");
                };
                if let Some(old) = m.conn.replace(conn) {
                    if old != conn {
                        conns.unbind(old);
                        conns.error(old, "replaced", "session resumed on another connection");
                    }
                }
                m.away_since = None;
                conns.bind(conn, &code, &peer);
                conns.send(conn, &room.view(&peer, true));
                if peer == room.host {
                    room.broadcast(conns, &json!({ "t": "host_back" }), Some(&peer));
                } else {
                    room.send_host(conns, &json!({ "t": "peer_back", "peer": peer }));
                }
            }

            In::Peek { code, key } => {
                let reject = |conns: &mut Conns, code: &str, message: &str| {
                    self.metrics.joins_rejected.fetch_add(1, Ordering::Relaxed);
                    conns.reply(conn, re, code, message);
                };
                if let Err((code, message)) = self.join_gate(&bound, rate, app_fails, ip, &fails) {
                    return reject(conns, code, message);
                }
                let code = code.trim().to_ascii_uppercase();
                let Some(room) = app_rooms.get(&code) else {
                    app_fail(app_fails, &fails);
                    return reject(conns, "not_found", "no room with that code");
                };
                if room.version != version {
                    return reject(conns, "version_mismatch", "that room runs a different game version");
                }
                if !room.public && !key.as_deref().is_some_and(|k| ct_eq(k, &room.key)) {
                    app_fail(app_fails, &fails);
                    return reject(conns, "bad_key", "invalid room key");
                }
                let players = room.members.len();
                conns.send(
                    conn,
                    &json!({
                        "t": "room_info",
                        "code": room.code,
                        "name": room.name,
                        "players": players,
                        "max_players": room.max_players,
                        "locked": room.locked,
                        "full": players >= room.max_players as usize,
                    }),
                );
            }

            In::List => {
                if app.list == ListMode::Hidden {
                    return conns.send(conn, &json!({ "t": "rooms", "rooms": [] }));
                }
                let mut listed: Vec<&Room> = app_rooms
                    .values()
                    .filter(|r| {
                        r.public
                            && r.version == version
                            && !r.locked
                            && r.members.len() < r.max_players as usize
                            && r.host_present()
                    })
                    .collect();
                listed.sort_by_key(|r| (r.host_ip_group != near, std::cmp::Reverse(r.created)));
                let rooms: Vec<Value> = listed
                    .into_iter()
                    .take(MAX_LISTED)
                    .map(|r| {
                        json!({
                            "code": r.code,
                            "name": r.name,
                            "players": r.members.len(),
                            "max_players": r.max_players,
                            "meta": r.meta,
                            "nearby": r.host_ip_group == near,
                        })
                    })
                    .collect();
                conns.send(conn, &json!({ "t": "rooms", "rooms": rooms }));
            }

            In::Signal { to, data } => {
                let Some((code, me)) = bound else {
                    return conns.reply(conn, re, "not_in_room", "join a room first");
                };
                let Some(room) = app_rooms.get(&code) else {
                    return conns.reply(conn, re, "not_in_room", "room no longer exists");
                };
                if me != room.host && to != room.host {
                    return conns.reply(conn, re, "bad_message", "peers may only signal the host");
                }
                let Some(target) = room.member(&to).and_then(|m| m.conn) else {
                    return conns.reply(conn, re, "peer_unavailable", "that peer is not connected");
                };
                conns.send(target, &json!({ "t": "signal", "from": me, "data": data }));
            }

            In::Lock { locked } => {
                if let Some(room) = host_room(app_rooms, &bound, conns, conn, re) {
                    room.locked = locked;
                    room.broadcast(conns, &room.meta_msg(), None);
                }
            }

            In::Meta { meta } => {
                if meta.to_string().len() > MAX_META_BYTES {
                    return conns.reply(conn, re, "meta_too_large", "meta must be 1 KB or less");
                }
                if let Some(room) = host_room(app_rooms, &bound, conns, conn, re) {
                    room.meta = meta;
                    room.broadcast(conns, &room.meta_msg(), None);
                }
            }

            In::Kick { peer } => {
                if let Some(room) = host_room(app_rooms, &bound, conns, conn, re) {
                    if peer == room.host {
                        return conns.reply(conn, re, "bad_message", "the host cannot kick themselves");
                    }
                    if room.member(&peer).is_none() {
                        return conns.reply(conn, re, "peer_unavailable", "no such peer");
                    }
                    remove_member(room, &peer, conns, resume, "kicked");
                }
            }

            In::Leave => {
                let Some((code, me)) = bound else {
                    return conns.reply(conn, re, "not_in_room", "not in a room");
                };
                let is_host = app_rooms.get(&code).is_some_and(|r| r.host == me);
                if is_host {
                    if let Some(room) = app_rooms.remove(&code) {
                        close_room(room, conns, resume, "host_left");
                    }
                } else if let Some(room) = app_rooms.get_mut(&code) {
                    remove_member(room, &me, conns, resume, "left");
                }
                conns.unbind(conn);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SessionReq {
    app: String,
    version: u32,
}

fn http_error(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({ "error": code }))).into_response()
}

async fn session(
    State(s): State<Arc<App>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<SessionReq>,
) -> Response {
    let ip = s.client_ip(&headers, addr);
    if !rate_allow(&mut s.inner.lock().unwrap().rate, ip, "session", s.cfg.limits.sessions_per_min) {
        return http_error(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let Some(app) = s.cfg.apps.get(&req.app) else {
        return http_error(StatusCode::NOT_FOUND, "not_found");
    };
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !s.origin_ok(app, origin) {
        return http_error(StatusCode::FORBIDDEN, "origin");
    }
    let (token, ttl) = s.mint_token(&req.app, req.version);
    s.metrics.sessions.fetch_add(1, Ordering::Relaxed);
    let turn = app.turn && s.cfg.turn.is_some() && s.turn_key.is_some();
    Json(json!({ "token": token, "expires_in": ttl, "turn": turn })).into_response()
}

async fn turn(State(s): State<Arc<App>>, headers: HeaderMap) -> Response {
    let claims = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .and_then(|t| s.verify_token(t));
    let Some(claims) = claims else {
        return http_error(StatusCode::UNAUTHORIZED, "bad_token");
    };
    if !s.cfg.apps.get(&claims.a).is_some_and(|a| a.turn) {
        return http_error(StatusCode::FORBIDDEN, "turn_disabled");
    }
    let (Some(tc), Some(key)) = (&s.cfg.turn, &s.turn_key) else {
        return http_error(StatusCode::SERVICE_UNAVAILABLE, "turn_unconfigured");
    };
    // coturn use-auth-secret: username "<expiry>:<anything>", password b64(HMAC-SHA1(secret, username)).
    let username = format!("{}:{}", now_unix() + tc.ttl_secs, claims.a);
    let credential = turn::auth::password(key, &username);
    s.metrics.turn.fetch_add(1, Ordering::Relaxed);
    Json(json!({
        "ice_servers": [{ "urls": tc.urls, "username": username, "credential": credential }],
        "ttl": tc.ttl_secs,
    }))
    .into_response()
}

async fn metrics(State(s): State<Arc<App>>) -> String {
    let m = &s.metrics;
    let mut out = String::new();
    let counters = [
        ("handshake_sessions_total", "counter", m.sessions.load(Ordering::Relaxed)),
        ("handshake_turn_credentials_total", "counter", m.turn.load(Ordering::Relaxed)),
        ("handshake_joins_rejected_total", "counter", m.joins_rejected.load(Ordering::Relaxed)),
        ("handshake_sockets", "gauge", m.sockets.load(Ordering::Relaxed)),
    ];
    for (name, kind, value) in counters {
        let _ = writeln!(out, "# TYPE {name} {kind}\n{name} {value}");
    }
    if s.cfg.turn.as_ref().is_some_and(|t| t.builtin()) {
        let t = &s.turn_stats;
        let turn = [
            ("handshake_turn_allocations", "gauge", t.allocations.load(Ordering::Relaxed)),
            ("handshake_turn_allocations_total", "counter", t.allocations_total.load(Ordering::Relaxed)),
            ("handshake_turn_auth_failures_total", "counter", t.auth_failures.load(Ordering::Relaxed)),
            ("handshake_turn_quota_rejections_total", "counter", t.quota_rejections.load(Ordering::Relaxed)),
        ];
        for (name, kind, value) in turn {
            let _ = writeln!(out, "# TYPE {name} {kind}\n{name} {value}");
        }
        let _ = writeln!(out, "# TYPE handshake_turn_relayed_bytes_total counter");
        let _ = writeln!(out, "handshake_turn_relayed_bytes_total{{direction=\"in\"}} {}", t.bytes_in.load(Ordering::Relaxed));
        let _ = writeln!(out, "handshake_turn_relayed_bytes_total{{direction=\"out\"}} {}", t.bytes_out.load(Ordering::Relaxed));
    }
    let inner = s.inner.lock().unwrap();
    let _ = writeln!(out, "# TYPE handshake_rooms gauge");
    for (app, rooms) in &inner.rooms {
        let _ = writeln!(out, "handshake_rooms{{app=\"{app}\"}} {}", rooms.len());
    }
    let _ = writeln!(out, "# TYPE handshake_players gauge");
    for (app, rooms) in &inner.rooms {
        let players: usize = rooms.values().map(|r| r.members.len()).sum();
        let _ = writeln!(out, "handshake_players{{app=\"{app}\"}} {players}");
    }
    out
}

async fn ws(
    State(s): State<Arc<App>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if !s.any_origin_ok(&origin) {
        return http_error(StatusCode::FORBIDDEN, "origin");
    }
    let ip = s.client_ip(&headers, addr);
    upgrade
        .max_message_size(MAX_WS_MESSAGE)
        .max_frame_size(MAX_WS_MESSAGE)
        .on_upgrade(move |socket| run_socket(s, socket, ip, origin))
}

async fn next_text(stream: &mut SplitStream<WebSocket>) -> Option<String> {
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(t) => return Some(t),
            Message::Close(_) => return None,
            _ => {}
        }
    }
    None
}

async fn run_socket(s: Arc<App>, socket: WebSocket, ip: IpAddr, origin: String) {
    let (mut sink, mut stream) = socket.split();

    // 1. hello, within the deadline
    let reject = |code: &str, message: &str| {
        json!({ "t": "error", "code": code, "message": message }).to_string()
    };
    let Ok(Some(text)) = tokio::time::timeout(HELLO_DEADLINE, next_text(&mut stream)).await else {
        let _ = sink.close().await;
        return;
    };
    let claims = match serde_json::from_str::<In>(&text) {
        Ok(In::Hello { v, token }) if v == SIGNAL_VERSION => s.verify_token(&token),
        Ok(In::Hello { .. }) => {
            let _ = sink.send(Message::Text(reject("bad_message", "unsupported signaling version"))).await;
            return;
        }
        _ => None,
    };
    let Some(claims) = claims else {
        let _ = sink.send(Message::Text(reject("bad_token", "invalid or expired session token"))).await;
        return;
    };
    if !s.cfg.apps.get(&claims.a).is_some_and(|a| s.origin_ok(a, &origin)) {
        let _ = sink.send(Message::Text(reject("origin", "origin not allowed for this app"))).await;
        return;
    }

    // 2. register
    let (tx, mut rx) = mpsc::channel::<String>(OUTBOUND_QUEUE);
    let conn = s.next_conn.fetch_add(1, Ordering::Relaxed);
    {
        let mut inner = s.inner.lock().unwrap();
        inner.conns.map.insert(
            conn,
            Conn { tx, app: claims.a.clone(), version: claims.v, ip, ip_group: ip_group(ip), room: None },
        );
        inner.conns.send(conn, &json!({ "t": "welcome", "v": SIGNAL_VERSION }));
    }
    s.metrics.sockets.fetch_add(1, Ordering::Relaxed);

    // 3. writer: drains the queue and pings; ends when the Conn (and its tx) is dropped
    tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(text) => if sink.send(Message::Text(text)).await.is_err() { break },
                    None => break,
                },
                _ = ping.tick() => if sink.send(Message::Ping(Vec::new())).await.is_err() { break },
            }
        }
        let _ = sink.close().await;
    });

    // 4. reader
    loop {
        let msg = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
            Ok(Some(Ok(msg))) => msg,
            _ => break,
        };
        match msg {
            Message::Text(text) => {
                let mut inner = s.inner.lock().unwrap();
                match serde_json::from_str::<In>(&text) {
                    Ok(m) => s.handle(&mut inner, conn, m),
                    Err(_) => match serde_json::from_str::<Value>(&text).ok().as_ref().and_then(|v| v["t"].as_str()) {
                        Some(re) => inner.conns.reply(conn, re, "bad_message", "unrecognized message"),
                        None => inner.conns.error(conn, "bad_message", "unrecognized message"),
                    },
                }
                s.reap_dead(&mut inner);
                if !inner.conns.map.contains_key(&conn) {
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {} // pings, pongs, binary: keep-alive only
        }
    }

    let mut inner = s.inner.lock().unwrap();
    s.disconnect(&mut inner, conn);
    s.reap_dead(&mut inner);
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256>>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The client address for rate limits and `nearby`. `X-Forwarded-For` counts only when the peer is a
/// trusted proxy. Proxies append, so the client is then the last entry that is not itself a trusted proxy.
fn forwarded_client(trusted: &[Cidr], xff: Option<&str>, peer: IpAddr) -> IpAddr {
    let is_trusted = |ip: IpAddr| trusted.iter().any(|c| c.contains(ip));
    if !is_trusted(peer) {
        return peer;
    }
    let mut client = peer;
    for entry in xff.into_iter().flat_map(|v| v.rsplit(',')) {
        let Ok(ip) = entry.trim().parse::<IpAddr>() else {
            break; // anything left of an unreadable entry is unverifiable
        };
        client = ip;
        if !is_trusted(ip) {
            break;
        }
    }
    client
}

fn rate_allow(
    rate: &mut RateMap,
    ip: IpAddr,
    bucket: &'static str,
    per_min: u32,
) -> bool {
    let now = Instant::now();
    let entry = rate.entry((ip, bucket)).or_insert((now, 0));
    if now - entry.0 >= Duration::from_secs(60) {
        *entry = (now, 0);
    }
    entry.1 += 1;
    entry.1 <= per_min
}

/// True while `key` (see `fail_keys`) has used up its failed-join budget for the current minute.
fn app_fails_over(fails: &mut FailMap, key: &str, per_min: u32) -> bool {
    let now = Instant::now();
    let entry = fails.entry(key.to_string()).or_insert((now, 0));
    if now - entry.0 >= Duration::from_secs(60) {
        *entry = (now, 0);
    }
    entry.1 >= per_min
}

/// The failed-join counters a join or peek from `ip_group` counts against: the app's, then the IP's within the app.
fn fail_keys(app: &str, ip_group: &str) -> [String; 2] {
    [app.to_string(), format!("{app} {ip_group}")]
}

/// Count one failed join or peek against the app and the IP.
fn app_fail(fails: &mut FailMap, keys: &[String; 2]) {
    for key in keys {
        fails.entry(key.clone()).or_insert((Instant::now(), 0)).1 += 1;
    }
}

/// IPv4 exact; IPv6 by /64, since every device on a v6 LAN has its own address.
/// The network a client is on, for `nearby`. A private IPv4 address (10/8, 172.16/12, 192.168/16) groups by its /24:
/// when the server is on the players' own LAN (split DNS), each device arrives with its own private address, and devices on
/// one home network must still be nearby. Any other address groups as `ip_group` does (exact IPv4, IPv6 /64); shared
/// carrier NAT (100.64/10) is not a home network, so it stays exact.
fn net_group(ip: IpAddr) -> String {
    let v4 = match ip { IpAddr::V4(v4) => Some(v4), IpAddr::V6(v6) => v6.to_ipv4_mapped() };
    match v4 {
        Some(v4) if v4.is_private() => { let o = v4.octets(); format!("{}.{}.{}.0/24", o[0], o[1], o[2]) }
        _ => ip_group(ip),
    }
}

fn ip_group(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        },
    }
}

fn is_localhost(origin: &str) -> bool {
    ["http://localhost", "http://127.0.0.1"]
        .iter()
        .any(|p| origin == *p || origin.strip_prefix(p).is_some_and(|rest| rest.starts_with(':')))
}

fn clean_name(name: Option<String>, fallback: &str) -> String {
    let cleaned: String = name
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect::<String>()
        .trim()
        .to_string();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    }
}

fn random_code() -> String {
    let mut rng = rand::thread_rng();
    (0..CODE_LEN).map(|_| CODE_ALPHABET[rng.gen_range(0..CODE_ALPHABET.len())] as char).collect()
}

fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

fn random_id() -> String {
    random_token(6)
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Janitor: grace periods, room expiry, rate-limit cleanup.
pub fn spawn_sweeper(state: Arc<App>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_EVERY);
        loop {
            tick.tick().await;
            let mut inner = state.inner.lock().unwrap();
            state.sweep(&mut inner);
        }
    })
}

/// All routes. Serve with `into_make_service_with_connect_info::<SocketAddr>()`.
pub fn router(state: Arc<App>) -> Router {
    let cors_state = state.clone();
    let cors = CorsLayer::new()
        .allow_methods([Method::POST])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
        .allow_origin(AllowOrigin::predicate(move |origin: &HeaderValue, _| {
            origin.to_str().is_ok_and(|o| cors_state.any_origin_ok(o))
        }))
        .max_age(Duration::from_secs(600));

    let api = Router::new().route("/session", post(session)).route("/turn", post(turn)).layer(cors);
    Router::new()
        .route("/ws", get(ws))
        .route("/healthz", get(|| async { "ok" }))
        .route("/metrics", get(metrics))
        .merge(api)
        .with_state(state)
}

/// Run the sweeper and serve on `listener` until `shutdown` resolves.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: Arc<App>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let sweeper = spawn_sweeper(state.clone());
    let app = router(state);
    let result = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown)
        .await;
    sweeper.abort();
    result
}

// ---------------------------------------------------------------------------
// Unit tests (pure helpers). Socket-level tests live in tests/server.rs.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"test-session-secret-0123456789abcdef";
    const PREV_KEY: &[u8] = b"previous-session-secret-0123456789ab";

    fn app_with(session_keys: Vec<Vec<u8>>, ttl: u64) -> App {
        let cfg: Config = toml::from_str(&format!(
            r#"
            [limits]
            session_ttl_secs = {ttl}

            [apps.game]
            origins = ["https://game.example"]
            "#
        ))
        .unwrap();
        App::new(cfg, session_keys, None)
    }

    /// A token signed with `key` for arbitrary claims, built independently of `mint_token`.
    fn forge(key: &[u8], claims: Value) -> String {
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        let sig = hmac_sha256(key, payload.as_bytes());
        format!("{payload}.{}", URL_SAFE_NO_PAD.encode(sig))
    }

    #[test]
    fn example_config_parses() {
        let cfg: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        let tp = &cfg.apps["tractor-pickup"];
        assert_eq!(tp.origins, ["https://four43.com"]);
        assert_eq!((tp.max_players, tp.max_rooms, tp.public_rooms, tp.turn), (4, 20, true, true));
        assert_eq!(tp.list, ListMode::Hidden);
        assert_eq!(cfg.apps["pig-pens"].list, ListMode::All);
        assert_eq!(cfg.limits.grace_secs, 30);
        let turn = cfg.turn.unwrap();
        assert!(turn.builtin());
        assert_eq!((turn.relay_ports, turn.external_ip.as_deref()), (Some(PortRange { first: 49160, last: 49200 }), Some("203.0.113.10")));
    }

    // ---- session tokens -------------------------------------------------

    #[test]
    fn token_round_trip() {
        let app = app_with(vec![KEY.to_vec()], 900);
        let (token, ttl) = app.mint_token("game", 3);
        assert_eq!(ttl, 900);
        let claims = app.verify_token(&token).expect("fresh token verifies");
        assert_eq!((claims.a.as_str(), claims.v), ("game", 3));
        let e = claims.e;
        assert!(e > now_unix() + 890 && e <= now_unix() + 900, "expiry is now + ttl");
    }

    #[test]
    fn token_expired_is_rejected() {
        // ttl 0: expiry == now, and verification requires expiry > now.
        let app = app_with(vec![KEY.to_vec()], 0);
        let (token, _) = app.mint_token("game", 1);
        assert!(app.verify_token(&token).is_none());

        let past = forge(KEY, json!({ "a": "game", "v": 1, "e": now_unix() - 10 }));
        assert!(app.verify_token(&past).is_none());
        let future = forge(KEY, json!({ "a": "game", "v": 1, "e": now_unix() + 60 }));
        assert!(app.verify_token(&future).is_some());
    }

    #[test]
    fn token_unknown_app_is_rejected() {
        let app = app_with(vec![KEY.to_vec()], 900);
        let token = forge(KEY, json!({ "a": "other", "v": 1, "e": now_unix() + 60 }));
        assert!(app.verify_token(&token).is_none());
    }

    #[test]
    fn token_rotation_accepts_previous_secret() {
        let old = app_with(vec![PREV_KEY.to_vec()], 900);
        let (old_token, _) = old.mint_token("game", 1);

        let rotated = app_with(vec![KEY.to_vec(), PREV_KEY.to_vec()], 900);
        assert!(rotated.verify_token(&old_token).is_some(), "previous secret still verifies");
        let (new_token, _) = rotated.mint_token("game", 1);
        assert!(old.verify_token(&new_token).is_none(), "new tokens are signed with the current secret");
        assert!(app_with(vec![PREV_KEY.to_vec()], 900).verify_token(&old_token).is_some());

        let without_prev = app_with(vec![KEY.to_vec()], 900);
        assert!(without_prev.verify_token(&old_token).is_none(), "dropped secret no longer verifies");
    }

    #[test]
    fn token_tampering_is_rejected() {
        let app = app_with(vec![KEY.to_vec()], 900);
        let (token, _) = app.mint_token("game", 1);
        let (payload, sig) = token.split_once('.').unwrap();

        // Swap in different claims under the original signature.
        let other = URL_SAFE_NO_PAD.encode(json!({ "a": "game", "v": 2, "e": now_unix() + 900 }).to_string());
        assert!(app.verify_token(&format!("{other}.{sig}")).is_none());

        // Flip one signature character.
        let mut bad_sig: Vec<char> = sig.chars().collect();
        bad_sig[0] = if bad_sig[0] == 'A' { 'B' } else { 'A' };
        let bad_sig: String = bad_sig.into_iter().collect();
        assert!(app.verify_token(&format!("{payload}.{bad_sig}")).is_none());

        // Signed with an unrelated key, malformed shapes.
        let foreign = forge(b"someone-elses-secret-0123456789abcd", json!({ "a": "game", "v": 1, "e": now_unix() + 60 }));
        assert!(app.verify_token(&foreign).is_none());
        for junk in ["", ".", "nodot", &format!("{payload}."), &format!(".{sig}"), "!!!.???"] {
            assert!(app.verify_token(junk).is_none(), "{junk:?}");
        }
    }

    // ---- codes, names, ids ----------------------------------------------

    #[test]
    fn join_codes_use_unambiguous_alphabet() {
        for c in "0O1IL".chars() {
            assert!(!CODE_ALPHABET.contains(&(c as u8)), "{c} is ambiguous");
        }
        for _ in 0..1000 {
            let code = random_code();
            assert_eq!(code.len(), CODE_LEN);
            assert!(code.bytes().all(|b| CODE_ALPHABET.contains(&b)), "{code}");
        }
    }

    #[test]
    fn clean_name_caps_and_falls_back() {
        assert_eq!(clean_name(Some("x".repeat(40)), "F"), "x".repeat(32));
        // The cap counts characters, not bytes.
        assert_eq!(clean_name(Some("é".repeat(40)), "F").chars().count(), 32);
        assert_eq!(clean_name(Some("  Seth  ".into()), "F"), "Seth");
        assert_eq!(clean_name(Some("a\u{0}b\nc".into()), "F"), "abc");
        assert_eq!(clean_name(Some("   ".into()), "F"), "F");
        assert_eq!(clean_name(None, "Host"), "Host");
    }

    #[test]
    fn random_tokens_are_url_safe() {
        let t = random_token(24);
        assert_eq!(t.len(), 32);
        assert!(t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_ne!(random_token(24), random_token(24));
    }

    // ---- network helpers ------------------------------------------------

    #[test]
    fn net_group_private_v4_by_24_else_like_ip_group() {
        let n = |s: &str| net_group(s.parse().unwrap());
        assert_eq!(n("192.168.1.20"), n("192.168.1.30"));
        assert_eq!(n("192.168.1.20"), "192.168.1.0/24");
        assert_ne!(n("192.168.1.20"), n("192.168.2.20"));
        assert_eq!(n("10.0.5.1"), n("10.0.5.200"));
        assert_eq!(n("172.16.4.9"), n("172.16.4.10"));
        assert_eq!(n("::ffff:192.168.1.7"), "192.168.1.0/24");
        assert_ne!(n("100.64.0.1"), n("100.64.0.2")); // carrier NAT is not one home
        assert_ne!(n("203.0.113.5"), n("203.0.113.6"));
        assert_eq!(n("2001:db8:1:2::10"), "2001:db8:1:2::/64");
    }

    #[test]
    fn ip_group_v4_exact_v6_by_64() {
        let g = |s: &str| ip_group(s.parse().unwrap());
        assert_eq!(g("203.0.113.5"), "203.0.113.5");
        assert_ne!(g("203.0.113.5"), g("203.0.113.6"));
        assert_eq!(g("2001:db8:1:2::10"), g("2001:db8:1:2:ffff::99"));
        assert_ne!(g("2001:db8:1:2::10"), g("2001:db8:1:3::10"));
        assert_eq!(g("2001:db8:1:2::10"), "2001:db8:1:2::/64");
        // IPv4-mapped IPv6 groups with the plain IPv4 address.
        assert_eq!(g("::ffff:203.0.113.5"), "203.0.113.5");
    }

    #[test]
    fn localhost_origins() {
        for ok in ["http://localhost", "http://localhost:5173", "http://127.0.0.1", "http://127.0.0.1:8080"] {
            assert!(is_localhost(ok), "{ok}");
        }
        for bad in [
            "https://localhost",
            "http://localhost.evil.com",
            "http://127.0.0.1.evil.com",
            "http://localhostx:80",
            "http://evil.com/http://localhost",
            "",
        ] {
            assert!(!is_localhost(bad), "{bad}");
        }
    }

    #[test]
    fn rate_limit_per_ip_and_bucket() {
        let mut rate = HashMap::new();
        let a: IpAddr = "203.0.113.5".parse().unwrap();
        let b: IpAddr = "203.0.113.6".parse().unwrap();
        for _ in 0..3 {
            assert!(rate_allow(&mut rate, a, "join", 3));
        }
        assert!(!rate_allow(&mut rate, a, "join", 3));
        assert!(rate_allow(&mut rate, a, "session", 3), "buckets are independent");
        assert!(rate_allow(&mut rate, b, "join", 3), "IPs are independent");

        // A window older than a minute resets.
        rate.insert((a, "join"), (Instant::now() - Duration::from_secs(61), 99));
        assert!(rate_allow(&mut rate, a, "join", 3));
        assert_eq!(rate[&(a, "join")].1, 1);
    }

    #[test]
    fn port_range_parses() {
        assert_eq!("49160-49200".parse(), Ok(PortRange { first: 49160, last: 49200 }));
        assert_eq!("5-5".parse(), Ok(PortRange { first: 5, last: 5 }));
        for bad in ["", "5", "2-1", "0-5", "a-b", "1-70000", "1-2-3"] {
            assert!(bad.parse::<PortRange>().is_err(), "{bad}");
        }
    }

    #[test]
    fn turn_config_defaults() {
        let tc: TurnConfig = toml::from_str(r#"urls = []"#).unwrap();
        assert!(!tc.builtin());
        assert_eq!((tc.listen.as_str(), tc.ttl_secs, tc.max_allocations, tc.allocations_per_ip, tc.kbps_per_allocation), ("0.0.0.0:3478", 3600, 500, 64, 2000));
        let tc: TurnConfig = toml::from_str("urls = []\nrelay_ports = \"49160-49200\"\nexternal_ip = \"203.0.113.5\"").unwrap();
        assert!(tc.builtin());
        assert!(toml::from_str::<TurnConfig>("urls = []\nrelay_ports = \"9-1\"").is_err());
    }

    #[test]
    fn cidr_parses_and_matches() {
        let c = |s: &str| s.parse::<Cidr>().unwrap();
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(c("10.0.0.0/8").contains(ip("10.1.2.3")));
        assert!(!c("10.0.0.0/8").contains(ip("11.0.0.1")));
        assert!(c("172.16.0.0/12").contains(ip("172.31.255.1")));
        assert!(!c("172.16.0.0/12").contains(ip("172.32.0.1")));
        assert!(c("192.0.2.1").contains(ip("192.0.2.1")), "a bare address is a /32");
        assert!(!c("192.0.2.1").contains(ip("192.0.2.2")));
        assert!(c("::1/128").contains(ip("::1")));
        assert!(c("fc00::/7").contains(ip("fd12:3456::1")));
        assert!(!c("fc00::/7").contains(ip("2001:db8::1")));
        assert!(c("0.0.0.0/0").contains(ip("203.0.113.5")));
        // Dual-stack sockets report IPv4 peers as IPv4-mapped IPv6.
        assert!(c("10.0.0.0/8").contains(ip("::ffff:10.0.0.1")));
        assert!(!c("10.0.0.0/8").contains(ip("2001:db8::1")), "families do not mix");
        for bad in ["nope", "10.0.0.0/33", "::/129", "10.0.0.0/", "/8", "10.0.0.0/x"] {
            assert!(bad.parse::<Cidr>().is_err(), "{bad}");
        }
    }

    #[test]
    fn trusted_proxies_default_and_validate() {
        let cfg: Config = toml::from_str("").unwrap();
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let trusted = |s: &str| cfg.trusted_proxies.iter().any(|c| c.contains(ip(s)));
        for t in ["127.0.0.1", "::1", "10.0.0.2", "172.17.0.1", "192.168.1.1", "fd00::1"] {
            assert!(trusted(t), "{t}");
        }
        for u in ["203.0.113.5", "8.8.8.8", "2001:db8::1", "100.64.0.1"] {
            assert!(!trusted(u), "{u}");
        }
        assert!(toml::from_str::<Config>("trusted_proxies = [\"nope\"]").is_err());
    }

    #[test]
    fn forwarded_client_from_trusted_peers_only() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let private: Vec<Cidr> = ["127.0.0.0/8", "10.0.0.0/8"].iter().map(|s| s.parse().unwrap()).collect();
        let f = |trusted: &[Cidr], xff: Option<&str>, peer: &str| forwarded_client(trusted, xff, ip(peer));

        // An untrusted peer's header is ignored.
        assert_eq!(f(&private, Some("198.51.100.1"), "203.0.113.5"), ip("203.0.113.5"));
        assert_eq!(f(&[], Some("198.51.100.1"), "127.0.0.1"), ip("127.0.0.1"));
        // A trusted peer: the last entry that is not itself a trusted proxy.
        assert_eq!(f(&private, Some("198.51.100.1"), "10.0.0.2"), ip("198.51.100.1"));
        assert_eq!(f(&private, Some("6.6.6.6, 198.51.100.1"), "10.0.0.2"), ip("198.51.100.1"));
        assert_eq!(f(&private, Some("198.51.100.1, 10.0.0.9"), "10.0.0.2"), ip("198.51.100.1"));
        // Every hop trusted: the first one (a client on the proxy's own network).
        assert_eq!(f(&private, Some("10.0.0.7, 10.0.0.9"), "10.0.0.2"), ip("10.0.0.7"));
        // No header, or one we cannot read: the peer.
        assert_eq!(f(&private, None, "10.0.0.2"), ip("10.0.0.2"));
        assert_eq!(f(&private, Some("garbage"), "10.0.0.2"), ip("10.0.0.2"));
        // Stop at an unreadable entry rather than trust what is left of it.
        assert_eq!(f(&private, Some("6.6.6.6, garbage, 10.0.0.9"), "10.0.0.2"), ip("10.0.0.9"));
    }

    #[test]
    fn ct_eq_compares_whole_strings() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "ab"));
        assert!(ct_eq("", ""));
    }

    /// The docs site renders `site/schemas/*.json`. Regenerate with
    /// `UPDATE_SCHEMAS=1 scripts/test.sh schemas_are_current`.
    #[test]
    fn schemas_are_current() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("site/schemas");
        let schemas = [
            ("config.json", schemars::schema_for!(Config)),
            ("client-messages.json", schemars::schema_for!(In)),
        ];
        let update = std::env::var_os("UPDATE_SCHEMAS").is_some();
        for (name, schema) in schemas {
            let path = dir.join(name);
            let json = serde_json::to_string_pretty(&schema).unwrap() + "\n";
            if update {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(&path, &json).unwrap();
            } else {
                let committed = std::fs::read_to_string(&path).unwrap_or_default();
                assert!(committed == json, "{} is stale: run UPDATE_SCHEMAS=1 scripts/test.sh schemas_are_current", path.display());
            }
        }
    }
}
