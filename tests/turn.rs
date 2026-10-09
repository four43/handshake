//! Built-in TURN server tests: run it in-process on 127.0.0.1 and drive it with a small TURN client over real UDP
//! and TCP sockets. Relay addresses are 127.0.0.1, so most configs allow loopback peers.

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use handshake::{
    turn::{
        auth::{long_term_key, password, REALM},
        stun::{self, attr, method, Builder, Class, Frame, Message, TxId},
        Stats, TurnServer, Tuning,
    },
    App, Config,
};
use rand::RngCore;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    time::timeout,
};

const SECRET: &[u8] = b"turn-test-secret";
const WAIT: Duration = Duration::from_secs(5);
const QUIET: Duration = Duration::from_millis(300);

/// Each server gets its own relay ports, so tests can run in parallel.
static NEXT_RANGE: AtomicU16 = AtomicU16::new(42_000);

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn config(turn_extra: &str, top: &str) -> Config {
    let first = NEXT_RANGE.fetch_add(20, Ordering::Relaxed);
    let text = format!(
        r#"
{top}
[turn]
urls = []
listen = "127.0.0.1:0"
relay_ports = "{first}-{last}"
external_ip = "127.0.0.1"
{turn_extra}

[apps.game]
origins = ["https://game.test"]
turn = true

[apps.private]
origins = ["https://private.test"]
turn = false
"#,
        last = first + 19
    );
    toml::from_str(&text).expect("test config parses")
}

struct Server {
    turn: TurnServer,
    stats: Arc<Stats>,
    cfg: Config,
}

async fn start(turn_extra: &str) -> Server {
    start_with(turn_extra, "", Tuning::default()).await
}

async fn start_with(turn_extra: &str, top: &str, tuning: Tuning) -> Server {
    let cfg = config(turn_extra, top);
    let stats = Arc::new(Stats::default());
    let turn = TurnServer::start_tuned(&cfg, SECRET.to_vec(), stats.clone(), tuning).await.expect("TURN starts");
    Server { turn, stats, cfg }
}

/// Loopback relays (the usual test setup).
const LOOPBACK: &str = r#"allowed_peers = ["127.0.0.0/8"]"#;

impl Server {
    fn addr(&self) -> SocketAddr {
        self.turn.local_addr()
    }
    fn in_range(&self, port: u16) -> bool {
        let r = self.cfg.turn.as_ref().unwrap().relay_ports.unwrap();
        (r.first..=r.last).contains(&port)
    }
    fn allocations(&self) -> u64 {
        self.stats.allocations.load(Ordering::Relaxed)
    }
}

fn tx() -> TxId {
    let mut t = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut t);
    t
}

fn error_code(m: &Message) -> Option<u16> {
    m.get(attr::ERROR_CODE).map(|v| v[2] as u16 * 100 + v[3] as u16)
}

fn xaddr(m: &Message, t: u16) -> SocketAddr {
    stun::xor_addr(m.get(t).expect("address attribute"), &m.tx).unwrap()
}

/// A UDP peer that echoes everything back.
async fn echo_peer() -> SocketAddr {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        while let Ok((n, from)) = sock.recv_from(&mut buf).await {
            let _ = sock.send_to(&buf[..n], from).await;
        }
    });
    addr
}

// ---------------------------------------------------------------------------
// Test client
// ---------------------------------------------------------------------------

enum Transport {
    Udp(UdpSocket, SocketAddr),
    Tcp(TcpStream, Vec<u8>),
}

struct Client {
    t: Transport,
    user: String,
    pass: String,
    nonce: Vec<u8>,
}

impl Client {
    async fn udp(server: &Server) -> Client {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        Client::new(Transport::Udp(sock, server.addr()))
    }

    async fn tcp(server: &Server) -> Client {
        Client::new(Transport::Tcp(TcpStream::connect(server.addr()).await.unwrap(), Vec::new()))
    }

    fn new(t: Transport) -> Client {
        let user = format!("{}:game", now_unix() + 600);
        let pass = password(SECRET, &user);
        Client { t, user, pass, nonce: Vec::new() }
    }

    fn local_addr(&self) -> SocketAddr {
        match &self.t {
            Transport::Udp(s, _) => s.local_addr().unwrap(),
            Transport::Tcp(s, _) => s.local_addr().unwrap(),
        }
    }

    fn key(&self) -> [u8; 16] {
        long_term_key(&self.user, REALM, &self.pass)
    }

    async fn send(&mut self, bytes: &[u8]) {
        match &mut self.t {
            Transport::Udp(s, to) => {
                s.send_to(bytes, *to).await.unwrap();
            }
            Transport::Tcp(s, _) => s.write_all(bytes).await.unwrap(),
        }
    }

    /// The next datagram or TCP frame, or `None` within `wait` (or when a TCP connection closes).
    async fn recv_within(&mut self, wait: Duration) -> Option<Vec<u8>> {
        match &mut self.t {
            Transport::Udp(s, _) => {
                let mut buf = vec![0u8; 65_536];
                let (n, _) = timeout(wait, s.recv_from(&mut buf)).await.ok()?.ok()?;
                buf.truncate(n);
                Some(buf)
            }
            Transport::Tcp(s, buf) => loop {
                if let Frame::Complete(n) = stun::frame(buf) {
                    return Some(buf.drain(..n).collect());
                }
                let n = timeout(wait, s.read_buf(buf)).await.ok()?.ok()?;
                if n == 0 {
                    return None;
                }
            },
        }
    }

    async fn recv(&mut self) -> Vec<u8> {
        self.recv_within(WAIT).await.expect("a reply")
    }

    /// Send `req` and return the response to it (skipping Data indications and ChannelData in between).
    async fn call(&mut self, req: Vec<u8>) -> Vec<u8> {
        let want = Message::parse(&req).unwrap().tx;
        self.send(&req).await;
        loop {
            let got = self.recv().await;
            if Message::parse(&got).is_some_and(|m| m.tx == want && matches!(m.class, Class::Success | Class::Error)) {
                return got;
            }
        }
    }

    /// An authenticated request with the current nonce.
    fn authed(&self, m: u16, build: impl FnOnce(Builder) -> Builder) -> Vec<u8> {
        build(Builder::new(m, Class::Request, tx()))
            .attr(attr::USERNAME, self.user.as_bytes())
            .attr(attr::REALM, REALM.as_bytes())
            .attr(attr::NONCE, &self.nonce)
            .finish(Some(&self.key()))
    }

    fn allocate_req(&self) -> Vec<u8> {
        self.authed(method::ALLOCATE, |b| b.attr(attr::REQUESTED_TRANSPORT, &[17, 0, 0, 0]))
    }

    /// Unauthenticated Allocate → 401 (keeps its nonce) → authenticated Allocate. Returns the success response.
    async fn allocate(&mut self) -> Vec<u8> {
        let first = self
            .call(Builder::new(method::ALLOCATE, Class::Request, tx()).attr(attr::REQUESTED_TRANSPORT, &[17, 0, 0, 0]).finish(None))
            .await;
        let m = Message::parse(&first).unwrap();
        assert_eq!(error_code(&m), Some(401));
        assert_eq!(m.get(attr::REALM), Some(REALM.as_bytes()));
        self.nonce = m.get(attr::NONCE).expect("nonce").to_vec();
        let resp = self.call(self.allocate_req()).await;
        let m = Message::parse(&resp).unwrap();
        assert_eq!(m.class, Class::Success, "allocate failed: {:?}", error_code(&m));
        assert!(m.check_integrity(&self.key()));
        resp
    }

    /// Allocate and return the relayed address.
    async fn relayed(&mut self) -> SocketAddr {
        let resp = self.allocate().await;
        xaddr(&Message::parse(&resp).unwrap(), attr::XOR_RELAYED_ADDRESS)
    }

    async fn permit(&mut self, peer: SocketAddr) -> Option<u16> {
        let resp = self.call(self.authed(method::CREATE_PERMISSION, |b| b.xor_addr(attr::XOR_PEER_ADDRESS, peer))).await;
        error_code(&Message::parse(&resp).unwrap())
    }

    async fn bind(&mut self, ch: u16, peer: SocketAddr) -> Option<u16> {
        let resp = self
            .call(self.authed(method::CHANNEL_BIND, |b| {
                b.attr(attr::CHANNEL_NUMBER, &[(ch >> 8) as u8, ch as u8, 0, 0]).xor_addr(attr::XOR_PEER_ADDRESS, peer)
            }))
            .await;
        error_code(&Message::parse(&resp).unwrap())
    }

    async fn send_to_peer(&mut self, peer: SocketAddr, data: &[u8]) {
        let ind = Builder::new(method::SEND, Class::Indication, tx())
            .xor_addr(attr::XOR_PEER_ADDRESS, peer)
            .attr(attr::DATA, data)
            .finish(None);
        self.send(&ind).await;
    }

    /// The next Data indication: (peer, data).
    async fn data(&mut self) -> Option<(SocketAddr, Vec<u8>)> {
        let got = self.recv_within(WAIT).await?;
        let m = Message::parse(&got)?;
        assert_eq!((m.method, m.class), (method::DATA, Class::Indication));
        Some((xaddr(&m, attr::XOR_PEER_ADDRESS), m.get(attr::DATA).unwrap().to_vec()))
    }

    async fn refresh(&mut self, lifetime: u32) -> Option<u16> {
        let resp = self.call(self.authed(method::REFRESH, |b| b.attr(attr::LIFETIME, &lifetime.to_be_bytes()))).await;
        error_code(&Message::parse(&resp).unwrap())
    }
}

async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !ok() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// UDP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn binding_needs_no_auth() {
    let s = start(LOOPBACK).await;
    let mut c = Client::udp(&s).await;
    let resp = c.call(Builder::new(method::BINDING, Class::Request, tx()).finish(None)).await;
    let m = Message::parse(&resp).unwrap();
    assert_eq!(m.class, Class::Success);
    assert_eq!(xaddr(&m, attr::XOR_MAPPED_ADDRESS), c.local_addr());
}

#[tokio::test]
async fn allocate_after_challenge() {
    let s = start(LOOPBACK).await;
    let mut c = Client::udp(&s).await;
    let resp = c.allocate().await;
    let m = Message::parse(&resp).unwrap();
    let relayed = xaddr(&m, attr::XOR_RELAYED_ADDRESS);
    assert_eq!(relayed.ip().to_string(), "127.0.0.1", "external_ip");
    assert!(s.in_range(relayed.port()), "{relayed} in relay_ports");
    assert_eq!(xaddr(&m, attr::XOR_MAPPED_ADDRESS), c.local_addr());
    assert_eq!(m.get(attr::LIFETIME), Some(&600u32.to_be_bytes()[..]));
    assert_eq!((s.allocations(), s.stats.allocations_total.load(Ordering::Relaxed)), (1, 1));
}

#[tokio::test]
async fn send_and_data_need_a_permission() {
    let s = start(LOOPBACK).await;
    let peer = echo_peer().await;
    let mut c = Client::udp(&s).await;
    c.relayed().await;

    c.send_to_peer(peer, b"dropped").await;
    assert!(c.recv_within(QUIET).await.is_none(), "no permission: nothing relayed");

    assert_eq!(c.permit(peer).await, None);
    c.send_to_peer(peer, b"ping").await;
    assert_eq!(c.data().await, Some((peer, b"ping".to_vec())));
}

#[tokio::test]
async fn channel_data_both_ways() {
    let s = start(LOOPBACK).await;
    let peer = echo_peer().await;
    let mut c = Client::udp(&s).await;
    c.relayed().await;
    assert_eq!(c.bind(0x4000, peer).await, None);
    c.send(&stun::channel_data(0x4000, b"over a channel", false)).await;
    let got = c.recv().await;
    assert_eq!(stun::parse_channel_data(&got), Some((0x4000, &b"over a channel"[..])));

    assert_eq!(c.bind(0x3FFF, peer).await, Some(400), "outside the channel range");
    assert_eq!(c.bind(0x4001, peer).await, Some(400), "peer already has a channel");
}

#[tokio::test]
async fn stranger_cannot_reach_the_client() {
    let s = start(LOOPBACK).await;
    let mut c = Client::udp(&s).await;
    let relayed = c.relayed().await;
    let stranger = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    stranger.send_to(b"hi", relayed).await.unwrap();
    assert!(c.recv_within(QUIET).await.is_none());

    assert_eq!(c.permit(stranger.local_addr().unwrap()).await, None);
    stranger.send_to(b"hi", relayed).await.unwrap();
    assert_eq!(c.data().await, Some((stranger.local_addr().unwrap(), b"hi".to_vec())));
}

#[tokio::test]
async fn relay_to_relay_in_process() {
    // No allowed_peers: 127.0.0.1 is forbidden as a peer, except as this server's own relay addresses.
    let s = start("").await;
    let (mut a, mut b) = (Client::udp(&s).await, Client::udp(&s).await);
    let (ra, rb) = (a.relayed().await, b.relayed().await);
    assert_eq!(a.permit(echo_peer().await).await, Some(403), "loopback is forbidden");
    assert_eq!(a.permit(rb).await, None);
    assert_eq!(b.permit(ra).await, None);
    a.send_to_peer(rb, b"a to b").await;
    assert_eq!(b.data().await, Some((ra, b"a to b".to_vec())));
    assert_eq!(b.bind(0x4abc, ra).await, None);
    a.send_to_peer(rb, b"by channel").await;
    assert_eq!(stun::parse_channel_data(&b.recv().await), Some((0x4abc, &b"by channel"[..])));
}

#[tokio::test]
async fn refresh_and_delete() {
    let s = start(LOOPBACK).await;
    let mut c = Client::udp(&s).await;
    c.relayed().await;
    assert_eq!(c.refresh(1200).await, None);
    assert_eq!(c.refresh(0).await, None);
    assert_eq!(s.allocations(), 0);
    assert_eq!(c.refresh(600).await, Some(437), "nothing left to refresh");
    c.allocate().await; // the 5-tuple is free again
}

#[tokio::test]
async fn retransmitted_allocate_and_mismatch() {
    let s = start(LOOPBACK).await;
    let mut c = Client::udp(&s).await;
    c.allocate().await;
    let resp = c.call(c.allocate_req()).await;
    assert_eq!(error_code(&Message::parse(&resp).unwrap()), Some(437), "a second Allocate (new transaction) is a mismatch");

    // A retransmission of the Allocate that made the allocation gets the same answer, byte for byte.

    let s = start(LOOPBACK).await;
    let mut c = Client::udp(&s).await;
    let first = c.call(Builder::new(method::ALLOCATE, Class::Request, tx()).attr(attr::REQUESTED_TRANSPORT, &[17, 0, 0, 0]).finish(None)).await;
    c.nonce = Message::parse(&first).unwrap().get(attr::NONCE).unwrap().to_vec();
    let req = c.allocate_req();
    let one = c.call(req.clone()).await;
    let two = c.call(req).await;
    assert_eq!(one, two);
    assert_eq!(s.allocations(), 1);
}

#[tokio::test]
async fn stale_nonce() {
    let tuning = Tuning { nonce_secs: 1, ..Tuning::default() };
    let s = start_with(LOOPBACK, "", tuning).await;
    let mut c = Client::udp(&s).await;
    let first = c.call(Builder::new(method::ALLOCATE, Class::Request, tx()).attr(attr::REQUESTED_TRANSPORT, &[17, 0, 0, 0]).finish(None)).await;
    c.nonce = Message::parse(&first).unwrap().get(attr::NONCE).unwrap().to_vec();
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let resp = c.call(c.allocate_req()).await;
    let m = Message::parse(&resp).unwrap();
    assert_eq!(error_code(&m), Some(438));
    c.nonce = m.get(attr::NONCE).unwrap().to_vec();
    let resp = c.call(c.allocate_req()).await;
    assert_eq!(Message::parse(&resp).unwrap().class, Class::Success);
}

#[tokio::test]
async fn bad_credentials() {
    let s = start(LOOPBACK).await;
    for (user, pass) in [
        (format!("{}:game", now_unix() + 600), "wrong".to_string()),
        (format!("{}:game", now_unix() - 1), password(SECRET, &format!("{}:game", now_unix() - 1))),
        (format!("{}:private", now_unix() + 600), password(SECRET, &format!("{}:private", now_unix() + 600))),
    ] {
        let mut c = Client::udp(&s).await;
        let first = c.call(Builder::new(method::ALLOCATE, Class::Request, tx()).attr(attr::REQUESTED_TRANSPORT, &[17, 0, 0, 0]).finish(None)).await;
        c.nonce = Message::parse(&first).unwrap().get(attr::NONCE).unwrap().to_vec();
        (c.user, c.pass) = (user.clone(), pass);
        let resp = c.call(c.allocate_req()).await;
        assert_eq!(error_code(&Message::parse(&resp).unwrap()), Some(401), "{user}");
    }
    assert_eq!(s.stats.auth_failures.load(Ordering::Relaxed), 3);
    assert_eq!(s.allocations(), 0);

    // A nonce minted for another IP address is no good either. (Both clients are 127.0.0.1, so forge one instead.)
    let mut c = Client::udp(&s).await;
    c.nonce = b"9999999999.AAAAAAAAAAAAAAAA".to_vec();
    let resp = c.call(c.allocate_req()).await;
    assert_eq!(error_code(&Message::parse(&resp).unwrap()), Some(401));
}

#[tokio::test]
async fn wrong_credentials_for_an_allocation() {
    let s = start(LOOPBACK).await;
    let mut c = Client::udp(&s).await;
    c.relayed().await;
    c.user = format!("{}:game", now_unix() + 700);
    c.pass = password(SECRET, &c.user);
    assert_eq!(c.permit("8.8.8.8:53".parse().unwrap()).await, Some(441));
}

#[tokio::test]
async fn forbidden_peers() {
    let s = start("").await;
    let mut c = Client::udp(&s).await;
    c.relayed().await;
    for peer in ["10.0.0.1:1", "127.0.0.1:9", "169.254.169.254:80", "192.168.1.1:1"] {
        assert_eq!(c.permit(peer.parse().unwrap()).await, Some(403), "{peer}");
    }
    assert_eq!(c.bind(0x4000, "172.16.0.1:1".parse().unwrap()).await, Some(403));
    assert_eq!(c.permit("[2001:db8::1]:1".parse().unwrap()).await, Some(443));
    assert_eq!(c.permit("8.8.8.8:53".parse().unwrap()).await, None);
}

#[tokio::test]
async fn quotas() {
    let s = start(&format!("{LOOPBACK}\nallocations_per_ip = 1")).await;
    Client::udp(&s).await.allocate().await;
    let mut c = Client::udp(&s).await;
    let first = c.call(Builder::new(method::ALLOCATE, Class::Request, tx()).attr(attr::REQUESTED_TRANSPORT, &[17, 0, 0, 0]).finish(None)).await;
    c.nonce = Message::parse(&first).unwrap().get(attr::NONCE).unwrap().to_vec();
    assert_eq!(error_code(&Message::parse(&c.call(c.allocate_req()).await).unwrap()), Some(486));

    let s = start(&format!("{LOOPBACK}\nmax_allocations = 1")).await;
    Client::udp(&s).await.allocate().await;
    let mut c = Client::udp(&s).await;
    let first = c.call(Builder::new(method::ALLOCATE, Class::Request, tx()).attr(attr::REQUESTED_TRANSPORT, &[17, 0, 0, 0]).finish(None)).await;
    c.nonce = Message::parse(&first).unwrap().get(attr::NONCE).unwrap().to_vec();
    assert_eq!(error_code(&Message::parse(&c.call(c.allocate_req()).await).unwrap()), Some(508));
    assert_eq!(s.stats.quota_rejections.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn unsupported_requests() {
    let s = start(LOOPBACK).await;
    let mut c = Client::udp(&s).await;
    let first = c.call(Builder::new(method::ALLOCATE, Class::Request, tx()).attr(attr::REQUESTED_TRANSPORT, &[17, 0, 0, 0]).finish(None)).await;
    c.nonce = Message::parse(&first).unwrap().get(attr::NONCE).unwrap().to_vec();

    let tcp_relay = c.authed(method::ALLOCATE, |b| b.attr(attr::REQUESTED_TRANSPORT, &[6, 0, 0, 0]));
    assert_eq!(error_code(&Message::parse(&c.call(tcp_relay).await).unwrap()), Some(442));
    let v6 = c.authed(method::ALLOCATE, |b| b.attr(attr::REQUESTED_TRANSPORT, &[17, 0, 0, 0]).attr(attr::REQUESTED_ADDRESS_FAMILY, &[2, 0, 0, 0]));
    assert_eq!(error_code(&Message::parse(&c.call(v6).await).unwrap()), Some(440));

    let even = c.authed(method::ALLOCATE, |b| b.attr(attr::REQUESTED_TRANSPORT, &[17, 0, 0, 0]).attr(attr::EVEN_PORT, &[0x80, 0, 0, 0]));
    let resp = c.call(even).await;
    let m = Message::parse(&resp).unwrap();
    assert_eq!(error_code(&m), Some(420));
    assert_eq!(m.get(attr::UNKNOWN_ATTRIBUTES), Some(&attr::EVEN_PORT.to_be_bytes()[..]));
    assert_eq!(s.allocations(), 0);
}

#[tokio::test]
async fn dropping_the_server_frees_allocations() {
    // Lifetimes are at least 600 s, so expiry by the sweeper is unit-tested in allocation.rs instead.
    let s = start(LOOPBACK).await;
    Client::udp(&s).await.relayed().await;
    let stats = s.stats.clone();
    assert_eq!(stats.allocations.load(Ordering::Relaxed), 1);
    drop(s);
    assert_eq!(stats.allocations.load(Ordering::Relaxed), 0);
}

// ---------------------------------------------------------------------------
// TCP and PROXY protocol
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tcp_relaying() {
    let s = start(LOOPBACK).await;
    let peer = echo_peer().await;
    let mut c = Client::tcp(&s).await;
    let resp = c.allocate().await;
    assert_eq!(xaddr(&Message::parse(&resp).unwrap(), attr::XOR_MAPPED_ADDRESS), c.local_addr());
    assert_eq!(c.permit(peer).await, None);
    c.send_to_peer(peer, b"over tcp").await;
    assert_eq!(c.data().await, Some((peer, b"over tcp".to_vec())));

    assert_eq!(c.bind(0x4001, peer).await, None);
    // Five bytes: padded to eight on the wire in both directions.
    let frame = stun::channel_data(0x4001, b"abcde", true);
    assert_eq!(frame.len(), 12);
    c.send(&frame).await;
    let got = c.recv().await;
    assert_eq!(got.len(), 12, "padded");
    assert_eq!(stun::parse_channel_data(&got), Some((0x4001, &b"abcde"[..])));
}

#[tokio::test]
async fn tcp_framing() {
    let s = start(LOOPBACK).await;
    let mut c = Client::tcp(&s).await;
    let (one, two) = (Builder::new(method::BINDING, Class::Request, tx()).finish(None), Builder::new(method::BINDING, Class::Request, tx()).finish(None));
    let mut both = one.clone();
    both.extend_from_slice(&two);
    c.send(&both).await;
    for want in [&one, &two] {
        let got = c.recv().await;
        assert_eq!(Message::parse(&got).unwrap().tx, Message::parse(want).unwrap().tx);
    }
    let three = Builder::new(method::BINDING, Class::Request, tx()).finish(None);
    c.send(&three[..7]).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    c.send(&three[7..]).await;
    assert_eq!(Message::parse(&c.recv().await).unwrap().tx, Message::parse(&three).unwrap().tx);

    c.send(b"POST / HTTP/1.1\r\n\r\n").await;
    assert!(c.recv_within(WAIT).await.is_none(), "not STUN: closed");
}

#[tokio::test]
async fn tcp_close_frees_the_allocation() {
    let s = start(LOOPBACK).await;
    let mut c = Client::tcp(&s).await;
    c.relayed().await;
    assert_eq!(s.allocations(), 1);
    drop(c);
    eventually("the allocation to go", || s.allocations() == 0).await;
}

fn proxy_header(src: SocketAddr, dst: SocketAddr) -> Vec<u8> {
    let (SocketAddr::V4(src), SocketAddr::V4(dst)) = (src, dst) else { panic!("IPv4 only") };
    let mut h = handshake::turn::proxy::SIGNATURE.to_vec();
    h.extend_from_slice(&[0x21, 0x11, 0, 12]);
    h.extend_from_slice(&src.ip().octets());
    h.extend_from_slice(&dst.ip().octets());
    h.extend_from_slice(&src.port().to_be_bytes());
    h.extend_from_slice(&dst.port().to_be_bytes());
    h
}

#[tokio::test]
async fn proxy_header_from_a_trusted_proxy() {
    // 127.0.0.0/8 is in the default trusted_proxies.
    let s = start(LOOPBACK).await;
    let client: SocketAddr = "198.51.100.7:4242".parse().unwrap();
    let mut c = Client::tcp(&s).await;
    let mut first = proxy_header(client, s.addr());
    first.extend_from_slice(&Builder::new(method::BINDING, Class::Request, tx()).finish(None));
    c.send(&first).await;
    assert_eq!(xaddr(&Message::parse(&c.recv().await).unwrap(), attr::XOR_MAPPED_ADDRESS), client);

    // Quotas count the address in the header: one allocation per IP, and the proxy's own address is not that IP.
    let s = start(&format!("{LOOPBACK}\nallocations_per_ip = 1")).await;
    let mut a = Client::tcp(&s).await;
    a.send(&proxy_header(client, s.addr())).await;
    let resp = a.allocate().await;
    assert_eq!(xaddr(&Message::parse(&resp).unwrap(), attr::XOR_MAPPED_ADDRESS), client);
    Client::tcp(&s).await.allocate().await; // 127.0.0.1 directly: a different IP, so allowed

    // A trusted peer without a header is a client itself (LAN players connecting directly).
    let mut c = Client::tcp(&s).await;
    let resp = c.call(Builder::new(method::BINDING, Class::Request, tx()).finish(None)).await;
    assert_eq!(xaddr(&Message::parse(&resp).unwrap(), attr::XOR_MAPPED_ADDRESS), c.local_addr());
}

#[tokio::test]
async fn proxy_header_from_anyone_else_is_refused() {
    let s = start_with(LOOPBACK, "trusted_proxies = []", Tuning::default()).await;
    let mut c = Client::tcp(&s).await;
    let mut first = proxy_header("198.51.100.7:4242".parse().unwrap(), s.addr());
    first.extend_from_slice(&Builder::new(method::BINDING, Class::Request, tx()).finish(None));
    c.send(&first).await;
    assert!(c.recv_within(WAIT).await.is_none(), "closed without an answer");
}

#[tokio::test]
async fn idle_tcp_connections_are_closed() {
    let tuning = Tuning { tcp_first_message: Duration::from_millis(200), tcp_idle: Duration::from_millis(400), ..Tuning::default() };
    let s = start_with(LOOPBACK, "", tuning).await;

    let mut silent = Client::tcp(&s).await;
    assert!(silent.recv_within(WAIT).await.is_none(), "no first message: closed");

    let mut idle = Client::tcp(&s).await;
    idle.call(Builder::new(method::BINDING, Class::Request, tx()).finish(None)).await;
    let t0 = tokio::time::Instant::now();
    assert!(idle.recv_within(WAIT).await.is_none(), "no allocation: closed when idle");
    assert!(t0.elapsed() < Duration::from_secs(2));

    let mut busy = Client::tcp(&s).await;
    busy.relayed().await;
    assert!(busy.recv_within(Duration::from_millis(1000)).await.is_none(), "nothing to read…");
    assert_eq!(busy.refresh(600).await, None, "…but the connection with an allocation stays open");
}

// ---------------------------------------------------------------------------
// With the HTTP side
// ---------------------------------------------------------------------------

#[tokio::test]
async fn credentials_from_turn_endpoint_work() {
    let cfg = config(LOOPBACK, "");
    let app = Arc::new(App::new(cfg.clone(), vec![b"test-session-secret-0123456789abcdef".to_vec()], Some(SECRET.to_vec())));
    let turn = TurnServer::start(&cfg, SECRET.to_vec(), app.turn_stats()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = listener.local_addr().unwrap();
    tokio::spawn(handshake::serve(listener, app, std::future::pending()));

    let client = reqwest::Client::new();
    let session: serde_json::Value = client
        .post(format!("http://{http}/session"))
        .header("origin", "https://game.test")
        .json(&serde_json::json!({ "app": "game", "version": 1 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(session["turn"], true);
    let creds: serde_json::Value = client
        .post(format!("http://{http}/turn"))
        .bearer_auth(session["token"].as_str().unwrap())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ice = &creds["ice_servers"][0];

    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut c = Client::new(Transport::Udp(sock, turn.local_addr()));
    c.user = ice["username"].as_str().unwrap().to_string();
    c.pass = ice["credential"].as_str().unwrap().to_string();
    c.allocate().await;

    let metrics = client.get(format!("http://{http}/metrics")).send().await.unwrap().text().await.unwrap();
    assert!(metrics.contains("handshake_turn_allocations 1"), "{metrics}");
    assert!(metrics.contains("handshake_turn_allocations_total 1"), "{metrics}");
}
