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
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use futures_util::{stream::SplitStream, SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use rand::{Rng, RngCore};
use serde::Deserialize;
use serde_json::{json, Value};
use sha1::Sha1;
use sha2::Sha256;
use tokio::sync::mpsc;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::{info, warn};

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

#[derive(Deserialize, Clone)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default)]
    pub trust_proxy: bool,
    #[serde(default)]
    pub allow_localhost: bool,
    #[serde(default)]
    pub limits: Limits,
    pub turn: Option<TurnConfig>,
    #[serde(default)]
    pub apps: HashMap<String, AppConfig>,
}

#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct Limits {
    pub session_ttl_secs: u64,
    pub grace_secs: u64,
    pub room_max_age_secs: u64,
    pub idle_room_secs: u64,
    pub sessions_per_min: u32,
    pub joins_per_min: u32,
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
        }
    }
}

#[derive(Deserialize, Clone)]
pub struct TurnConfig {
    pub urls: Vec<String>,
    #[serde(default = "default_turn_ttl")]
    pub ttl_secs: u64,
}

/// What `list` returns for an app.
#[derive(Deserialize, Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum ListMode {
    /// Public rooms for the caller's version, nearby first (SPEC.md "Public listing").
    #[default]
    #[serde(rename = "all")]
    All,
    /// Nothing: a public room is joinable with its code, but its code is never listed.
    #[serde(rename = "none")]
    Hidden,
}

#[derive(Deserialize, Clone)]
pub struct AppConfig {
    pub origins: Vec<String>,
    #[serde(default = "default_max_players")]
    pub max_players: u8,
    #[serde(default = "default_max_rooms")]
    pub max_rooms: usize,
    #[serde(default)]
    pub public_rooms: bool,
    #[serde(default)]
    pub list: ListMode,
    #[serde(default)]
    pub turn: bool,
}

pub fn default_listen() -> String {
    "0.0.0.0:8080".into()
}
fn default_turn_ttl() -> u64 {
    3600
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
    next_conn: AtomicU64,
}

#[derive(Default)]
struct Metrics {
    sessions: AtomicU64,
    turn: AtomicU64,
    joins_rejected: AtomicU64,
    sockets: AtomicU64,
}

#[derive(Default)]
struct Inner {
    conns: Conns,
    rooms: HashMap<String, HashMap<String, Room>>, // app id -> code -> room
    resume: HashMap<String, (String, String, PeerId)>, // token -> (app, code, peer)
    rate: HashMap<(IpAddr, &'static str), (Instant, u32)>,
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

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum In {
    Hello {
        #[allow(dead_code)]
        v: u32,
        #[allow(dead_code)]
        token: String,
    },
    Create {
        #[serde(default)]
        public: bool,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        player: Option<String>,
        #[serde(default)]
        max_players: Option<u8>,
        #[serde(default)]
        meta: Option<Value>,
    },
    Join {
        code: String,
        #[serde(default)]
        key: Option<String>,
        #[serde(default)]
        player: Option<String>,
    },
    Resume {
        token: String,
    },
    List,
    Signal {
        to: PeerId,
        data: Value,
    },
    Lock {
        locked: bool,
    },
    Meta {
        meta: Value,
    },
    Kick {
        peer: PeerId,
    },
    Leave,
}

// ---------------------------------------------------------------------------
// Conns / Room helpers
// ---------------------------------------------------------------------------

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
            .map(|m| json!({ "id": m.id, "name": m.name, "away": m.away_since.is_some() }))
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
) -> Option<&'r mut Room> {
    let Some((code, me)) = bound else {
        conns.error(conn, "not_in_room", "join a room first");
        return None;
    };
    let Some(room) = app_rooms.get_mut(code) else {
        conns.error(conn, "not_in_room", "room no longer exists");
        return None;
    };
    if &room.host != me {
        conns.error(conn, "not_host", "only the host can do that");
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
            next_conn: AtomicU64::new(1),
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
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
        if self.cfg.trust_proxy {
            // Caddy appends the real client address last.
            let forwarded = headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit(',').next())
                .and_then(|v| v.trim().parse().ok());
            if let Some(ip) = forwarded {
                return ip;
            }
        }
        addr.ip()
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
        let Inner { conns, rooms, resume, rate } = inner;

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
        self.reap_dead(inner);
    }

    // ---- message handling ----------------------------------------------

    fn handle(&self, inner: &mut Inner, conn: ConnId, msg: In) {
        let Inner { conns, rooms, resume, rate } = inner;
        let Some(c) = conns.map.get(&conn) else {
            return;
        };
        let (app_id, version, ip, ip_group, bound) =
            (c.app.clone(), c.version, c.ip, c.ip_group.clone(), c.room.clone());
        let Some(app) = self.cfg.apps.get(&app_id) else {
            return;
        };
        let app_rooms = rooms.entry(app_id.clone()).or_default();

        match msg {
            In::Hello { .. } => conns.error(conn, "bad_message", "already said hello"),

            In::Create { public, name, player, max_players, meta } => {
                if bound.is_some() {
                    return conns.error(conn, "already_in_room", "leave your current room first");
                }
                if public && !app.public_rooms {
                    return conns.error(conn, "public_disabled", "public rooms are disabled for this app");
                }
                if app_rooms.len() >= app.max_rooms {
                    return conns.error(conn, "too_many_rooms", "room limit reached for this app");
                }
                let meta = meta.unwrap_or(Value::Null);
                if meta.to_string().len() > MAX_META_BYTES {
                    return conns.error(conn, "meta_too_large", "meta must be 1 KB or less");
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
                    }],
                    host_ip_group: ip_group,
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
                    conns.error(conn, code, message);
                };
                if bound.is_some() {
                    return reject(conns, "already_in_room", "leave your current room first");
                }
                if !rate_allow(rate, ip, "join", self.cfg.limits.joins_per_min) {
                    return reject(conns, "rate_limited", "too many join attempts; wait a minute");
                }
                let code = code.trim().to_ascii_uppercase();
                let Some(room) = app_rooms.get_mut(&code) else {
                    return reject(conns, "not_found", "no room with that code");
                };
                if room.version != version {
                    return reject(conns, "version_mismatch", "that room runs a different game version");
                }
                if !room.public && !key.as_deref().is_some_and(|k| ct_eq(k, &room.key)) {
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
                });
                room.alone_since = None;
                resume.insert(peer_resume, (app_id.clone(), code.clone(), peer.clone()));
                conns.bind(conn, &code, &peer);
                conns.send(conn, &room.view(&peer, false));
                room.broadcast(conns, &json!({ "t": "peer_joined", "peer": peer, "name": name }), Some(&peer));
                info!(app = %app_id, room = %code, peer = %peer, "peer joined");
            }

            In::Resume { token } => {
                if bound.is_some() {
                    return conns.error(conn, "already_in_room", "leave your current room first");
                }
                let Some((r_app, code, peer)) = resume.get(&token).cloned() else {
                    return conns.error(conn, "not_found", "resume token expired");
                };
                if r_app != app_id {
                    return conns.error(conn, "not_found", "resume token expired");
                }
                let Some(room) = app_rooms.get_mut(&code) else {
                    return conns.error(conn, "not_found", "room no longer exists");
                };
                let Some(m) = room.member_mut(&peer) else {
                    return conns.error(conn, "not_found", "no longer in that room");
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
                listed.sort_by_key(|r| (r.host_ip_group != ip_group, std::cmp::Reverse(r.created)));
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
                            "nearby": r.host_ip_group == ip_group,
                        })
                    })
                    .collect();
                conns.send(conn, &json!({ "t": "rooms", "rooms": rooms }));
            }

            In::Signal { to, data } => {
                let Some((code, me)) = bound else {
                    return conns.error(conn, "not_in_room", "join a room first");
                };
                let Some(room) = app_rooms.get(&code) else {
                    return conns.error(conn, "not_in_room", "room no longer exists");
                };
                if me != room.host && to != room.host {
                    return conns.error(conn, "bad_message", "peers may only signal the host");
                }
                let Some(target) = room.member(&to).and_then(|m| m.conn) else {
                    return conns.error(conn, "peer_unavailable", "that peer is not connected");
                };
                conns.send(target, &json!({ "t": "signal", "from": me, "data": data }));
            }

            In::Lock { locked } => {
                if let Some(room) = host_room(app_rooms, &bound, conns, conn) {
                    room.locked = locked;
                    room.broadcast(conns, &room.meta_msg(), None);
                }
            }

            In::Meta { meta } => {
                if meta.to_string().len() > MAX_META_BYTES {
                    return conns.error(conn, "meta_too_large", "meta must be 1 KB or less");
                }
                if let Some(room) = host_room(app_rooms, &bound, conns, conn) {
                    room.meta = meta;
                    room.broadcast(conns, &room.meta_msg(), None);
                }
            }

            In::Kick { peer } => {
                if let Some(room) = host_room(app_rooms, &bound, conns, conn) {
                    if peer == room.host {
                        return conns.error(conn, "bad_message", "the host cannot kick themselves");
                    }
                    if room.member(&peer).is_none() {
                        return conns.error(conn, "peer_unavailable", "no such peer");
                    }
                    remove_member(room, &peer, conns, resume, "kicked");
                }
            }

            In::Leave => {
                let Some((code, me)) = bound else {
                    return conns.error(conn, "not_in_room", "not in a room");
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
    let credential = turn_credential(key, &username);
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
                    Err(_) => inner.conns.error(conn, "bad_message", "unrecognized message"),
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

/// coturn use-auth-secret password: base64(HMAC-SHA1(secret, username)).
fn turn_credential(key: &[u8], username: &str) -> String {
    let mut mac = <Hmac<Sha1>>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(username.as_bytes());
    STANDARD.encode(mac.finalize().into_bytes())
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn rate_allow(
    rate: &mut HashMap<(IpAddr, &'static str), (Instant, u32)>,
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

/// IPv4 exact; IPv6 by /64, since every device on a v6 LAN has its own address.
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

    // ---- TURN -----------------------------------------------------------

    #[test]
    fn turn_credential_matches_coturn_scheme() {
        // Known-answer HMAC-SHA1, computed independently:
        //   printf '1700000000:game' | openssl dgst -sha1 -hmac turn-secret -binary | base64
        assert_eq!(turn_credential(b"turn-secret", "1700000000:game"), "n12nWdEgEYR9Wpg+vbxX8n3V8VA=");
        // 20-byte SHA1 digest -> 28 base64 chars with padding.
        let cred = turn_credential(b"k", "1:a");
        assert_eq!(STANDARD.decode(&cred).unwrap().len(), 20);
        assert_ne!(cred, turn_credential(b"k", "2:a"));
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
    fn ct_eq_compares_whole_strings() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "ab"));
        assert!(ct_eq("", ""));
    }
}
