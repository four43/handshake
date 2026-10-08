//! Integration tests: run the server in-process on 127.0.0.1:0 and drive
//! hosts and joiners over real HTTP and WebSocket connections.
//!
//! Grace is 1 s, but the sweeper runs every 5 s (fixed), so timeouts can take
//! up to ~6 s to fire. `WAIT` leaves headroom for that.

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use handshake::{serve, App, Config};
use serde_json::{json, Value};
use sha1::Sha1;
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, http::HeaderValue, Error as WsError, Message},
    MaybeTlsStream, WebSocketStream,
};

const SESSION_SECRET: &str = "test-session-secret-0123456789abcdef";
const TURN_SECRET: &str = "test-turn-secret";
const WAIT: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_millis(300);

/// Every app's allowed origin is `https://<app>.test`.
/// Tests reach the server from 127.0.0.1, a trusted proxy by default, so they set the
/// client IP with X-Forwarded-For.
const BASE_CONFIG: &str = r#"

[limits]
grace_secs = 1
idle_room_secs = 3600
sessions_per_min = 1000
joins_per_min = 1000

[turn]
urls = ["stun:turn.test:3478", "turn:turn.test:3478?transport=udp"]
ttl_secs = 600

[apps.game]
origins = ["https://game.test"]
max_players = 3
max_rooms = 10
public_rooms = true
turn = true

[apps.private]
origins = ["https://private.test"]
max_players = 4
public_rooms = false
turn = false

[apps.tiny]
origins = ["https://tiny.test"]
max_rooms = 1
public_rooms = true
"#;

fn origin(app: &str) -> String {
    format!("https://{app}.test")
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Server {
    addr: SocketAddr,
    http: reqwest::Client,
}

async fn start() -> Server {
    start_with(BASE_CONFIG, Some(TURN_SECRET)).await
}

/// Start a server with one `[limits]` key overridden, e.g. `"grace_secs = 5"`.
async fn start_limits(line: &str) -> Server {
    let key = line.split('=').next().unwrap().trim();
    let config: Vec<&str> = BASE_CONFIG.lines().filter(|l| !l.starts_with(&format!("{key} ="))).collect();
    let config = config.join("\n").replace("[limits]", &format!("[limits]\n{line}"));
    start_with(&config, Some(TURN_SECRET)).await
}

async fn start_with(config: &str, turn_secret: Option<&str>) -> Server {
    let cfg: Config = toml::from_str(config).expect("test config parses");
    let state = Arc::new(App::new(
        cfg,
        vec![SESSION_SECRET.as_bytes().to_vec()],
        turn_secret.map(|s| s.as_bytes().to_vec()),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(listener, state, std::future::pending()));
    Server { addr, http: reqwest::Client::new() }
}

impl Server {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    async fn session(&self, app: &str, version: u32, origin: Option<&str>) -> reqwest::Response {
        let mut req = self.http.post(self.url("/session")).json(&json!({ "app": app, "version": version }));
        if let Some(o) = origin {
            req = req.header("origin", o);
        }
        req.send().await.unwrap()
    }

    async fn token(&self, app: &str, version: u32) -> String {
        let res = self.session(app, version, Some(&origin(app))).await;
        assert_eq!(res.status(), 200);
        let body: Value = res.json().await.unwrap();
        body["token"].as_str().unwrap().to_string()
    }

    async fn connect(&self, origin: Option<&str>, xff: Option<&str>) -> Result<Client, WsError> {
        let mut req = format!("ws://{}/ws", self.addr).into_client_request().unwrap();
        if let Some(o) = origin {
            req.headers_mut().insert("origin", HeaderValue::from_str(o).unwrap());
        }
        if let Some(ip) = xff {
            req.headers_mut().insert("x-forwarded-for", HeaderValue::from_str(ip).unwrap());
        }
        let (ws, _) = tokio_tungstenite::connect_async(req).await?;
        Ok(Client { ws })
    }

    /// A socket past `hello`/`welcome` for `app` at `version`.
    async fn client(&self, app: &str, version: u32) -> Client {
        self.client_at(app, version, None).await
    }

    /// Same, with the client IP set through X-Forwarded-For.
    async fn client_at(&self, app: &str, version: u32, xff: Option<&str>) -> Client {
        let token = self.token(app, version).await;
        let mut c = self.connect(Some(&origin(app)), xff).await.unwrap();
        c.send(json!({ "t": "hello", "v": 1, "token": token })).await;
        c.expect("welcome").await;
        c
    }
}

struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl Client {
    async fn send(&mut self, msg: Value) {
        self.ws.send(Message::Text(msg.to_string())).await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        loop {
            let msg = timeout(WAIT, self.ws.next())
                .await
                .expect("timed out waiting for a message")
                .expect("socket closed")
                .expect("socket error");
            match msg {
                Message::Text(t) => return serde_json::from_str(&t).unwrap(),
                Message::Close(_) => panic!("socket closed"),
                _ => {}
            }
        }
    }

    /// Next message, which must have type `t`.
    async fn expect(&mut self, t: &str) -> Value {
        let msg = self.recv().await;
        assert_eq!(msg["t"], t, "unexpected message: {msg}");
        msg
    }

    async fn expect_error(&mut self, code: &str) {
        let msg = self.expect("error").await;
        assert_eq!(msg["code"], code, "unexpected error: {msg}");
    }

    /// No text message arrives for a short while.
    async fn quiet(&mut self) {
        let got = timeout(QUIET, async {
            loop {
                match self.ws.next().await {
                    Some(Ok(Message::Text(t))) => return Some(t),
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return None,
                    _ => {}
                }
            }
        })
        .await;
        if let Ok(Some(t)) = got {
            panic!("expected silence, got {t}");
        }
    }

    /// The server ends the connection (close frame or EOF) without further text.
    async fn expect_closed(&mut self) {
        loop {
            match timeout(WAIT, self.ws.next()).await.expect("socket was not closed") {
                Some(Ok(Message::Text(t))) => panic!("expected close, got {t}"),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                _ => {}
            }
        }
    }

    /// `create` and return the `room` object of the `joined` reply.
    async fn create(&mut self, opts: Value) -> Value {
        let mut msg = json!({ "t": "create" });
        msg.as_object_mut().unwrap().extend(opts.as_object().unwrap().clone());
        self.send(msg).await;
        let joined = self.expect("joined").await;
        assert_eq!(joined["resumed"], false);
        joined["room"].clone()
    }

    async fn join(&mut self, room: &Value) -> Value {
        self.send(json!({ "t": "join", "code": room["code"], "key": room["key"] })).await;
        self.expect("joined").await["room"].clone()
    }
}

/// Host creates a private room in `game`; one peer joins. Returns (host, peer, host view, peer view).
async fn room_with_peer(s: &Server) -> (Client, Client, Value, Value) {
    let mut host = s.client("game", 1).await;
    let hroom = host.create(json!({ "public": false, "player": "Hosty" })).await;
    let mut peer = s.client("game", 1).await;
    let proom = peer.join(&hroom).await;
    let joined = host.expect("peer_joined").await;
    assert_eq!(joined["peer"], proom["you"]);
    (host, peer, hroom, proom)
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn healthz_and_metrics() {
    let s = start().await;
    let res = s.http.get(s.url("/healthz")).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.text().await.unwrap(), "ok");

    let mut host = s.client("game", 1).await;
    host.create(json!({})).await;
    let metrics = s.http.get(s.url("/metrics")).send().await.unwrap().text().await.unwrap();
    assert!(metrics.contains("handshake_sessions_total 1"), "{metrics}");
    assert!(metrics.contains("handshake_sockets 1"), "{metrics}");
    assert!(metrics.contains("handshake_rooms{app=\"game\"} 1"), "{metrics}");
}

#[tokio::test]
async fn session_origin_allowlist() {
    let s = start().await;

    let res = s.session("game", 3, Some("https://game.test")).await;
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert!(body["token"].as_str().is_some_and(|t| t.contains('.')));
    assert_eq!(body["expires_in"], 900);
    assert_eq!(body["turn"], true);

    let res = s.session("private", 1, Some("https://private.test")).await;
    assert_eq!(res.json::<Value>().await.unwrap()["turn"], false, "app has turn = false");

    for bad in [Some("https://evil.test"), Some("https://private.test"), Some("http://localhost:5173"), None] {
        let res = s.session("game", 1, bad).await;
        assert_eq!(res.status(), 403, "{bad:?}");
        assert_eq!(res.json::<Value>().await.unwrap(), json!({ "error": "origin" }));
    }

    let res = s.session("nope", 1, Some("https://game.test")).await;
    assert_eq!(res.status(), 404);
    assert_eq!(res.json::<Value>().await.unwrap(), json!({ "error": "not_found" }));
}

#[tokio::test]
async fn session_allow_localhost() {
    let config = format!("allow_localhost = true\n{BASE_CONFIG}");
    let s = start_with(&config, Some(TURN_SECRET)).await;
    for ok in ["http://localhost:5173", "http://127.0.0.1:8080", "https://game.test"] {
        assert_eq!(s.session("game", 1, Some(ok)).await.status(), 200, "{ok}");
    }
    for bad in ["http://localhost.evil.test", "https://localhost", "https://evil.test"] {
        assert_eq!(s.session("game", 1, Some(bad)).await.status(), 403, "{bad}");
    }

    // The WebSocket upgrade honours it too.
    let token = s.token("game", 1).await;
    let mut c = s.connect(Some("http://localhost:5173"), None).await.unwrap();
    c.send(json!({ "t": "hello", "v": 1, "token": token })).await;
    c.expect("welcome").await;
}

#[tokio::test]
async fn session_rate_limited_per_ip() {
    let s = start_limits("sessions_per_min = 2").await;
    let mint = |ip: &'static str| {
        s.http
            .post(s.url("/session"))
            .header("origin", "https://game.test")
            .header("x-forwarded-for", ip)
            .json(&json!({ "app": "game", "version": 1 }))
            .send()
    };
    assert_eq!(mint("203.0.113.5").await.unwrap().status(), 200);
    assert_eq!(mint("203.0.113.5").await.unwrap().status(), 200);
    let res = mint("203.0.113.5").await.unwrap();
    assert_eq!(res.status(), 429);
    assert_eq!(res.json::<Value>().await.unwrap(), json!({ "error": "rate_limited" }));
    // Caddy appends the real client last; only that entry counts.
    assert_eq!(mint("203.0.113.5, 198.51.100.7").await.unwrap().status(), 200);
    // Trusted proxies at the end of the chain are skipped: the client is the last untrusted entry.
    assert_eq!(mint("203.0.113.5, 10.0.0.2").await.unwrap().status(), 429);
}

#[tokio::test]
async fn forwarded_for_ignored_from_untrusted_peer() {
    // With no trusted proxies, X-Forwarded-For is a client's own claim: limits use the socket address.
    let config = format!("trusted_proxies = []\n{BASE_CONFIG}").replace("sessions_per_min = 1000", "sessions_per_min = 2");
    let s = start_with(&config, Some(TURN_SECRET)).await;
    let mint = |ip: &'static str| {
        s.http
            .post(s.url("/session"))
            .header("origin", "https://game.test")
            .header("x-forwarded-for", ip)
            .json(&json!({ "app": "game", "version": 1 }))
            .send()
    };
    assert_eq!(mint("203.0.113.5").await.unwrap().status(), 200);
    assert_eq!(mint("203.0.113.6").await.unwrap().status(), 200);
    assert_eq!(mint("203.0.113.7").await.unwrap().status(), 429, "spoofed addresses share the real one's bucket");
}

#[tokio::test]
async fn cors_preflight() {
    let s = start().await;
    let preflight = |o: &'static str| {
        s.http
            .request(reqwest::Method::OPTIONS, s.url("/session"))
            .header("origin", o)
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "content-type")
            .send()
    };
    let res = preflight("https://game.test").await.unwrap();
    assert_eq!(res.headers()["access-control-allow-origin"], "https://game.test");
    let res = preflight("https://evil.test").await.unwrap();
    assert!(res.headers().get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn turn_credentials() {
    let s = start().await;
    let token = s.token("game", 1).await;
    let res = s.http.post(s.url("/turn")).bearer_auth(&token).send().await.unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["ttl"], 600);
    let server = &body["ice_servers"][0];
    assert_eq!(server["urls"], json!(["stun:turn.test:3478", "turn:turn.test:3478?transport=udp"]));

    // username "<expiry unix>:<app id>", credential base64(HMAC-SHA1(TURN_SECRET, username))
    let username = server["username"].as_str().unwrap();
    let (expiry, app) = username.split_once(':').unwrap();
    assert_eq!(app, "game");
    let expiry: u64 = expiry.parse().unwrap();
    assert!(expiry.abs_diff(now_unix() + 600) <= 2, "expiry {expiry} is now + ttl");
    let mut mac = <Hmac<Sha1>>::new_from_slice(TURN_SECRET.as_bytes()).unwrap();
    mac.update(username.as_bytes());
    assert_eq!(server["credential"], STANDARD.encode(mac.finalize().into_bytes()));
}

#[tokio::test]
async fn turn_rejections() {
    let s = start().await;
    let post = |auth: Option<String>| {
        let mut req = s.http.post(s.url("/turn"));
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        req.send()
    };
    for auth in [None, Some("Bearer garbage".into()), Some("Basic abc".into())] {
        let res = post(auth.clone()).await.unwrap();
        assert_eq!(res.status(), 401, "{auth:?}");
        assert_eq!(res.json::<Value>().await.unwrap(), json!({ "error": "bad_token" }));
    }
    let token = s.token("game", 1).await;
    assert_eq!(post(Some(token.clone())).await.unwrap().status(), 401, "Bearer prefix is required");

    let token = s.token("private", 1).await;
    let res = post(Some(format!("Bearer {token}"))).await.unwrap();
    assert_eq!(res.status(), 403);
    assert_eq!(res.json::<Value>().await.unwrap(), json!({ "error": "turn_disabled" }));

    // [turn] configured but no TURN_SECRET.
    let s = start_with(BASE_CONFIG, None).await;
    let res = s.session("game", 1, Some("https://game.test")).await;
    assert_eq!(res.json::<Value>().await.unwrap()["turn"], false);
    let token = s.token("game", 1).await;
    let res = s.http.post(s.url("/turn")).bearer_auth(&token).send().await.unwrap();
    assert_eq!(res.status(), 503);
    assert_eq!(res.json::<Value>().await.unwrap(), json!({ "error": "turn_unconfigured" }));
}

// ---------------------------------------------------------------------------
// WebSocket handshake
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ws_origin_checks() {
    let s = start().await;
    for bad in [Some("https://evil.test"), None] {
        match s.connect(bad, None).await {
            Err(WsError::Http(res)) => assert_eq!(res.status(), 403, "{bad:?}"),
            Err(e) => panic!("unexpected error {e}"),
            Ok(_) => panic!("upgrade accepted for {bad:?}"),
        }
    }

    // Origin allowed for some app, but not for the token's app.
    let token = s.token("game", 1).await;
    let mut c = s.connect(Some("https://private.test"), None).await.unwrap();
    c.send(json!({ "t": "hello", "v": 1, "token": token })).await;
    c.expect_error("origin").await;
    c.expect_closed().await;
}

#[tokio::test]
async fn hello_rejections() {
    let s = start().await;
    let token = s.token("game", 1).await;
    let cases = [
        (json!({ "t": "hello", "v": 1, "token": "garbage" }), "bad_token"),
        (json!({ "t": "hello", "v": 2, "token": token }), "bad_message"),
        (json!({ "t": "list" }), "bad_token"),
    ];
    for (hello, code) in cases {
        let mut c = s.connect(Some("https://game.test"), None).await.unwrap();
        c.send(hello).await;
        c.expect_error(code).await;
        c.expect_closed().await;
    }
}

/// Hello deadline is a fixed 10 s, so this test takes ~10 s.
#[tokio::test]
async fn hello_deadline_closes_silent_socket() {
    let s = start().await;
    let mut c = s.connect(Some("https://game.test"), None).await.unwrap();
    let started = std::time::Instant::now();
    loop {
        match timeout(Duration::from_secs(15), c.ws.next()).await.expect("not closed after deadline") {
            Some(Ok(Message::Text(t))) => panic!("unexpected {t}"),
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
            _ => {}
        }
    }
    assert!(started.elapsed() >= Duration::from_secs(9), "closed early: {:?}", started.elapsed());
}

#[tokio::test]
async fn bad_messages_after_hello() {
    let s = start().await;
    let mut c = s.client("game", 1).await;
    c.send(json!({ "t": "hello", "v": 1, "token": "x" })).await;
    c.expect_error("bad_message").await;
    c.send(json!({ "t": "dance" })).await;
    c.expect_error("bad_message").await;
    c.ws.send(Message::Text("not json".into())).await.unwrap();
    c.expect_error("bad_message").await;
    for msg in [
        json!({ "t": "signal", "to": "x", "data": {} }),
        json!({ "t": "lock", "locked": true }),
        json!({ "t": "meta", "meta": {} }),
        json!({ "t": "kick", "peer": "x" }),
        json!({ "t": "leave" }),
    ] {
        c.send(msg).await;
        c.expect_error("not_in_room").await;
    }
}

// ---------------------------------------------------------------------------
// Create and join
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_and_join_private() {
    let s = start().await;
    let mut host = s.client("game", 1).await;
    let hroom = host
        .create(json!({ "public": false, "name": "Back yard", "player": "Seth", "meta": { "map": "farm" } }))
        .await;
    let code = hroom["code"].as_str().unwrap();
    assert_eq!(code.len(), 5);
    assert!(code.bytes().all(|b| b"ABCDEFGHJKMNPQRSTUVWXYZ23456789".contains(&b)), "{code}");
    assert_eq!(hroom["name"], "Back yard");
    assert_eq!(hroom["public"], false);
    assert_eq!(hroom["max_players"], 3);
    assert_eq!(hroom["locked"], false);
    assert_eq!(hroom["meta"], json!({ "map": "farm" }));
    assert_eq!(hroom["is_host"], true);
    assert_eq!(hroom["host"], hroom["you"]);
    assert!(hroom["key"].as_str().is_some_and(|k| !k.is_empty()));
    assert!(hroom["resume"].as_str().is_some_and(|r| !r.is_empty()));
    assert_eq!(hroom["peers"], json!([{ "id": hroom["you"], "name": "Seth", "away": false, "nearby": true }]));

    // Missing or wrong key.
    let mut peer = s.client("game", 1).await;
    peer.send(json!({ "t": "join", "code": code })).await;
    peer.expect_error("bad_key").await;
    peer.send(json!({ "t": "join", "code": code, "key": "wrong" })).await;
    peer.expect_error("bad_key").await;
    host.quiet().await;

    // Code is case-insensitive and trimmed.
    let lower = format!(" {} ", code.to_lowercase());
    peer.send(json!({ "t": "join", "code": lower, "key": hroom["key"], "player": "Ana" })).await;
    let proom = peer.expect("joined").await["room"].clone();
    assert_eq!(proom["code"], code);
    assert_eq!(proom["is_host"], false);
    assert_eq!(proom["host"], hroom["you"]);
    assert_ne!(proom["you"], hroom["you"]);
    assert!(proom["key"].is_null(), "only the host sees the key");
    assert_ne!(proom["resume"], hroom["resume"]);
    assert_eq!(proom["peers"].as_array().unwrap().len(), 2);
    assert_eq!(proom["peers"][1], json!({ "id": proom["you"], "name": "Ana", "away": false, "nearby": true }));

    let joined = host.expect("peer_joined").await;
    assert_eq!(joined, json!({ "t": "peer_joined", "peer": proom["you"], "name": "Ana", "nearby": true }));
    peer.quiet().await;
}

#[tokio::test]
async fn join_public_with_code_only() {
    let s = start().await;
    let mut host = s.client("game", 1).await;
    let room = host.create(json!({ "public": true })).await;
    assert_eq!(room["name"], "Room");
    let mut peer = s.client("game", 1).await;
    peer.send(json!({ "t": "join", "code": room["code"] })).await;
    let proom = peer.expect("joined").await["room"].clone();
    assert_eq!(proom["peers"][0]["name"], "Host");
    assert_eq!(proom["peers"][1]["name"], "Player");
    host.expect("peer_joined").await;
}

#[tokio::test]
async fn join_rejections() {
    let s = start().await;
    let mut host = s.client("game", 1).await;
    let room = host.create(json!({ "public": true, "max_players": 2 })).await;
    assert_eq!(room["max_players"], 2);

    let mut c = s.client("game", 1).await;
    c.send(json!({ "t": "join", "code": "ZZZZZ" })).await;
    c.expect_error("not_found").await;

    // Rooms are namespaced by app.
    let mut other_app = s.client("tiny", 1).await;
    other_app.send(json!({ "t": "join", "code": room["code"] })).await;
    other_app.expect_error("not_found").await;

    let mut v2 = s.client("game", 2).await;
    v2.send(json!({ "t": "join", "code": room["code"] })).await;
    v2.expect_error("version_mismatch").await;

    c.join(&room).await;
    host.expect("peer_joined").await;
    c.send(json!({ "t": "join", "code": room["code"] })).await;
    c.expect_error("already_in_room").await;
    c.send(json!({ "t": "create" })).await;
    c.expect_error("already_in_room").await;

    let mut late = s.client("game", 1).await;
    late.send(json!({ "t": "join", "code": room["code"] })).await;
    late.expect_error("full").await;

    let metrics = s.http.get(s.url("/metrics")).send().await.unwrap().text().await.unwrap();
    assert!(metrics.contains("handshake_joins_rejected_total 5"), "{metrics}");
}

#[tokio::test]
async fn join_locked() {
    let s = start().await;
    let mut host = s.client("game", 1).await;
    let room = host.create(json!({ "public": true })).await;
    host.send(json!({ "t": "lock", "locked": true })).await;
    host.expect("room_meta").await;
    let mut c = s.client("game", 1).await;
    c.send(json!({ "t": "join", "code": room["code"] })).await;
    c.expect_error("locked").await;

    host.send(json!({ "t": "lock", "locked": false })).await;
    host.expect("room_meta").await;
    c.join(&room).await;
}

#[tokio::test]
async fn join_rate_limited() {
    let s = start_limits("joins_per_min = 2").await;
    let mut c = s.client_at("game", 1, Some("203.0.113.5")).await;
    for _ in 0..2 {
        c.send(json!({ "t": "join", "code": "ZZZZZ" })).await;
        c.expect_error("not_found").await;
    }
    c.send(json!({ "t": "join", "code": "ZZZZZ" })).await;
    c.expect_error("rate_limited").await;

    let mut other = s.client_at("game", 1, Some("198.51.100.7")).await;
    other.send(json!({ "t": "join", "code": "ZZZZZ" })).await;
    other.expect_error("not_found").await;
}

#[tokio::test]
async fn create_rejections() {
    let s = start().await;

    let mut c = s.client("private", 1).await;
    c.send(json!({ "t": "create", "public": true })).await;
    c.expect_error("public_disabled").await;

    let big = "x".repeat(1100);
    c.send(json!({ "t": "create", "meta": { "blob": big } })).await;
    c.expect_error("meta_too_large").await;

    let mut a = s.client("tiny", 1).await;
    a.create(json!({})).await;
    let mut b = s.client("tiny", 1).await;
    b.send(json!({ "t": "create" })).await;
    b.expect_error("too_many_rooms").await;
    a.send(json!({ "t": "create" })).await;
    a.expect_error("already_in_room").await;
}

#[tokio::test]
async fn create_clamps_room_settings() {
    let s = start().await;
    let mut a = s.client("game", 1).await;
    let room = a.create(json!({ "max_players": 50, "name": "n".repeat(40) })).await;
    assert_eq!(room["max_players"], 3, "capped at the app's max_players");
    assert_eq!(room["name"].as_str().unwrap().chars().count(), 32);
    let mut b = s.client("game", 1).await;
    assert_eq!(b.create(json!({ "max_players": 1 })).await["max_players"], 2);
}

// ---------------------------------------------------------------------------
// Signaling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn signal_relay_host_and_peers_only() {
    let s = start().await;
    let (mut host, mut p1, hroom, proom1) = room_with_peer(&s).await;
    let mut p2 = s.client("game", 1).await;
    let proom2 = p2.join(&hroom).await;
    host.expect("peer_joined").await;
    p1.expect("peer_joined").await;

    let (h, a, b) = (&hroom["you"], &proom1["you"], &proom2["you"]);
    let offer = json!({ "sdp": "v=0 offer", "type": "offer" });
    host.send(json!({ "t": "signal", "to": a, "data": offer })).await;
    assert_eq!(p1.expect("signal").await, json!({ "t": "signal", "from": h, "data": offer }));

    let ice = json!({ "candidate": "candidate:1 1 udp 1 1.2.3.4 5 typ host" });
    p1.send(json!({ "t": "signal", "to": h, "data": ice })).await;
    assert_eq!(host.expect("signal").await, json!({ "t": "signal", "from": a, "data": ice }));

    // Star topology: peer to peer is refused and nothing is delivered.
    p1.send(json!({ "t": "signal", "to": b, "data": {} })).await;
    p1.expect_error("bad_message").await;
    p2.quiet().await;
    host.quiet().await;

    host.send(json!({ "t": "signal", "to": "nobody", "data": {} })).await;
    host.expect_error("peer_unavailable").await;
}

// ---------------------------------------------------------------------------
// Public listing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn devices_on_one_home_network_are_nearby() {
    // The server on the players' own LAN (split DNS): each device arrives with its own private address.
    let s = start().await;
    let mut host = s.client_at("game", 1, Some("192.168.1.20")).await;
    let room = host.create(json!({ "public": true })).await;
    let mut same_lan = s.client_at("game", 1, Some("192.168.1.30")).await;
    let joined = same_lan.join(&room).await;
    assert_eq!(joined["peers"][1]["nearby"], true);
    assert_eq!(host.expect("peer_joined").await["nearby"], true);
    let mut other_lan = s.client_at("game", 1, Some("192.168.2.30")).await;
    other_lan.join(&room).await;
    assert_eq!(host.expect("peer_joined").await["nearby"], false);
}

#[tokio::test]
async fn list_public_rooms() {
    let s = start().await;
    let far = Some("198.51.100.7");
    let near = Some("203.0.113.5");

    // Listed: two public rooms, one hosted on the caller's IP.
    let mut far_host = s.client_at("game", 1, far).await;
    let far_room = far_host.create(json!({ "public": true, "name": "Far", "meta": { "m": 1 } })).await;
    let mut near_host = s.client_at("game", 1, near).await;
    let near_room = near_host.create(json!({ "public": true, "name": "Near" })).await;
    let mut newer_far = s.client_at("game", 1, far).await;
    let newer_far_room = newer_far.create(json!({ "public": true, "name": "Newer" })).await;

    // Not listed: private, locked, full, other version, other app.
    let mut private = s.client_at("game", 1, near).await;
    private.create(json!({ "public": false })).await;
    let mut locked = s.client_at("game", 1, near).await;
    locked.create(json!({ "public": true })).await;
    locked.send(json!({ "t": "lock", "locked": true })).await;
    locked.expect("room_meta").await;
    let mut full = s.client_at("game", 1, near).await;
    let full_room = full.create(json!({ "public": true, "max_players": 2 })).await;
    s.client("game", 1).await.join(&full_room).await;
    let mut v2 = s.client_at("game", 2, near).await;
    v2.create(json!({ "public": true })).await;
    let mut tiny = s.client_at("tiny", 1, near).await;
    tiny.create(json!({ "public": true })).await;

    let mut lister = s.client_at("game", 1, near).await;
    lister.send(json!({ "t": "list" })).await;
    let rooms = lister.expect("rooms").await["rooms"].clone();
    let codes: Vec<&Value> = rooms.as_array().unwrap().iter().map(|r| &r["code"]).collect();
    // Nearby first, then newest.
    assert_eq!(codes, [&near_room["code"], &newer_far_room["code"], &far_room["code"]]);
    assert_eq!(
        rooms[2],
        json!({ "code": far_room["code"], "name": "Far", "players": 1, "max_players": 3, "meta": { "m": 1 }, "nearby": false })
    );
    assert_eq!(rooms[0]["nearby"], true);

    // A room disappears from the list while its host is away.
    drop(near_host);
    tokio::time::sleep(Duration::from_millis(200)).await;
    lister.send(json!({ "t": "list" })).await;
    let rooms = lister.expect("rooms").await["rooms"].clone();
    assert_eq!(rooms.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn list_nearby_ipv6_by_prefix() {
    let s = start().await;
    let mut host = s.client_at("game", 1, Some("2001:db8:1:2::10")).await;
    host.create(json!({ "public": true })).await;

    let mut same64 = s.client_at("game", 1, Some("2001:db8:1:2:abcd::99")).await;
    same64.send(json!({ "t": "list" })).await;
    assert_eq!(same64.expect("rooms").await["rooms"][0]["nearby"], true);

    let mut other64 = s.client_at("game", 1, Some("2001:db8:1:3::10")).await;
    other64.send(json!({ "t": "list" })).await;
    assert_eq!(other64.expect("rooms").await["rooms"][0]["nearby"], false);
}

// ---------------------------------------------------------------------------
// Host controls
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lock_and_meta_broadcast() {
    let s = start().await;
    let (mut host, mut peer, _, _) = room_with_peer(&s).await;

    host.send(json!({ "t": "lock", "locked": true })).await;
    let expected = json!({ "t": "room_meta", "meta": null, "locked": true });
    assert_eq!(host.expect("room_meta").await, expected);
    assert_eq!(peer.expect("room_meta").await, expected);

    host.send(json!({ "t": "meta", "meta": { "mode": "race", "started": true } })).await;
    let expected = json!({ "t": "room_meta", "meta": { "mode": "race", "started": true }, "locked": true });
    assert_eq!(host.expect("room_meta").await, expected);
    assert_eq!(peer.expect("room_meta").await, expected);

    host.send(json!({ "t": "meta", "meta": { "blob": "x".repeat(1100) } })).await;
    host.expect_error("meta_too_large").await;
    peer.quiet().await;

    for msg in [
        json!({ "t": "lock", "locked": false }),
        json!({ "t": "meta", "meta": {} }),
        json!({ "t": "kick", "peer": "x" }),
    ] {
        peer.send(msg).await;
        peer.expect_error("not_host").await;
    }
    host.quiet().await;
}

#[tokio::test]
async fn kick_and_rejoin() {
    let s = start().await;
    let (mut host, mut peer, hroom, proom) = room_with_peer(&s).await;

    host.send(json!({ "t": "kick", "peer": hroom["you"] })).await;
    host.expect_error("bad_message").await;
    host.send(json!({ "t": "kick", "peer": "nobody" })).await;
    host.expect_error("peer_unavailable").await;

    host.send(json!({ "t": "kick", "peer": proom["you"] })).await;
    assert_eq!(peer.expect("kicked").await, json!({ "t": "kicked" }));
    assert_eq!(host.expect("peer_left").await, json!({ "t": "peer_left", "peer": proom["you"], "reason": "kicked" }));

    // The kicked peer's resume token is gone, but it may rejoin an unlocked room.
    peer.send(json!({ "t": "resume", "token": proom["resume"] })).await;
    peer.expect_error("not_found").await;
    peer.join(&hroom).await;
    let rejoined = host.expect("peer_joined").await;
    host.send(json!({ "t": "lock", "locked": true })).await;
    host.expect("room_meta").await;
    peer.expect("room_meta").await;
    host.send(json!({ "t": "kick", "peer": rejoined["peer"] })).await;
    peer.expect("kicked").await;
    host.expect("peer_left").await;
    peer.send(json!({ "t": "join", "code": hroom["code"], "key": hroom["key"] })).await;
    peer.expect_error("locked").await;
}

#[tokio::test]
async fn peer_leave() {
    let s = start().await;
    let (mut host, mut peer, _, proom) = room_with_peer(&s).await;
    peer.send(json!({ "t": "leave" })).await;
    assert_eq!(host.expect("peer_left").await, json!({ "t": "peer_left", "peer": proom["you"], "reason": "left" }));
    peer.quiet().await;
    // Out of the room: can create a new one; the old resume token is dead.
    peer.send(json!({ "t": "resume", "token": proom["resume"] })).await;
    peer.expect_error("not_found").await;
    peer.create(json!({})).await;
}

#[tokio::test]
async fn host_leave_closes_room() {
    let s = start().await;
    let (mut host, mut peer, hroom, proom) = room_with_peer(&s).await;
    host.send(json!({ "t": "leave" })).await;
    assert_eq!(peer.expect("room_closed").await, json!({ "t": "room_closed", "reason": "host_left" }));

    let mut c = s.client("game", 1).await;
    c.send(json!({ "t": "join", "code": hroom["code"], "key": hroom["key"] })).await;
    c.expect_error("not_found").await;
    peer.send(json!({ "t": "resume", "token": proom["resume"] })).await;
    peer.expect_error("not_found").await;
    // Both sockets are free again.
    peer.create(json!({})).await;
    host.expect("room_closed").await; // the host is told too
    host.create(json!({})).await;
}

// ---------------------------------------------------------------------------
// Grace and resume
// ---------------------------------------------------------------------------

#[tokio::test]
async fn peer_drop_and_resume() {
    let s = start().await;
    let (mut host, peer, _, proom) = room_with_peer(&s).await;
    drop(peer);
    assert_eq!(host.expect("peer_away").await, json!({ "t": "peer_away", "peer": proom["you"] }));

    let mut peer = s.client("game", 1).await;
    peer.send(json!({ "t": "resume", "token": proom["resume"] })).await;
    let joined = peer.expect("joined").await;
    assert_eq!(joined["resumed"], true);
    assert_eq!(joined["room"]["you"], proom["you"]);
    assert_eq!(joined["room"]["resume"], proom["resume"]);
    assert_eq!(host.expect("peer_back").await, json!({ "t": "peer_back", "peer": proom["you"] }));

    // Signaling works on the new socket.
    peer.send(json!({ "t": "signal", "to": proom["host"], "data": "hi" })).await;
    assert_eq!(host.expect("signal").await["from"], proom["you"]);
}

#[tokio::test]
async fn peer_grace_expiry() {
    let s = start().await;
    let (mut host, peer, _, proom) = room_with_peer(&s).await;
    drop(peer);
    host.expect("peer_away").await;
    let left = host.expect("peer_left").await; // after grace (1 s) + sweep (<= 5 s)
    assert_eq!(left, json!({ "t": "peer_left", "peer": proom["you"], "reason": "timeout" }));

    let mut peer = s.client("game", 1).await;
    peer.send(json!({ "t": "resume", "token": proom["resume"] })).await;
    peer.expect_error("not_found").await;
}

#[tokio::test]
async fn host_drop_and_resume() {
    let s = start().await;
    let (host, mut peer, hroom, _) = room_with_peer(&s).await;
    drop(host);
    assert_eq!(peer.expect("host_away").await, json!({ "t": "host_away", "grace_secs": 1 }));

    let mut host = s.client("game", 1).await;
    host.send(json!({ "t": "resume", "token": hroom["resume"] })).await;
    let joined = host.expect("joined").await;
    assert_eq!(joined["resumed"], true);
    assert_eq!(joined["room"]["is_host"], true);
    assert_eq!(joined["room"]["key"], hroom["key"]);
    assert_eq!(peer.expect("host_back").await, json!({ "t": "host_back" }));

    // Survives past the grace period once back.
    tokio::time::sleep(Duration::from_secs(6)).await;
    peer.quiet().await;
    host.send(json!({ "t": "lock", "locked": true })).await;
    host.expect("room_meta").await;
}

#[tokio::test]
async fn host_grace_expiry_closes_room() {
    let s = start().await;
    let (host, mut peer, hroom, _) = room_with_peer(&s).await;
    drop(host);
    peer.expect("host_away").await;
    assert_eq!(peer.expect("room_closed").await, json!({ "t": "room_closed", "reason": "host_gone" }));

    let mut host = s.client("game", 1).await;
    host.send(json!({ "t": "resume", "token": hroom["resume"] })).await;
    host.expect_error("not_found").await;
}

#[tokio::test]
async fn away_peer_shows_in_view() {
    let s = start().await;
    let (mut host, peer, hroom, proom) = room_with_peer(&s).await;
    drop(peer);
    host.expect("peer_away").await;
    let mut late = s.client("game", 1).await;
    let view = late.join(&hroom).await;
    let away: Vec<(&Value, &Value)> = view["peers"].as_array().unwrap().iter().map(|p| (&p["id"], &p["away"])).collect();
    assert_eq!(away[1], (&proom["you"], &json!(true)));
}

#[tokio::test]
async fn resume_on_new_socket_replaces_old() {
    let s = start().await;
    let (mut host, mut old, _, proom) = room_with_peer(&s).await;

    let mut new = s.client("game", 1).await;
    new.send(json!({ "t": "resume", "token": proom["resume"] })).await;
    let joined = new.expect("joined").await;
    assert_eq!((&joined["resumed"], &joined["room"]["you"]), (&json!(true), &proom["you"]));
    old.expect_error("replaced").await;
    // The host was never told the peer was away; it still gets peer_back.
    assert_eq!(host.expect("peer_back").await["peer"], proom["you"]);

    // The old socket is detached from the room, and closing it does not mark the peer away.
    old.send(json!({ "t": "signal", "to": proom["host"], "data": {} })).await;
    old.expect_error("not_in_room").await;
    drop(old);
    host.quiet().await;
    new.send(json!({ "t": "signal", "to": proom["host"], "data": 1 })).await;
    assert_eq!(host.expect("signal").await["from"], proom["you"]);
}

#[tokio::test]
async fn resume_rejections() {
    let s = start().await;
    let (_host, mut peer, hroom, _) = room_with_peer(&s).await;
    peer.send(json!({ "t": "resume", "token": hroom["resume"] })).await;
    peer.expect_error("already_in_room").await;

    let mut c = s.client("game", 1).await;
    c.send(json!({ "t": "resume", "token": "nope" })).await;
    c.expect_error("not_found").await;
    // Tokens are scoped to their app.
    let mut other = s.client("tiny", 1).await;
    other.send(json!({ "t": "resume", "token": hroom["resume"] })).await;
    other.expect_error("not_found").await;
}

// ---------------------------------------------------------------------------
// Room expiry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn idle_host_alone_closes_room() {
    let s = start_limits("idle_room_secs = 1").await;
    let mut host = s.client("game", 1).await;
    host.create(json!({})).await;
    assert_eq!(host.expect("room_closed").await, json!({ "t": "room_closed", "reason": "idle" }));
}

#[tokio::test]
async fn idle_timer_stops_while_peers_present() {
    let s = start_limits("idle_room_secs = 1").await;
    let mut host = s.client("game", 1).await;
    let room = host.create(json!({})).await;
    let mut peer = s.client("game", 1).await;
    peer.join(&room).await;
    host.expect("peer_joined").await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    host.quiet().await;

    // Alone again: the timer restarts.
    peer.send(json!({ "t": "leave" })).await;
    host.expect("peer_left").await;
    assert_eq!(host.expect("room_closed").await["reason"], "idle");
}

#[tokio::test]
async fn old_room_expires() {
    let s = start_limits("room_max_age_secs = 1").await;
    let (mut host, mut peer, _, _) = room_with_peer(&s).await;
    let expected = json!({ "t": "room_closed", "reason": "expired" });
    assert_eq!(peer.expect("room_closed").await, expected);
    assert_eq!(host.expect("room_closed").await, expected);
}

#[tokio::test]
async fn list_none_hides_public_rooms() {
    let config = format!(
        "{}\n[apps.unlisted]\norigins = [\"https://unlisted.test\"]\npublic_rooms = true\nlist = \"none\"\n",
        BASE_CONFIG
    );
    let s = start_with(&config, Some(TURN_SECRET)).await;
    let mut host = s.client("unlisted", 1).await;
    let room = host.create(json!({ "public": true })).await;

    let mut lister = s.client("unlisted", 1).await;
    lister.send(json!({ "t": "list" })).await;
    assert_eq!(lister.expect("rooms").await["rooms"], json!([]));

    // Still joinable with the code alone.
    lister.send(json!({ "t": "join", "code": room["code"] })).await;
    lister.expect("joined").await;
}

#[tokio::test]
async fn players_carry_nearby_flag() {
    let s = start().await;
    let mut host = s.client_at("game", 1, Some("203.0.113.5")).await;
    let room = host.create(json!({ "public": true })).await;
    assert_eq!(room["peers"][0]["nearby"], true);

    let mut near = s.client_at("game", 1, Some("203.0.113.5")).await;
    near.join(&room).await;
    assert_eq!(host.expect("peer_joined").await["nearby"], true);

    let mut far = s.client_at("game", 1, Some("198.51.100.7")).await;
    let far_view = far.join(&room).await;
    assert_eq!(host.expect("peer_joined").await["nearby"], false);
    assert_eq!(near.expect("peer_joined").await["nearby"], false);
    let flags: Vec<&Value> = far_view["peers"].as_array().unwrap().iter().map(|p| &p["nearby"]).collect();
    assert_eq!(flags, [&json!(true), &json!(true), &json!(false)]);
}

#[tokio::test]
async fn failed_joins_limited_per_app_across_ips() {
    let s = start_limits("app_failed_joins_per_min = 3").await;
    let mut host = s.client("game", 1).await;
    let room = host.create(json!({ "public": false })).await;

    for i in 0..2 {
        let mut c = s.client_at("game", 1, Some(&format!("198.51.100.{i}"))).await;
        c.send(json!({ "t": "join", "code": "ZZZZZ" })).await;
        c.expect_error("not_found").await;
    }
    let mut c = s.client_at("game", 1, Some("198.51.100.9")).await;
    c.send(json!({ "t": "join", "code": room["code"], "key": "wrong" })).await;
    c.expect_error("bad_key").await;

    // Three failures from three IPs: every join for the app is now refused, even a good one.
    let mut good = s.client_at("game", 1, Some("203.0.113.77")).await;
    good.send(json!({ "t": "join", "code": room["code"], "key": room["key"] })).await;
    good.expect_error("rate_limited").await;

    // Other apps are unaffected.
    let mut other = s.client("tiny", 1).await;
    other.send(json!({ "t": "join", "code": "ZZZZZ" })).await;
    other.expect_error("not_found").await;
}

#[tokio::test]
async fn successful_joins_do_not_count_toward_app_limit() {
    let s = start_limits("app_failed_joins_per_min = 1").await;
    let mut host = s.client("game", 1).await;
    let room = host.create(json!({ "public": true })).await;
    s.client("game", 1).await.join(&room).await;
    s.client("game", 1).await.join(&room).await;
}

#[tokio::test]
async fn peek_describes_room_without_joining() {
    let s = start().await;
    let mut host = s.client("game", 1).await;
    let room = host.create(json!({ "public": true, "name": "Farm", "max_players": 2 })).await;

    let mut c = s.client("game", 1).await;
    c.send(json!({ "t": "peek", "code": room["code"].as_str().unwrap().to_lowercase() })).await;
    assert_eq!(
        c.expect("room_info").await,
        json!({ "t": "room_info", "code": room["code"], "name": "Farm", "players": 1, "max_players": 2, "locked": false, "full": false })
    );
    host.quiet().await; // a peek is invisible to the host

    c.join(&room).await;
    host.expect("peer_joined").await;
    let mut d = s.client("game", 1).await;
    d.send(json!({ "t": "peek", "code": room["code"] })).await;
    let info = d.expect("room_info").await;
    assert_eq!((&info["players"], &info["full"]), (&json!(2), &json!(true)));

    host.send(json!({ "t": "lock", "locked": true })).await;
    host.expect("room_meta").await;
    d.send(json!({ "t": "peek", "code": room["code"] })).await;
    assert_eq!(d.expect("room_info").await["locked"], true);

    c.expect("room_meta").await; // c is in the room, so it heard the lock too
    c.send(json!({ "t": "peek", "code": room["code"] })).await;
    c.expect_error("already_in_room").await;
}

#[tokio::test]
async fn peek_rejections_share_the_join_budget() {
    let s = start_limits("joins_per_min = 3").await;
    let mut host = s.client("game", 1).await;
    let room = host.create(json!({ "public": true })).await;

    let mut c = s.client_at("game", 2, Some("203.0.113.5")).await;
    c.send(json!({ "t": "peek", "code": "ZZZZZ" })).await;
    c.expect_error("not_found").await;
    c.send(json!({ "t": "peek", "code": room["code"] })).await;
    c.expect_error("version_mismatch").await;
    c.send(json!({ "t": "peek", "code": room["code"] })).await;
    c.expect_error("version_mismatch").await;
    // Three peeks used this IP's three attempts: the next join is refused.
    c.send(json!({ "t": "join", "code": room["code"] })).await;
    c.expect_error("rate_limited").await;

    let metrics = s.http.get(s.url("/metrics")).send().await.unwrap().text().await.unwrap();
    assert!(metrics.contains("handshake_joins_rejected_total 4"), "{metrics}");
}

#[tokio::test]
async fn peek_private_room_needs_its_key() {
    let s = start_limits("app_failed_joins_per_min = 2").await;
    let mut host = s.client("game", 1).await;
    let room = host.create(json!({ "public": false })).await;

    let mut c = s.client_at("game", 1, Some("198.51.100.1")).await;
    c.send(json!({ "t": "peek", "code": room["code"] })).await;
    c.expect_error("bad_key").await;
    c.send(json!({ "t": "peek", "code": room["code"], "key": "wrong" })).await;
    c.expect_error("bad_key").await;
    let metrics = s.http.get(s.url("/metrics")).send().await.unwrap().text().await.unwrap();
    assert!(metrics.contains("handshake_joins_rejected_total 2"), "{metrics}");

    // Two bad keys used the app's failure budget: now even the right key is refused, from any IP.
    let mut d = s.client_at("game", 1, Some("203.0.113.9")).await;
    d.send(json!({ "t": "peek", "code": room["code"], "key": room["key"] })).await;
    d.expect_error("rate_limited").await;
}

#[tokio::test]
async fn peek_private_room_with_its_key() {
    let s = start().await;
    let mut host = s.client("game", 1).await;
    let room = host.create(json!({ "public": false, "name": "Secret" })).await;
    let mut c = s.client("game", 1).await;
    c.send(json!({ "t": "peek", "code": room["code"], "key": room["key"] })).await;
    let info = c.expect("room_info").await;
    assert_eq!((&info["name"], &info["players"]), (&json!("Secret"), &json!(1)));
}

#[tokio::test]
async fn failed_peeks_count_toward_app_limit() {
    let s = start_limits("app_failed_joins_per_min = 1").await;
    let mut host = s.client("game", 1).await;
    let room = host.create(json!({ "public": true })).await;
    let mut c = s.client_at("game", 1, Some("198.51.100.1")).await;
    c.send(json!({ "t": "peek", "code": "ZZZZZ" })).await;
    c.expect_error("not_found").await;
    let mut good = s.client_at("game", 1, Some("203.0.113.9")).await;
    good.send(json!({ "t": "peek", "code": room["code"] })).await;
    good.expect_error("rate_limited").await;
}

#[tokio::test]
async fn failed_joins_limited_per_ip() {
    let s = start_limits("ip_failed_joins_per_min = 2").await;
    let mut host = s.client("game", 1).await;
    let room = host.create(json!({ "public": true })).await;

    let mut bad = s.client_at("game", 1, Some("198.51.100.1")).await;
    for _ in 0..2 {
        bad.send(json!({ "t": "join", "code": "ZZZZZ" })).await;
        bad.expect_error("not_found").await;
    }
    // This IP used its failures: even a good code is refused for it...
    bad.send(json!({ "t": "peek", "code": room["code"] })).await;
    bad.expect_error("rate_limited").await;
    // ...but not for anyone else.
    s.client_at("game", 1, Some("203.0.113.9")).await.join(&room).await;
}

#[tokio::test]
async fn ten_guessing_ips_do_not_lock_out_joins() {
    let s = start_limits("joins_per_min = 20").await; // the default per-IP attempt budget
    let mut host = s.client("game", 1).await;
    let room = host.create(json!({ "public": true })).await;
    for i in 0..10 {
        let mut c = s.client_at("game", 1, Some(&format!("198.51.100.{i}"))).await;
        for _ in 0..20 {
            c.send(json!({ "t": "join", "code": "ZZZZZ" })).await;
            assert_eq!(c.expect("error").await["re"], "join");
        }
    }
    s.client_at("game", 1, Some("203.0.113.9")).await.join(&room).await;
}

#[tokio::test]
async fn errors_name_the_request_they_answer() {
    let s = start().await;
    let (mut host, peer, hroom, proom) = room_with_peer(&s).await;
    drop(peer);
    host.expect("peer_away").await;
    host.send(json!({ "t": "signal", "to": proom["you"], "data": {} })).await;
    let e = host.expect("error").await;
    assert_eq!((&e["code"], &e["re"]), (&json!("peer_unavailable"), &json!("signal")));
    host.send(json!({ "t": "kick", "peer": "nobody" })).await;
    assert_eq!(host.expect("error").await["re"], "kick");

    let mut c = s.client("game", 1).await;
    c.send(json!({ "t": "join", "code": "ZZZZZ" })).await;
    assert_eq!(c.expect("error").await["re"], "join");
    c.send(json!({ "t": "resume", "token": "nope" })).await;
    assert_eq!(c.expect("error").await["re"], "resume");
    c.send(json!({ "t": "lock", "locked": true })).await;
    assert_eq!(c.expect("error").await["re"], "lock");
    c.send(json!({ "t": "dance" })).await;
    assert_eq!(c.expect("error").await["re"], "dance"); // unrecognized, but it has a type
    c.ws.send(Message::Text("not json".into())).await.unwrap();
    let e = c.expect("error").await;
    assert_eq!((&e["code"], e.get("re")), (&json!("bad_message"), None));
    let _ = hroom;
}
