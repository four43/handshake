//! STUN and TURN messages (RFC 8489, RFC 8656) and ChannelData frames: parsing, building, MESSAGE-INTEGRITY,
//! FINGERPRINT and TCP framing. Pure functions; no I/O.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use hmac::{Hmac, Mac};
use sha1::Sha1;

pub const MAGIC: u32 = 0x2112_A442;
const HEADER: usize = 20;
const FINGERPRINT_XOR: u32 = 0x5354_554E;

pub type TxId = [u8; 12];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Request,
    Indication,
    Success,
    Error,
}

pub mod method {
    pub const BINDING: u16 = 0x001;
    pub const ALLOCATE: u16 = 0x003;
    pub const REFRESH: u16 = 0x004;
    pub const SEND: u16 = 0x006;
    pub const DATA: u16 = 0x007;
    pub const CREATE_PERMISSION: u16 = 0x008;
    pub const CHANNEL_BIND: u16 = 0x009;
}

pub mod attr {
    pub const MAPPED_ADDRESS: u16 = 0x0001;
    pub const USERNAME: u16 = 0x0006;
    pub const MESSAGE_INTEGRITY: u16 = 0x0008;
    pub const ERROR_CODE: u16 = 0x0009;
    pub const UNKNOWN_ATTRIBUTES: u16 = 0x000A;
    pub const CHANNEL_NUMBER: u16 = 0x000C;
    pub const LIFETIME: u16 = 0x000D;
    pub const XOR_PEER_ADDRESS: u16 = 0x0012;
    pub const DATA: u16 = 0x0013;
    pub const REALM: u16 = 0x0014;
    pub const NONCE: u16 = 0x0015;
    pub const XOR_RELAYED_ADDRESS: u16 = 0x0016;
    pub const REQUESTED_ADDRESS_FAMILY: u16 = 0x0017;
    pub const EVEN_PORT: u16 = 0x0018;
    pub const REQUESTED_TRANSPORT: u16 = 0x0019;
    pub const DONT_FRAGMENT: u16 = 0x001A;
    pub const XOR_MAPPED_ADDRESS: u16 = 0x0020;
    pub const RESERVATION_TOKEN: u16 = 0x0022;
    pub const PRIORITY: u16 = 0x0024;
    pub const USE_CANDIDATE: u16 = 0x0025;
    pub const SOFTWARE: u16 = 0x8022;
    pub const FINGERPRINT: u16 = 0x8028;
}

/// Comprehension-required attributes (type < 0x8000) this server understands. Anything else in a request gets a 420.
/// EVEN-PORT and RESERVATION-TOKEN are left out on purpose: browsers never send them and the server does not honor them.
const KNOWN: &[u16] = &[
    attr::MAPPED_ADDRESS,
    attr::USERNAME,
    attr::MESSAGE_INTEGRITY,
    attr::ERROR_CODE,
    attr::UNKNOWN_ATTRIBUTES,
    attr::CHANNEL_NUMBER,
    attr::LIFETIME,
    attr::XOR_PEER_ADDRESS,
    attr::DATA,
    attr::REALM,
    attr::NONCE,
    attr::XOR_RELAYED_ADDRESS,
    attr::REQUESTED_ADDRESS_FAMILY,
    attr::REQUESTED_TRANSPORT,
    attr::DONT_FRAGMENT,
    attr::XOR_MAPPED_ADDRESS,
    attr::PRIORITY,
    attr::USE_CANDIDATE,
];

const fn padded(len: usize) -> usize {
    (len + 3) & !3
}

/// A parsed STUN message borrowing its buffer.
pub struct Message<'a> {
    pub method: u16,
    pub class: Class,
    pub tx: TxId,
    buf: &'a [u8],
    attrs: Vec<(u16, &'a [u8])>,
    integrity: Option<(usize, &'a [u8])>, // (offset of the attribute header, HMAC)
}

impl<'a> Message<'a> {
    /// Parse exactly one message. Attributes after MESSAGE-INTEGRITY other than FINGERPRINT are ignored, as RFC 8489
    /// says; a FINGERPRINT must be last and correct.
    pub fn parse(buf: &'a [u8]) -> Option<Message<'a>> {
        if buf.len() < HEADER || buf[0] & 0xC0 != 0 {
            return None;
        }
        let t = u16::from_be_bytes([buf[0], buf[1]]);
        let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        if u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) != MAGIC || len % 4 != 0 || buf.len() != HEADER + len {
            return None;
        }
        let method = (t & 0x000F) | ((t & 0x00E0) >> 1) | ((t & 0x3E00) >> 2);
        let class = match ((t >> 4) & 1) | ((t >> 7) & 2) {
            0 => Class::Request,
            1 => Class::Indication,
            2 => Class::Success,
            _ => Class::Error,
        };
        let tx: TxId = buf[8..20].try_into().ok()?;

        let mut attrs = Vec::new();
        let mut integrity = None;
        let mut i = HEADER;
        while i < buf.len() {
            if i + 4 > buf.len() {
                return None;
            }
            let at = u16::from_be_bytes([buf[i], buf[i + 1]]);
            let al = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
            let end = i + 4 + padded(al);
            if end > buf.len() {
                return None;
            }
            let value = &buf[i + 4..i + 4 + al];
            if at == attr::FINGERPRINT {
                if end != buf.len() || al != 4 {
                    return None;
                }
                let want = u32::from_be_bytes(value.try_into().ok()?);
                if crc32(&buf[..i]) ^ FINGERPRINT_XOR != want {
                    return None;
                }
            } else if integrity.is_none() {
                if at == attr::MESSAGE_INTEGRITY {
                    if al != 20 {
                        return None;
                    }
                    integrity = Some((i, value));
                } else {
                    attrs.push((at, value));
                }
            }
            i = end;
        }
        Some(Message { method, class, tx, buf, attrs, integrity })
    }

    /// The first attribute of type `t` (before MESSAGE-INTEGRITY).
    pub fn get(&self, t: u16) -> Option<&'a [u8]> {
        self.attrs.iter().find(|(at, _)| *at == t).map(|(_, v)| *v)
    }

    /// Every attribute of type `t`, in order.
    pub fn all(&self, t: u16) -> impl Iterator<Item = &'a [u8]> + '_ {
        self.attrs.iter().filter(move |(at, _)| *at == t).map(|(_, v)| *v)
    }

    /// Comprehension-required attributes this server does not understand.
    pub fn unknown_required(&self) -> Vec<u16> {
        let mut out: Vec<u16> = self.attrs.iter().map(|(t, _)| *t).filter(|t| *t < 0x8000 && !KNOWN.contains(t)).collect();
        out.dedup();
        out
    }

    pub fn has_integrity(&self) -> bool {
        self.integrity.is_some()
    }

    /// Check MESSAGE-INTEGRITY (HMAC-SHA1 over the message up to it, with the length field covering it) with `key`.
    pub fn check_integrity(&self, key: &[u8]) -> bool {
        let Some((at, hmac)) = self.integrity else {
            return false;
        };
        let mut mac = <Hmac<Sha1>>::new_from_slice(key).expect("hmac accepts any key length");
        mac.update(&self.buf[..2]);
        mac.update(&((at - HEADER + 24) as u16).to_be_bytes());
        mac.update(&self.buf[4..at]);
        mac.verify_slice(hmac).is_ok()
    }
}

/// Builds a message; [`Builder::finish`] appends MESSAGE-INTEGRITY (with a key) and always FINGERPRINT.
pub struct Builder {
    t: u16,
    tx: TxId,
    body: Vec<u8>,
}

impl Builder {
    pub fn new(method: u16, class: Class, tx: TxId) -> Self {
        let c = match class {
            Class::Request => 0,
            Class::Indication => 1,
            Class::Success => 2,
            Class::Error => 3,
        };
        let m = method & 0x0FFF;
        let t = (m & 0x000F) | ((m & 0x0070) << 1) | ((m & 0x0F80) << 2) | ((c & 1) << 4) | ((c & 2) << 7);
        Builder { t, tx, body: Vec::with_capacity(64) }
    }

    pub fn attr(mut self, t: u16, v: &[u8]) -> Self {
        self.body.extend_from_slice(&t.to_be_bytes());
        self.body.extend_from_slice(&(v.len() as u16).to_be_bytes());
        self.body.extend_from_slice(v);
        self.body.resize(self.body.len() + padded(v.len()) - v.len(), 0);
        self
    }

    pub fn xor_addr(self, t: u16, addr: SocketAddr) -> Self {
        let v = encode_xor_addr(addr, &self.tx);
        self.attr(t, &v)
    }

    /// ERROR-CODE with `code` (e.g. 401) and a reason phrase.
    pub fn error(self, code: u16, reason: &str) -> Self {
        let mut v = vec![0, 0, (code / 100) as u8, (code % 100) as u8];
        v.extend_from_slice(reason.as_bytes());
        self.attr(attr::ERROR_CODE, &v)
    }

    pub fn finish(mut self, key: Option<&[u8]>) -> Vec<u8> {
        if let Some(key) = key {
            let mut mac = <Hmac<Sha1>>::new_from_slice(key).expect("hmac accepts any key length");
            mac.update(&self.header(self.body.len() + 24));
            mac.update(&self.body);
            let hmac = mac.finalize().into_bytes();
            self = self.attr(attr::MESSAGE_INTEGRITY, &hmac);
        }
        let mut crc_input = self.header(self.body.len() + 8).to_vec();
        crc_input.extend_from_slice(&self.body);
        let fingerprint = (crc32(&crc_input) ^ FINGERPRINT_XOR).to_be_bytes();
        self = self.attr(attr::FINGERPRINT, &fingerprint);
        let mut out = self.header(self.body.len()).to_vec();
        out.extend_from_slice(&self.body);
        out
    }

    fn header(&self, len: usize) -> [u8; HEADER] {
        let mut h = [0u8; HEADER];
        h[..2].copy_from_slice(&self.t.to_be_bytes());
        h[2..4].copy_from_slice(&(len as u16).to_be_bytes());
        h[4..8].copy_from_slice(&MAGIC.to_be_bytes());
        h[8..].copy_from_slice(&self.tx);
        h
    }
}

fn encode_xor_addr(addr: SocketAddr, tx: &TxId) -> Vec<u8> {
    let port = (addr.port() ^ (MAGIC >> 16) as u16).to_be_bytes();
    match addr.ip() {
        IpAddr::V4(ip) => {
            let x = (u32::from(ip) ^ MAGIC).to_be_bytes();
            vec![0, 1, port[0], port[1], x[0], x[1], x[2], x[3]]
        }
        IpAddr::V6(ip) => {
            let mut v = vec![0, 2, port[0], port[1]];
            let mut mask = MAGIC.to_be_bytes().to_vec();
            mask.extend_from_slice(tx);
            v.extend(ip.octets().iter().zip(mask).map(|(a, b)| a ^ b));
            v
        }
    }
}

/// Decode an XOR-MAPPED-ADDRESS-style value (XOR-PEER-ADDRESS, XOR-RELAYED-ADDRESS, …).
pub fn xor_addr(v: &[u8], tx: &TxId) -> Option<SocketAddr> {
    if v.len() < 4 {
        return None;
    }
    let port = u16::from_be_bytes([v[2], v[3]]) ^ (MAGIC >> 16) as u16;
    match (v[1], v.len()) {
        (1, 8) => {
            let ip = u32::from_be_bytes([v[4], v[5], v[6], v[7]]) ^ MAGIC;
            Some(SocketAddr::new(Ipv4Addr::from(ip).into(), port))
        }
        (2, 20) => {
            let mut mask = MAGIC.to_be_bytes().to_vec();
            mask.extend_from_slice(tx);
            let octets: [u8; 16] = std::array::from_fn(|i| v[4 + i] ^ mask[i]);
            Some(SocketAddr::new(Ipv6Addr::from(octets).into(), port))
        }
        _ => None,
    }
}

/// A ChannelData frame. Over TCP (`pad`) it is padded to a multiple of four bytes.
pub fn channel_data(ch: u16, data: &[u8], pad: bool) -> Vec<u8> {
    let total = 4 + if pad { padded(data.len()) } else { data.len() };
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&ch.to_be_bytes());
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
    out.resize(total, 0);
    out
}

/// A ChannelData frame's channel number (0x4000–0x4FFF) and data; trailing padding is ignored.
pub fn parse_channel_data(buf: &[u8]) -> Option<(u16, &[u8])> {
    if buf.len() < 4 {
        return None;
    }
    let ch = u16::from_be_bytes([buf[0], buf[1]]);
    let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    ((0x4000..=0x4FFF).contains(&ch) && buf.len() >= 4 + len).then(|| (ch, &buf[4..4 + len]))
}

/// What the start of a TCP stream holds.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    /// Not enough bytes yet.
    Need,
    /// One whole STUN message or ChannelData frame (with padding) of this many bytes.
    Complete(usize),
    /// Neither STUN nor ChannelData: close the connection.
    Bad,
}

pub fn frame(buf: &[u8]) -> Frame {
    if buf.len() < 4 {
        return Frame::Need;
    }
    let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let total = match buf[0] >> 6 {
        0 if len % 4 == 0 => HEADER + len,
        1 if (0x40..=0x4F).contains(&buf[0]) => 4 + padded(len),
        _ => return Frame::Bad,
    };
    if buf.len() >= total {
        Frame::Complete(total)
    } else {
        Frame::Need
    }
}

/// CRC-32 (IEEE 802.3), as FINGERPRINT uses.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use md5::{Digest, Md5};
    use rand::{rngs::StdRng, Rng, SeedableRng};

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    // RFC 5769 test vectors.
    const SHORT_TERM_PASSWORD: &[u8] = b"VOkJxbRl1RmTxUk/WvJxBt";

    fn rfc5769_request() -> Vec<u8> {
        hex("00 01 00 58 21 12 a4 42 b7 e7 a7 01 bc 34 d6 86 fa 87 df ae
             80 22 00 10 53 54 55 4e 20 74 65 73 74 20 63 6c 69 65 6e 74
             00 24 00 04 6e 00 01 ff 80 29 00 08 93 2f f9 b1 51 26 3b 36
             00 06 00 09 65 76 74 6a 3a 68 36 76 59 20 20 20
             00 08 00 14 9a ea a7 0c bf d8 cb 56 78 1e f2 b5 b2 d3 f2 49 c1 b5 71 a2
             80 28 00 04 e5 7a 3b cf")
    }

    fn rfc5769_ipv4_response() -> Vec<u8> {
        hex("01 01 00 3c 21 12 a4 42 b7 e7 a7 01 bc 34 d6 86 fa 87 df ae
             80 22 00 0b 74 65 73 74 20 76 65 63 74 6f 72 20
             00 20 00 08 00 01 a1 47 e1 12 a6 43
             00 08 00 14 2b 91 f5 99 fd 9e 90 c3 8c 74 89 f9 2a f9 ba 53 f0 6b e7 d7
             80 28 00 04 c0 7d 4c 96")
    }

    fn rfc5769_ipv6_response() -> Vec<u8> {
        hex("01 01 00 48 21 12 a4 42 b7 e7 a7 01 bc 34 d6 86 fa 87 df ae
             80 22 00 0b 74 65 73 74 20 76 65 63 74 6f 72 20
             00 20 00 14 00 02 a1 47 01 13 a9 fa a5 d3 f1 79 bc 25 f4 b5 be d2 b9 d9
             00 08 00 14 a3 82 95 4e 4b e6 7b f1 17 84 c9 7c 82 92 c2 75 bf e3 ed 41
             80 28 00 04 c8 fb 0b 4c")
    }

    fn rfc5769_long_term_request() -> Vec<u8> {
        hex("00 01 00 60 21 12 a4 42 78 ad 34 33 c6 ad 72 c0 29 da 41 2e
             00 06 00 12 e3 83 9e e3 83 88 e3 83 aa e3 83 83 e3 82 af e3 82 b9 00 00
             00 15 00 1c 66 2f 2f 34 39 39 6b 39 35 34 64 36 4f 4c 33 34 6f 4c 39 46 53 54 76 79 36 34 73 41
             00 14 00 0b 65 78 61 6d 70 6c 65 2e 6f 72 67 00
             00 08 00 14 f6 70 24 65 6d d6 4a 3e 02 b8 e0 71 2e 85 c9 a2 8c a8 96 66")
    }

    #[test]
    fn rfc5769_sample_request() {
        let buf = rfc5769_request();
        let m = Message::parse(&buf).expect("parses, fingerprint ok");
        assert_eq!((m.method, m.class), (method::BINDING, Class::Request));
        assert_eq!(m.get(attr::USERNAME), Some(&b"evtj:h6vY"[..]));
        assert_eq!(m.get(attr::SOFTWARE), Some(&b"STUN test client"[..]));
        assert!(m.check_integrity(SHORT_TERM_PASSWORD));
        assert!(!m.check_integrity(b"wrong"));
        assert_eq!(m.unknown_required(), Vec::<u16>::new(), "PRIORITY is known; ICE-CONTROLLED is optional");
    }

    #[test]
    fn rfc5769_sample_responses() {
        let buf = rfc5769_ipv4_response();
        let m = Message::parse(&buf).unwrap();
        assert_eq!((m.method, m.class), (method::BINDING, Class::Success));
        assert!(m.check_integrity(SHORT_TERM_PASSWORD));
        assert_eq!(xor_addr(m.get(attr::XOR_MAPPED_ADDRESS).unwrap(), &m.tx), Some("192.0.2.1:32853".parse().unwrap()));

        let buf = rfc5769_ipv6_response();
        let m = Message::parse(&buf).unwrap();
        assert!(m.check_integrity(SHORT_TERM_PASSWORD));
        assert_eq!(
            xor_addr(m.get(attr::XOR_MAPPED_ADDRESS).unwrap(), &m.tx),
            Some("[2001:db8:1234:5678:11:2233:4455:6677]:32853".parse().unwrap())
        );
    }

    #[test]
    fn rfc5769_long_term_auth() {
        let buf = rfc5769_long_term_request();
        let m = Message::parse(&buf).unwrap();
        let user = std::str::from_utf8(m.get(attr::USERNAME).unwrap()).unwrap();
        assert_eq!(user, "マトリックス");
        assert_eq!(m.get(attr::REALM), Some(&b"example.org"[..]));
        assert_eq!(m.get(attr::NONCE), Some(&b"f//499k954d6OL34oL9FSTvy64sA"[..]));
        let key = Md5::digest("マトリックス:example.org:TheMatrIX".as_bytes());
        assert!(m.check_integrity(&key));
    }

    #[test]
    fn builder_reproduces_rfc5769_ipv4_response() {
        let want = rfc5769_ipv4_response();
        let tx: TxId = want[8..20].try_into().unwrap();
        let got = Builder::new(method::BINDING, Class::Success, tx)
            .attr(attr::SOFTWARE, b"test vector")
            .xor_addr(attr::XOR_MAPPED_ADDRESS, "192.0.2.1:32853".parse().unwrap())
            .finish(Some(SHORT_TERM_PASSWORD));
        // The RFC pads SOFTWARE with a space; the builder pads with zero. Everything else must match once that byte agrees.
        let mut got_spaced = got.clone();
        got_spaced[35] = 0x20;
        let reparsed = Message::parse(&got).unwrap();
        assert!(reparsed.check_integrity(SHORT_TERM_PASSWORD));
        assert_eq!(&got_spaced[..48], &want[..48], "header, SOFTWARE and XOR-MAPPED-ADDRESS");
    }

    #[test]
    fn round_trip_every_class_and_method() {
        let tx = [7u8; 12];
        for class in [Class::Request, Class::Indication, Class::Success, Class::Error] {
            for m in [method::BINDING, method::ALLOCATE, method::REFRESH, method::SEND, method::DATA, method::CREATE_PERMISSION, method::CHANNEL_BIND] {
                let buf = Builder::new(m, class, tx).attr(attr::DATA, b"hello").finish(None);
                let msg = Message::parse(&buf).unwrap();
                assert_eq!((msg.method, msg.class, msg.tx), (m, class, tx));
                assert_eq!(msg.get(attr::DATA), Some(&b"hello"[..]));
                assert!(!msg.has_integrity());
            }
        }
        assert_eq!(Builder::new(method::ALLOCATE, Class::Error, tx).finish(None)[..2], [0x01, 0x13]);
        assert_eq!(Builder::new(method::DATA, Class::Indication, tx).finish(None)[..2], [0x00, 0x17]);
    }

    #[test]
    fn error_code_and_repeated_attributes() {
        let buf = Builder::new(method::ALLOCATE, Class::Error, [1; 12])
            .error(438, "Stale Nonce")
            .xor_addr(attr::XOR_PEER_ADDRESS, "1.2.3.4:5".parse().unwrap())
            .xor_addr(attr::XOR_PEER_ADDRESS, "5.6.7.8:9".parse().unwrap())
            .finish(Some(b"k"));
        let m = Message::parse(&buf).unwrap();
        assert_eq!(m.get(attr::ERROR_CODE), Some(&[0, 0, 4, 38, b'S', b't', b'a', b'l', b'e', b' ', b'N', b'o', b'n', b'c', b'e'][..]));
        let peers: Vec<_> = m.all(attr::XOR_PEER_ADDRESS).map(|v| xor_addr(v, &m.tx).unwrap().to_string()).collect();
        assert_eq!(peers, ["1.2.3.4:5", "5.6.7.8:9"]);
        assert!(m.check_integrity(b"k"));
    }

    #[test]
    fn tampering_is_caught() {
        let good = rfc5769_request();
        // A changed attribute byte breaks the fingerprint.
        let mut bad = good.clone();
        bad[30] ^= 1;
        assert!(Message::parse(&bad).is_none());
        // Without FINGERPRINT (and the length fixed up), a changed byte breaks MESSAGE-INTEGRITY instead.
        let mut no_fp = good[..good.len() - 8].to_vec();
        let len = (no_fp.len() - HEADER) as u16;
        no_fp[2..4].copy_from_slice(&len.to_be_bytes());
        assert!(Message::parse(&no_fp).unwrap().check_integrity(SHORT_TERM_PASSWORD));
        no_fp[30] ^= 1;
        assert!(!Message::parse(&no_fp).unwrap().check_integrity(SHORT_TERM_PASSWORD));
    }

    #[test]
    fn unknown_required_attributes() {
        let buf = Builder::new(method::ALLOCATE, Class::Request, [0; 12])
            .attr(attr::REQUESTED_TRANSPORT, &[17, 0, 0, 0])
            .attr(attr::EVEN_PORT, &[0x80])
            .attr(0x7777, b"x")
            .attr(0xC001, b"optional")
            .finish(None);
        assert_eq!(Message::parse(&buf).unwrap().unknown_required(), vec![attr::EVEN_PORT, 0x7777]);
    }

    #[test]
    fn rejects_malformed_headers() {
        let good = Builder::new(method::BINDING, Class::Request, [0; 12]).finish(None);
        assert!(Message::parse(&good).is_some());
        let mut bad = good.clone();
        bad[4] = 0; // magic cookie
        assert!(Message::parse(&bad).is_none());
        let mut bad = good.clone();
        bad[0] |= 0x40; // ChannelData bits
        assert!(Message::parse(&bad).is_none());
        let mut long = good.clone();
        long.push(0);
        assert!(Message::parse(&long).is_none(), "length field must match");
        assert!(Message::parse(&good[..19]).is_none());
    }

    #[test]
    fn random_and_truncated_input_never_panics() {
        let mut rng = StdRng::seed_from_u64(5769);
        let samples = [rfc5769_request(), rfc5769_ipv4_response(), rfc5769_ipv6_response(), rfc5769_long_term_request()];
        for _ in 0..20_000 {
            let mut buf = samples[rng.gen_range(0..samples.len())].clone();
            match rng.gen_range(0..4) {
                0 => buf.truncate(rng.gen_range(0..buf.len())),
                1 => {
                    for _ in 0..rng.gen_range(1..6) {
                        let i = rng.gen_range(0..buf.len());
                        buf[i] = rng.gen();
                    }
                }
                2 => buf = (0..rng.gen_range(0..120)).map(|_| rng.gen()).collect(),
                _ => {
                    // Keep the header valid but scramble the attribute area.
                    for b in buf.iter_mut().skip(HEADER) {
                        *b = rng.gen();
                    }
                }
            }
            if let Some(m) = Message::parse(&buf) {
                let _ = (m.unknown_required(), m.check_integrity(b"k"), m.get(attr::XOR_PEER_ADDRESS).map(|v| xor_addr(v, &m.tx)));
            }
            let _ = (frame(&buf), parse_channel_data(&buf));
        }
    }

    #[test]
    fn channel_data_and_framing() {
        let udp = channel_data(0x4001, b"abcde", false);
        assert_eq!(udp, [0x40, 0x01, 0, 5, b'a', b'b', b'c', b'd', b'e']);
        let tcp = channel_data(0x4001, b"abcde", true);
        assert_eq!(tcp.len(), 12);
        assert_eq!(parse_channel_data(&tcp), Some((0x4001, &b"abcde"[..])));
        assert_eq!(parse_channel_data(&channel_data(0x3FFF, b"x", false)), None, "below the channel range");
        assert_eq!(parse_channel_data(&udp[..6]), None, "truncated");

        assert_eq!(frame(&tcp), Frame::Complete(12));
        assert_eq!(frame(&tcp[..9]), Frame::Need, "padding not in yet");
        let stun = Builder::new(method::BINDING, Class::Request, [0; 12]).finish(None);
        let mut two = stun.clone();
        two.extend_from_slice(&tcp);
        assert_eq!(frame(&two), Frame::Complete(stun.len()));
        assert_eq!(frame(&stun[..3]), Frame::Need);
        assert_eq!(frame(&stun[..10]), Frame::Need);
        assert_eq!(frame(b"POST / HTTP/1.1"), Frame::Bad); // (GET starts with 0x47: a valid channel byte)
        assert_eq!(frame(&[0x0D, 0x0A, 0x0D, 0x0A]), Frame::Bad, "a PROXY header is not a frame");
        assert_eq!(frame(&[0x50, 0, 0, 0]), Frame::Bad, "0x5000 is outside the channel range");
    }

    #[test]
    fn crc32_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
