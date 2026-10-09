//! Per-allocation state (permissions, channels, lifetime, bandwidth) and the rules around it: lifetimes, which peers
//! the relay may reach, and the relay port pool. No sockets here except binding relay ports.

use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    time::{Duration, Instant},
};

use super::stun::TxId;
use crate::{Cidr, PortRange};

pub const DEFAULT_LIFETIME: Duration = Duration::from_secs(600);
pub const MAX_LIFETIME: Duration = Duration::from_secs(3600);
pub const PERMISSION_LIFETIME: Duration = Duration::from_secs(300);
pub const CHANNEL_LIFETIME: Duration = Duration::from_secs(600);

/// The lifetime to grant for a requested LIFETIME (seconds). RFC 8656: less than the default means the default.
/// A Refresh asking for 0 deletes the allocation before this is consulted.
pub fn lifetime(requested: Option<u32>) -> Duration {
    requested.map_or(DEFAULT_LIFETIME, |s| Duration::from_secs(s as u64).clamp(DEFAULT_LIFETIME, MAX_LIFETIME))
}

/// IPv4 normalised: IPv4-mapped IPv6 becomes IPv4.
pub fn v4(ip: IpAddr) -> Option<Ipv4Addr> {
    match ip {
        IpAddr::V4(v4) => Some(v4),
        IpAddr::V6(v6) => v6.to_ipv4_mapped(),
    }
}

/// Whether the relay may send to `ip`. Relays carry IPv4 only, and never into loopback, private, carrier-grade NAT,
/// link-local (cloud metadata lives there), special-use (IETF protocol assignments, documentation, benchmarking, 6to4
/// relay), multicast, broadcast or reserved space unless `allowed` lists it: otherwise a TURN user could reach the
/// server's own network, or one that routes those ranges internally (fake-IP proxies use 198.18/15).
pub fn peer_allowed(ip: IpAddr, allowed: &[Cidr]) -> bool {
    let Some(ip) = v4(ip) else {
        return false;
    };
    if allowed.iter().any(|c| c.contains(IpAddr::V4(ip))) {
        return true;
    }
    let [a, b, c, _] = ip.octets();
    let forbidden = ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || a == 0
        || a >= 240
        || (a == 100 && (64..128).contains(&b))
        || (a, b, c) == (192, 0, 0)
        || (a, b, c) == (192, 0, 2)
        || (a, b, c) == (192, 88, 99)
        || (a, b, c) == (198, 51, 100)
        || (a, b, c) == (203, 0, 113)
        || (a == 198 && (b == 18 || b == 19));
    !forbidden
}

/// The smallest burst a [`Bucket`] allows: one maximal UDP payload, so a low rate slows traffic down instead of
/// dropping every packet bigger than a second's worth.
const MIN_BURST: f64 = 65_535.0;

/// A token bucket in bytes: a second's worth of burst at `kbps`, and never less than [`MIN_BURST`].
pub struct Bucket {
    rate: f64, // bytes per second
    burst: f64,
    tokens: f64,
    at: Instant,
}

impl Bucket {
    pub fn new(kbps: u32, now: Instant) -> Self {
        let rate = kbps as f64 * 125.0;
        let burst = rate.max(MIN_BURST);
        Bucket { rate, burst, tokens: burst, at: now }
    }

    /// Spend `bytes` if the bucket holds them.
    pub fn take(&mut self, bytes: usize, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.burst);
        self.at = now;
        if self.tokens >= bytes as f64 {
            self.tokens -= bytes as f64;
            true
        } else {
            false
        }
    }
}

/// The mutable part of one allocation.
pub struct State {
    pub expires: Instant,
    /// The last Allocate/Refresh transaction and its response, resent for a retransmission.
    pub last: Option<(TxId, Vec<u8>)>,
    /// Client → peer.
    pub up: Bucket,
    /// Peer → client.
    pub down: Bucket,
    permissions: HashMap<Ipv4Addr, Instant>,
    channels: HashMap<u16, (SocketAddr, Instant)>,
}

impl State {
    pub fn new(lifetime: Duration, kbps: u32, now: Instant) -> Self {
        State {
            expires: now + lifetime,
            last: None,
            up: Bucket::new(kbps, now),
            down: Bucket::new(kbps, now),
            permissions: HashMap::new(),
            channels: HashMap::new(),
        }
    }

    /// False once the lifetime is over, even before the sweeper removes the allocation: it then neither relays nor
    /// accepts requests.
    pub fn live(&self, now: Instant) -> bool {
        self.expires > now
    }

    /// Install or refresh a permission for `ip` (port is not part of a permission).
    pub fn permit(&mut self, ip: IpAddr, now: Instant) {
        if let Some(ip) = v4(ip) {
            self.permissions.insert(ip, now + PERMISSION_LIFETIME);
        }
    }

    pub fn permitted(&self, ip: IpAddr, now: Instant) -> bool {
        v4(ip).and_then(|ip| self.permissions.get(&ip)).is_some_and(|until| *until > now)
    }

    /// Bind channel `ch` to `peer` (or refresh the binding), which also installs a permission. False when `ch` is bound
    /// to another peer or `peer` to another channel.
    pub fn bind(&mut self, ch: u16, peer: SocketAddr, now: Instant) -> bool {
        if self.channel_peer(ch, now).is_some_and(|p| p != peer) || self.peer_channel(peer, now).is_some_and(|c| c != ch) {
            return false;
        }
        self.channels.insert(ch, (peer, now + CHANNEL_LIFETIME));
        self.permit(peer.ip(), now);
        true
    }

    pub fn channel_peer(&self, ch: u16, now: Instant) -> Option<SocketAddr> {
        self.channels.get(&ch).filter(|(_, until)| *until > now).map(|(peer, _)| *peer)
    }

    pub fn peer_channel(&self, peer: SocketAddr, now: Instant) -> Option<u16> {
        self.channels.iter().find(|(_, (p, until))| *p == peer && *until > now).map(|(ch, _)| *ch)
    }

    /// Drop expired permissions and channels.
    pub fn expire(&mut self, now: Instant) {
        self.permissions.retain(|_, until| *until > now);
        self.channels.retain(|_, (_, until)| *until > now);
    }
}

/// Relay ports in use, handed out from `relay_ports`.
pub struct PortPool {
    range: PortRange,
    used: HashSet<u16>,
    next: u16,
}

impl PortPool {
    pub fn new(range: PortRange) -> Self {
        PortPool { range, used: HashSet::new(), next: range.first }
    }

    pub fn contains(&self, port: u16) -> bool {
        (self.range.first..=self.range.last).contains(&port)
    }

    /// Bind a non-blocking UDP socket on a free port of the range (an even one if `even`, for EVEN-PORT), or `None`
    /// when every such port is taken.
    pub fn bind(&mut self, even: bool) -> Option<(UdpSocket, u16)> {
        let size = (self.range.last - self.range.first) as u32 + 1;
        for _ in 0..size {
            let port = self.next;
            self.next = if port == self.range.last { self.range.first } else { port + 1 };
            if self.used.contains(&port) || (even && port % 2 == 1) {
                continue;
            }
            // Another process may hold the port; then try the next.
            if let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)) {
                if sock.set_nonblocking(true).is_ok() {
                    self.used.insert(port);
                    return Some((sock, port));
                }
            }
        }
        None
    }

    pub fn release(&mut self, port: u16) {
        self.used.remove(&port);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn lifetimes() {
        let s = |n| Duration::from_secs(n);
        assert_eq!(lifetime(None), s(600));
        assert_eq!(lifetime(Some(30)), s(600), "below the default means the default");
        assert_eq!(lifetime(Some(1200)), s(1200));
        assert_eq!(lifetime(Some(u32::MAX)), s(3600));
    }

    #[test]
    fn forbidden_peers() {
        for bad in [
            "127.0.0.1", "10.1.2.3", "172.16.0.1", "172.31.255.255", "192.168.1.10", "100.64.0.1", "100.127.255.255",
            "169.254.169.254", "224.0.0.1", "255.255.255.255", "0.0.0.0", "0.1.2.3", "240.0.0.1", "::1", "2001:db8::1",
            "::ffff:10.0.0.1", "192.0.0.8", "192.0.2.1", "192.88.99.1", "198.18.0.5", "198.19.255.255", "198.51.100.1",
            "203.0.113.5",
        ] {
            assert!(!peer_allowed(ip(bad), &[]), "{bad} must be forbidden");
        }
        for good in [
            "8.8.8.8", "1.1.1.1", "100.63.255.255", "100.128.0.1", "172.32.0.1", "::ffff:8.8.4.4", "192.0.1.1", "198.17.0.1",
            "198.20.0.1", "203.0.114.1",
        ] {
            assert!(peer_allowed(ip(good), &[]), "{good} must be allowed");
        }
        let allowed: Vec<Cidr> = vec!["127.0.0.0/8".parse().unwrap()];
        assert!(peer_allowed(ip("127.0.0.1"), &allowed));
        assert!(!peer_allowed(ip("10.0.0.1"), &allowed));
    }

    #[test]
    fn bucket_bursts_one_second_then_refills() {
        let t0 = Instant::now();
        let mut b = Bucket::new(800, t0); // 800 kbps = 100 000 bytes/s
        assert!(b.take(60_000, t0));
        assert!(b.take(40_000, t0));
        assert!(!b.take(1, t0), "burst used up");
        assert!(b.take(50_000, t0 + Duration::from_millis(500)));
        assert!(!b.take(1, t0 + Duration::from_millis(500)));
        assert!(!b.take(100_001, t0 + Duration::from_secs(10)), "never more than one second's worth");
        assert!(b.take(100_000, t0 + Duration::from_secs(10)));
    }

    #[test]
    fn slow_bucket_still_passes_big_packets() {
        let t0 = Instant::now();
        let mut b = Bucket::new(8, t0); // 1000 bytes/s, but a burst of one maximal datagram
        assert!(b.take(1200, t0), "a WebRTC-sized packet fits the burst");
        assert!(b.take(64_335, t0));
        assert!(!b.take(1, t0));
        assert!(b.take(1000, t0 + Duration::from_secs(1)), "then it refills at the rate");
        assert!(!b.take(1, t0 + Duration::from_secs(1)));
    }

    #[test]
    fn allocations_die_at_their_lifetime() {
        let t0 = Instant::now();
        let st = State::new(DEFAULT_LIFETIME, 1000, t0);
        assert!(st.live(t0 + DEFAULT_LIFETIME - Duration::from_millis(1)));
        assert!(!st.live(t0 + DEFAULT_LIFETIME));
    }

    #[test]
    fn permissions_expire() {
        let t0 = Instant::now();
        let mut st = State::new(DEFAULT_LIFETIME, 1000, t0);
        let peer = ip("203.0.113.5");
        assert!(!st.permitted(peer, t0));
        st.permit(peer, t0);
        assert!(st.permitted(peer, t0 + Duration::from_secs(299)));
        assert!(st.permitted(ip("::ffff:203.0.113.5"), t0), "mapped form is the same peer");
        assert!(!st.permitted(peer, t0 + PERMISSION_LIFETIME));
        st.permit(peer, t0 + Duration::from_secs(200));
        assert!(st.permitted(peer, t0 + Duration::from_secs(450)), "refreshed");
        st.expire(t0 + Duration::from_secs(600));
        assert!(st.permissions.is_empty());
    }

    #[test]
    fn channel_bindings() {
        let t0 = Instant::now();
        let mut st = State::new(DEFAULT_LIFETIME, 1000, t0);
        let (a, b): (SocketAddr, SocketAddr) = ("203.0.113.5:1000".parse().unwrap(), "203.0.113.6:2000".parse().unwrap());
        assert!(st.bind(0x4000, a, t0));
        assert!(st.permitted(a.ip(), t0), "binding installs a permission");
        assert_eq!(st.channel_peer(0x4000, t0), Some(a));
        assert_eq!(st.peer_channel(a, t0), Some(0x4000));
        assert!(st.bind(0x4000, a, t0 + Duration::from_secs(500)), "rebinding the same pair refreshes");
        assert!(!st.bind(0x4000, b, t0), "channel taken by another peer");
        assert!(!st.bind(0x4001, a, t0), "peer has another channel");
        assert!(st.bind(0x4001, b, t0));
        assert_eq!(st.channel_peer(0x4001, t0 + CHANNEL_LIFETIME), None, "expired");
        assert_eq!(st.channel_peer(0x4000, t0 + CHANNEL_LIFETIME), Some(a), "refreshed at +500 s");
        let c: SocketAddr = "203.0.113.7:9".parse().unwrap();
        assert!(st.bind(0x4001, c, t0 + CHANNEL_LIFETIME), "an expired channel is free again");
    }

    #[test]
    fn port_pool() {
        // A high range the OS is unlikely to hand out at the same moment.
        let mut pool = PortPool::new(PortRange { first: 61_111, last: 61_113 });
        let mut got = Vec::new();
        while let Some((sock, port)) = pool.bind(false) {
            assert!(pool.contains(port));
            got.push((sock, port));
        }
        let mut ports: Vec<u16> = got.iter().map(|(_, p)| *p).collect();
        ports.sort();
        assert_eq!(ports, [61_111, 61_112, 61_113]);
        let (sock, port) = got.remove(0); // 61111
        drop(sock);
        pool.release(port);
        assert!(pool.bind(true).is_none(), "the free port is odd");
        assert_eq!(pool.bind(false).map(|(_, p)| p), Some(port));
        assert!(!pool.contains(61_110));
    }
}
