//! The TURN server's sockets and request handling: one UDP socket and one TCP listener on `[turn].listen`, a UDP
//! relay socket per allocation, a sweeper for expiry and a resolver for a hostname `external_ip`.
//! Protocol rules: docs/specs/builtin-turn.md "Protocol".

use std::{
    collections::{HashMap, HashSet},
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rand::RngCore;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::mpsc,
    task::{AbortHandle, JoinHandle},
    time::timeout,
};
use tracing::{debug, info, warn};

use super::{
    allocation::{self, peer_allowed, v4, PortPool, State},
    auth::{Auth, NonceCheck, REALM},
    proxy::{self, Proxy},
    stun::{self, attr, channel_data, method, parse_channel_data, Builder, Class, Frame, Message},
    Stats,
};
use crate::{Cidr, Config};

const RESOLVE_EVERY: Duration = Duration::from_secs(60);
const TCP_QUEUE: usize = 256;
/// The most a TCP client may have buffered: one maximal STUN message or ChannelData frame.
const MAX_TCP_BUFFER: usize = 20 + 65_535 + 4;

/// Timings the tests shorten. [`Tuning::default`] is what the server runs with.
#[doc(hidden)]
#[derive(Clone)]
pub struct Tuning {
    pub nonce_secs: u64,
    /// A TCP connection must complete its first message within this.
    pub tcp_first_message: Duration,
    /// A TCP connection without an allocation is closed after this long without a message.
    pub tcp_idle: Duration,
    pub sweep_every: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            nonce_secs: 600,
            tcp_first_message: Duration::from_secs(10),
            tcp_idle: Duration::from_secs(30),
            sweep_every: Duration::from_secs(5),
        }
    }
}

/// A running TURN server. Dropping it stops the listeners and frees every allocation.
pub struct TurnServer {
    addr: SocketAddr,
    shared: Arc<Shared>,
    tasks: Vec<JoinHandle<()>>,
}

impl TurnServer {
    /// Bind `[turn].listen` (UDP and TCP on one port; port 0 picks a free one) and start serving.
    pub async fn start(cfg: &Config, secret: Vec<u8>, stats: Arc<Stats>) -> io::Result<TurnServer> {
        Self::start_tuned(cfg, secret, stats, Tuning::default()).await
    }

    #[doc(hidden)]
    pub async fn start_tuned(cfg: &Config, secret: Vec<u8>, stats: Arc<Stats>, tuning: Tuning) -> io::Result<TurnServer> {
        let invalid = |msg: &str| io::Error::new(io::ErrorKind::InvalidInput, msg.to_string());
        let tc = cfg.turn.as_ref().ok_or_else(|| invalid("[turn] is missing"))?;
        let range = tc.relay_ports.ok_or_else(|| invalid("[turn] relay_ports is missing"))?;
        let external = tc.external_ip.clone().ok_or_else(|| invalid("[turn] external_ip is required with relay_ports"))?;
        let listen: SocketAddr = tc.listen.parse().map_err(|_| invalid("[turn] listen is not an address and port"))?;
        let external_now = resolve(&external).await?;
        let (udp, tcp) = bind_pair(listen).await?;
        let addr = udp.local_addr()?;

        let apps: HashSet<String> = cfg.apps.iter().filter(|(_, a)| a.turn).map(|(id, _)| id.clone()).collect();
        let shared = Arc::new(Shared {
            auth: Auth::new(secret, apps, tuning.nonce_secs),
            max_allocations: tc.max_allocations,
            allocations_per_ip: tc.allocations_per_ip,
            kbps: tc.kbps_per_allocation,
            allowed_peers: tc.allowed_peers.clone(),
            trusted: cfg.trusted_proxies.clone(),
            tuning: tuning.clone(),
            external: Mutex::new(external_now),
            udp: Arc::new(udp),
            table: Mutex::new(Table { allocs: HashMap::new(), by_port: HashMap::new(), ports: PortPool::new(range) }),
            stats,
            next_tcp: AtomicU64::new(1),
        });

        let mut tasks = vec![
            tokio::spawn(udp_loop(shared.clone())),
            tokio::spawn(tcp_loop(shared.clone(), tcp)),
            tokio::spawn(sweeper(Arc::downgrade(&shared))),
        ];
        if external.parse::<Ipv4Addr>().is_err() {
            tasks.push(tokio::spawn(resolver(Arc::downgrade(&shared), external)));
        }
        info!(%addr, relay_ports = %range, external_ip = %external_now, "TURN listening");
        Ok(TurnServer { addr, shared, tasks })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for TurnServer {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        let mut table = self.shared.table.lock().unwrap();
        let keys: Vec<Key> = table.allocs.keys().copied().collect();
        for key in keys {
            self.shared.remove(&mut table, key);
        }
    }
}

/// UDP and TCP on the same port. For port 0, retry until the port the UDP socket got is free for TCP too.
async fn bind_pair(listen: SocketAddr) -> io::Result<(UdpSocket, TcpListener)> {
    for _ in 0..20 {
        let udp = UdpSocket::bind(listen).await?;
        let addr = SocketAddr::new(listen.ip(), udp.local_addr()?.port());
        match TcpListener::bind(addr).await {
            Ok(tcp) => return Ok((udp, tcp)),
            Err(e) if listen.port() != 0 => return Err(e),
            Err(_) => continue,
        }
    }
    Err(io::Error::new(io::ErrorKind::AddrInUse, "no port free for both UDP and TCP"))
}

/// `external_ip` as an IPv4 address: a literal, or the first IPv4 address the hostname resolves to.
async fn resolve(external: &str) -> io::Result<Ipv4Addr> {
    if let Ok(ip) = external.parse::<Ipv4Addr>() {
        return Ok(ip);
    }
    tokio::net::lookup_host((external, 0))
        .await?
        .find_map(|a| v4(a.ip()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("external_ip {external:?} has no IPv4 address")))
}

async fn resolver(shared: Weak<Shared>, host: String) {
    let mut tick = tokio::time::interval(RESOLVE_EVERY);
    tick.tick().await; // resolved at startup already
    loop {
        tick.tick().await;
        let Some(sh) = shared.upgrade() else { return };
        match resolve(&host).await {
            Ok(ip) => {
                let mut ext = sh.external.lock().unwrap();
                if *ext != ip {
                    info!(external_ip = %ip, host, "TURN external address changed");
                    *ext = ip;
                }
            }
            Err(e) => warn!(host, error = %e, "cannot resolve TURN external_ip; keeping the last address"),
        }
    }
}

async fn sweeper(shared: Weak<Shared>) {
    let every = match shared.upgrade() {
        Some(sh) => sh.tuning.sweep_every,
        None => return,
    };
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        let Some(sh) = shared.upgrade() else { return };
        let now = Instant::now();
        let mut table = sh.table.lock().unwrap();
        let mut expired = Vec::new();
        for (key, a) in &table.allocs {
            let mut st = a.state.lock().unwrap();
            if st.expires <= now {
                expired.push(*key);
            } else {
                st.expire(now);
            }
        }
        for key in expired {
            sh.remove(&mut table, key);
        }
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct Shared {
    auth: Auth,
    max_allocations: usize,
    allocations_per_ip: usize,
    kbps: u32,
    allowed_peers: Vec<Cidr>,
    trusted: Vec<Cidr>,
    tuning: Tuning,
    external: Mutex<Ipv4Addr>,
    udp: Arc<UdpSocket>,
    table: Mutex<Table>, // lock order: table, then an allocation's state
    stats: Arc<Stats>,
    next_tcp: AtomicU64,
}

struct Table {
    allocs: HashMap<Key, Arc<Alloc>>,
    by_port: HashMap<u16, Arc<Alloc>>,
    ports: PortPool,
}

/// What identifies a client's allocation: its UDP address, or its TCP connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Key {
    Udp(SocketAddr),
    Tcp(u64),
}

/// How to reach a client.
#[derive(Clone)]
enum Sink {
    Udp(Arc<UdpSocket>, SocketAddr),
    Tcp(mpsc::Sender<Vec<u8>>),
}

impl Sink {
    /// Never waits: a full TCP queue or socket buffer drops the packet, as UDP would.
    fn send(&self, bytes: Vec<u8>) {
        match self {
            Sink::Udp(sock, to) => {
                let _ = sock.try_send_to(&bytes, *to);
            }
            Sink::Tcp(tx) => {
                let _ = tx.try_send(bytes);
            }
        }
    }

    fn is_tcp(&self) -> bool {
        matches!(self, Sink::Tcp(_))
    }
}

struct Alloc {
    client: SocketAddr,
    username: String,
    relay: Arc<UdpSocket>,
    port: u16,
    sink: Sink,
    state: Mutex<State>,
    reader: Mutex<Option<AbortHandle>>,
}

impl Drop for Alloc {
    fn drop(&mut self) {
        if let Some(r) = self.reader.get_mut().unwrap().take() {
            r.abort();
        }
    }
}

/// Where a request came from and how to answer it.
struct Ctx {
    key: Key,
    /// The client as the server sees it: the socket peer, or the PROXY header's source.
    client: SocketAddr,
    sink: Sink,
}

fn canon(addr: SocketAddr) -> SocketAddr {
    v4(addr.ip()).map_or(addr, |ip| SocketAddr::new(IpAddr::V4(ip), addr.port()))
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn random_tx() -> stun::TxId {
    let mut tx = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut tx);
    tx
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

impl Shared {
    fn alloc(&self, key: Key) -> Option<Arc<Alloc>> {
        self.table.lock().unwrap().allocs.get(&key).cloned()
    }

    fn remove(&self, table: &mut Table, key: Key) {
        if let Some(a) = table.allocs.remove(&key) {
            table.by_port.remove(&a.port);
            table.ports.release(a.port);
            self.stats.allocations.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// A STUN message from a client.
    fn on_stun(self: &Arc<Self>, ctx: &Ctx, buf: &[u8]) {
        let Some(msg) = Message::parse(buf) else { return };
        match msg.class {
            Class::Request => {
                if let Some(resp) = self.request(&msg, ctx) {
                    ctx.sink.send(resp);
                }
            }
            Class::Indication if msg.method == method::SEND => self.send_indication(&msg, ctx.key),
            _ => {}
        }
    }

    fn request(self: &Arc<Self>, msg: &Message, ctx: &Ctx) -> Option<Vec<u8>> {
        let unknown = msg.unknown_required();
        if !unknown.is_empty() {
            debug!(client = %ctx.client, method = msg.method, ?unknown, "TURN request with unknown attributes");
            let list: Vec<u8> = unknown.iter().flat_map(|t| t.to_be_bytes()).collect();
            return Some(
                Builder::new(msg.method, Class::Error, msg.tx)
                    .error(420, "Unknown Attribute")
                    .attr(attr::UNKNOWN_ATTRIBUTES, &list)
                    .finish(None),
            );
        }
        if msg.method == method::BINDING {
            return Some(Builder::new(method::BINDING, Class::Success, msg.tx).xor_addr(attr::XOR_MAPPED_ADDRESS, ctx.client).finish(None));
        }
        let (username, key) = match self.authenticate(msg, ctx.client.ip()) {
            Ok(user) => user,
            Err(resp) => return Some(resp),
        };
        let reply = Reply { msg, key: &key };
        Some(match msg.method {
            method::ALLOCATE => self.allocate(msg, ctx, username, key),
            method::REFRESH => self.refresh(msg, ctx, &username, &reply),
            method::CREATE_PERMISSION => self.create_permission(msg, ctx, &username, &reply),
            method::CHANNEL_BIND => self.channel_bind(msg, ctx, &username, &reply),
            _ => reply.error(400, "Bad Request"),
        })
    }

    /// Long-term credentials: `Ok((username, key))`, or the error response to send.
    fn authenticate(&self, msg: &Message, ip: IpAddr) -> Result<(String, [u8; 16]), Vec<u8>> {
        let now = now_unix();
        let challenge = |code: u16, reason: &str| {
            Builder::new(msg.method, Class::Error, msg.tx)
                .error(code, reason)
                .attr(attr::REALM, REALM.as_bytes())
                .attr(attr::NONCE, self.auth.nonce(ip, now).as_bytes())
                .finish(None)
        };
        if !msg.has_integrity() {
            return Err(challenge(401, "Unauthorized"));
        }
        let fail = |code, reason| {
            self.stats.auth_failures.fetch_add(1, Ordering::Relaxed);
            challenge(code, reason)
        };
        let (Some(user), Some(realm), Some(nonce)) = (msg.get(attr::USERNAME), msg.get(attr::REALM), msg.get(attr::NONCE)) else {
            return Err(Builder::new(msg.method, Class::Error, msg.tx).error(400, "Bad Request").finish(None));
        };
        match self.auth.check_nonce(nonce, ip, now) {
            NonceCheck::Valid => {}
            NonceCheck::Stale => return Err(challenge(438, "Stale Nonce")),
            NonceCheck::Bad => return Err(fail(401, "Unauthorized")),
        }
        let user = std::str::from_utf8(user).map_err(|_| fail(401, "Unauthorized"))?;
        let key = match self.auth.user_key(user, now) {
            Some(key) if realm == REALM.as_bytes() && msg.check_integrity(&key) => key,
            _ => return Err(fail(401, "Unauthorized")),
        };
        Ok((user.to_string(), key))
    }

    fn allocate(self: &Arc<Self>, msg: &Message, ctx: &Ctx, username: String, key: [u8; 16]) -> Vec<u8> {
        let reply = Reply { msg, key: &key };
        let mut table = self.table.lock().unwrap();
        if let Some(existing) = table.allocs.get(&ctx.key) {
            // A retransmission of the request that made it gets the same answer; anything else is a mismatch.
            if let Some((tx, resp)) = &existing.state.lock().unwrap().last {
                if *tx == msg.tx {
                    return resp.clone();
                }
            }
            return reply.error(437, "Allocation Mismatch");
        }
        match msg.get(attr::REQUESTED_TRANSPORT) {
            Some([17, ..]) => {}
            Some(_) => return reply.error(442, "Unsupported Transport Protocol"),
            None => return reply.error(400, "Bad Request"),
        }
        if msg.get(attr::REQUESTED_ADDRESS_FAMILY).is_some_and(|v| v.first() != Some(&1)) {
            return reply.error(440, "Address Family not Supported");
        }
        // EVEN-PORT: an even relay port. With the R bit it also asks to reserve the next one, which this server does not do.
        let even = match msg.get(attr::EVEN_PORT) {
            None => false,
            Some([r, ..]) if r & 0x80 != 0 => return reply.error(508, "Insufficient Capacity"),
            Some(_) => true,
        };
        let same_ip = table.allocs.values().filter(|a| a.client.ip() == ctx.client.ip()).count();
        if same_ip >= self.allocations_per_ip {
            self.stats.quota_rejections.fetch_add(1, Ordering::Relaxed);
            return reply.error(486, "Allocation Quota Reached");
        }
        if table.allocs.len() >= self.max_allocations {
            self.stats.quota_rejections.fetch_add(1, Ordering::Relaxed);
            return reply.error(508, "Insufficient Capacity");
        }
        let Some((sock, port)) = table.ports.bind(even) else {
            self.stats.quota_rejections.fetch_add(1, Ordering::Relaxed);
            return reply.error(508, "Insufficient Capacity");
        };
        let relay = match UdpSocket::from_std(sock) {
            Ok(r) => Arc::new(r),
            Err(_) => {
                table.ports.release(port);
                return reply.error(508, "Insufficient Capacity");
            }
        };
        let lifetime = allocation::lifetime(lifetime_attr(msg));
        let external = *self.external.lock().unwrap();
        let resp = Builder::new(method::ALLOCATE, Class::Success, msg.tx)
            .xor_addr(attr::XOR_RELAYED_ADDRESS, SocketAddr::new(external.into(), port))
            .xor_addr(attr::XOR_MAPPED_ADDRESS, ctx.client)
            .attr(attr::LIFETIME, &(lifetime.as_secs() as u32).to_be_bytes())
            .finish(Some(&key));
        let mut state = State::new(lifetime, self.kbps, Instant::now());
        state.last = Some((msg.tx, resp.clone()));
        let alloc = Arc::new(Alloc {
            client: ctx.client,
            username,
            relay: relay.clone(),
            port,
            sink: ctx.sink.clone(),
            state: Mutex::new(state),
            reader: Mutex::new(None),
        });
        let reader = tokio::spawn(relay_reader(Arc::downgrade(&alloc), relay, self.stats.clone()));
        *alloc.reader.lock().unwrap() = Some(reader.abort_handle());
        table.allocs.insert(ctx.key, alloc.clone());
        table.by_port.insert(port, alloc);
        self.stats.allocations.fetch_add(1, Ordering::Relaxed);
        self.stats.allocations_total.fetch_add(1, Ordering::Relaxed);
        resp
    }

    /// The caller's allocation, or the error to answer with: 437 without one, 441 when it belongs to another username.
    fn own_alloc(&self, ctx: &Ctx, username: &str, reply: &Reply) -> Result<Arc<Alloc>, Vec<u8>> {
        let alloc = self.alloc(ctx.key).ok_or_else(|| reply.error(437, "Allocation Mismatch"))?;
        if alloc.username != username {
            return Err(reply.error(441, "Wrong Credentials"));
        }
        Ok(alloc)
    }

    fn refresh(&self, msg: &Message, ctx: &Ctx, username: &str, reply: &Reply) -> Vec<u8> {
        let alloc = match self.own_alloc(ctx, username, reply) {
            Ok(a) => a,
            Err(resp) => return resp,
        };
        let requested = lifetime_attr(msg);
        if requested == Some(0) {
            let mut table = self.table.lock().unwrap();
            self.remove(&mut table, ctx.key);
            return reply.success(&[(attr::LIFETIME, &0u32.to_be_bytes())]);
        }
        let mut st = alloc.state.lock().unwrap();
        if let Some((tx, resp)) = &st.last {
            if *tx == msg.tx {
                return resp.clone();
            }
        }
        let lifetime = allocation::lifetime(requested);
        st.expires = Instant::now() + lifetime;
        let resp = reply.success(&[(attr::LIFETIME, &(lifetime.as_secs() as u32).to_be_bytes())]);
        st.last = Some((msg.tx, resp.clone()));
        resp
    }

    /// May the relay send to `peer`? Its own relay addresses are always fine (delivered in-process).
    fn peer_ok(&self, peer: SocketAddr) -> Result<(), (u16, &'static str)> {
        if peer.is_ipv6() && v4(peer.ip()).is_none() {
            return Err((443, "Peer Address Family Mismatch"));
        }
        let own = v4(peer.ip()) == Some(*self.external.lock().unwrap()) && self.table.lock().unwrap().ports.contains(peer.port());
        if own || peer_allowed(peer.ip(), &self.allowed_peers) {
            Ok(())
        } else {
            Err((403, "Forbidden"))
        }
    }

    fn create_permission(&self, msg: &Message, ctx: &Ctx, username: &str, reply: &Reply) -> Vec<u8> {
        let alloc = match self.own_alloc(ctx, username, reply) {
            Ok(a) => a,
            Err(resp) => return resp,
        };
        let peers: Option<Vec<SocketAddr>> = msg.all(attr::XOR_PEER_ADDRESS).map(|v| stun::xor_addr(v, &msg.tx)).collect();
        let Some(peers) = peers.filter(|p| !p.is_empty()) else {
            return reply.error(400, "Bad Request");
        };
        for peer in &peers {
            if let Err((code, reason)) = self.peer_ok(*peer) {
                return reply.error(code, reason);
            }
        }
        let now = Instant::now();
        let mut st = alloc.state.lock().unwrap();
        for peer in peers {
            st.permit(peer.ip(), now);
        }
        reply.success(&[])
    }

    fn channel_bind(&self, msg: &Message, ctx: &Ctx, username: &str, reply: &Reply) -> Vec<u8> {
        let alloc = match self.own_alloc(ctx, username, reply) {
            Ok(a) => a,
            Err(resp) => return resp,
        };
        let ch = msg.get(attr::CHANNEL_NUMBER).filter(|v| v.len() == 4).map(|v| u16::from_be_bytes([v[0], v[1]]));
        let peer = msg.get(attr::XOR_PEER_ADDRESS).and_then(|v| stun::xor_addr(v, &msg.tx));
        let (Some(ch), Some(peer)) = (ch.filter(|c| (0x4000..=0x4FFF).contains(c)), peer) else {
            return reply.error(400, "Bad Request");
        };
        if let Err((code, reason)) = self.peer_ok(peer) {
            return reply.error(code, reason);
        }
        if !alloc.state.lock().unwrap().bind(ch, canon(peer), Instant::now()) {
            return reply.error(400, "Bad Request");
        }
        reply.success(&[])
    }

    // ---- data ------------------------------------------------------------

    /// A Send indication: client → peer, if the peer has a permission.
    fn send_indication(&self, msg: &Message, key: Key) {
        let (Some(peer), Some(data)) = (msg.get(attr::XOR_PEER_ADDRESS).and_then(|v| stun::xor_addr(v, &msg.tx)), msg.get(attr::DATA)) else {
            return;
        };
        let Some(alloc) = self.alloc(key) else { return };
        let peer = canon(peer);
        {
            let now = Instant::now();
            let mut st = alloc.state.lock().unwrap();
            if !st.permitted(peer.ip(), now) || !st.up.take(data.len(), now) {
                return;
            }
        }
        self.relay_out(&alloc, peer, data);
    }

    /// ChannelData from a client: client → the channel's peer.
    fn on_channel_data(&self, key: Key, buf: &[u8]) {
        let Some((ch, data)) = parse_channel_data(buf) else { return };
        let Some(alloc) = self.alloc(key) else { return };
        let peer = {
            let now = Instant::now();
            let mut st = alloc.state.lock().unwrap();
            match st.channel_peer(ch, now) {
                Some(peer) if st.up.take(data.len(), now) => peer,
                _ => return,
            }
        };
        self.relay_out(&alloc, peer, data);
    }

    /// Send from `alloc`'s relay address to `peer`. A peer that is one of this server's own relay addresses gets the
    /// data in-process: no trip out and back through the router (which may not hairpin its WAN address).
    fn relay_out(&self, alloc: &Alloc, peer: SocketAddr, data: &[u8]) {
        self.stats.bytes_out.fetch_add(data.len() as u64, Ordering::Relaxed);
        let external = *self.external.lock().unwrap();
        if v4(peer.ip()) == Some(external) {
            let target = self.table.lock().unwrap().by_port.get(&peer.port()).cloned();
            if let Some(target) = target {
                on_peer_data(&target, SocketAddr::new(external.into(), alloc.port), data, &self.stats);
                return;
            }
        }
        let _ = alloc.relay.try_send_to(data, peer);
    }
}

/// Builds responses to one authenticated request.
struct Reply<'a> {
    msg: &'a Message<'a>,
    key: &'a [u8; 16],
}

impl Reply<'_> {
    fn error(&self, code: u16, reason: &str) -> Vec<u8> {
        Builder::new(self.msg.method, Class::Error, self.msg.tx).error(code, reason).finish(Some(self.key))
    }

    fn success(&self, attrs: &[(u16, &[u8])]) -> Vec<u8> {
        let mut b = Builder::new(self.msg.method, Class::Success, self.msg.tx);
        for (t, v) in attrs {
            b = b.attr(*t, v);
        }
        b.finish(Some(self.key))
    }
}

fn lifetime_attr(msg: &Message) -> Option<u32> {
    msg.get(attr::LIFETIME).filter(|v| v.len() == 4).map(|v| u32::from_be_bytes([v[0], v[1], v[2], v[3]]))
}

/// Data that reached `alloc`'s relay address from `from`: to the client if `from` has a permission, as ChannelData
/// when it has a channel, else as a Data indication.
fn on_peer_data(alloc: &Alloc, from: SocketAddr, data: &[u8], stats: &Stats) {
    let from = canon(from);
    let channel = {
        let now = Instant::now();
        let mut st = alloc.state.lock().unwrap();
        if !st.permitted(from.ip(), now) || !st.down.take(data.len(), now) {
            return;
        }
        st.peer_channel(from, now)
    };
    let frame = match channel {
        Some(ch) => channel_data(ch, data, alloc.sink.is_tcp()),
        None => Builder::new(method::DATA, Class::Indication, random_tx())
            .xor_addr(attr::XOR_PEER_ADDRESS, from)
            .attr(attr::DATA, data)
            .finish(None),
    };
    stats.bytes_in.fetch_add(data.len() as u64, Ordering::Relaxed);
    alloc.sink.send(frame);
}

async fn relay_reader(alloc: Weak<Alloc>, relay: Arc<UdpSocket>, stats: Arc<Stats>) {
    let mut buf = vec![0u8; 65_536];
    loop {
        // Errors here are ICMP reports for earlier sends (a peer port that was closed); keep reading.
        let Ok((n, from)) = relay.recv_from(&mut buf).await else { continue };
        let Some(alloc) = alloc.upgrade() else { return };
        on_peer_data(&alloc, from, &buf[..n], &stats);
    }
}

// ---------------------------------------------------------------------------
// Transports
// ---------------------------------------------------------------------------

async fn udp_loop(sh: Arc<Shared>) {
    let mut buf = vec![0u8; 65_536];
    loop {
        let Ok((n, from)) = sh.udp.recv_from(&mut buf).await else { continue };
        let from = canon(from);
        let pkt = &buf[..n];
        match pkt.first().map(|b| b >> 6) {
            Some(0) => sh.on_stun(&Ctx { key: Key::Udp(from), client: from, sink: Sink::Udp(sh.udp.clone(), from) }, pkt),
            Some(1) => sh.on_channel_data(Key::Udp(from), pkt),
            _ => {}
        }
    }
}

async fn tcp_loop(sh: Arc<Shared>, listener: TcpListener) {
    loop {
        let Ok((stream, peer)) = listener.accept().await else { continue };
        let id = sh.next_tcp.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(tcp_conn(sh.clone(), stream, canon(peer), id));
    }
}

async fn tcp_conn(sh: Arc<Shared>, stream: TcpStream, peer: SocketAddr, id: u64) {
    let _ = stream.set_nodelay(true);
    let (mut rd, mut wr) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(TCP_QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if wr.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });
    let key = Key::Tcp(id);
    let mut ctx = Ctx { key, client: peer, sink: Sink::Tcp(tx) };
    // Only a trusted proxy may say who the client is, and it may also connect without saying (LAN clients).
    let mut proxy_pending = sh.trusted.iter().any(|c| c.contains(peer.ip()));
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut first = true;
    'conn: loop {
        loop {
            if proxy_pending {
                match buf.first() {
                    None => break,
                    Some(&b) if b == proxy::SIGNATURE[0] => match proxy::parse_v2(&buf) {
                        Proxy::Need => break,
                        Proxy::Bad => break 'conn,
                        Proxy::Header { src, len } => {
                            if let Some(src) = src {
                                ctx.client = canon(src);
                            }
                            buf.drain(..len);
                        }
                    },
                    Some(_) => {}
                }
                proxy_pending = false;
            }
            match stun::frame(&buf) {
                Frame::Need => break,
                Frame::Bad => break 'conn,
                Frame::Complete(n) => {
                    let frame: Vec<u8> = buf.drain(..n).collect();
                    first = false;
                    if frame[0] >> 6 == 0 {
                        sh.on_stun(&ctx, &frame);
                    } else {
                        sh.on_channel_data(key, &frame);
                    }
                }
            }
        }
        if buf.len() > MAX_TCP_BUFFER {
            break;
        }
        let wait = if first { sh.tuning.tcp_first_message } else { sh.tuning.tcp_idle };
        buf.reserve(4096);
        match timeout(wait, rd.read_buf(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => break,
            Ok(Ok(_)) => {}
            Err(_) if first => break,
            Err(_) => {
                // Idle: fine while the connection holds an allocation, which the sweeper ends when it expires.
                if sh.alloc(key).is_none() {
                    break;
                }
            }
        }
    }
    let mut table = sh.table.lock().unwrap();
    sh.remove(&mut table, key);
    drop(table);
    writer.abort();
}
