//! PROXY protocol v2 headers (haproxy's proxy-protocol.txt), as a reverse proxy such as Caddy's layer4 app sends
//! ahead of a TCP stream it forwards: they carry the real client address. Pure parsing; who may send one is the
//! caller's decision (`trusted_proxies`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub const SIGNATURE: [u8; 12] = *b"\r\n\r\n\0\r\nQUIT\n";

#[derive(Debug, PartialEq, Eq)]
pub enum Proxy {
    /// Not enough bytes yet.
    Need,
    /// A complete header of `len` bytes. `src` is the client, or `None` for a LOCAL connection (health checks) or an
    /// address family other than IPv4/IPv6: use the socket's own peer address then.
    Header { src: Option<SocketAddr>, len: usize },
    /// Not a v2 header.
    Bad,
}

pub fn parse_v2(buf: &[u8]) -> Proxy {
    let sig = buf.len().min(SIGNATURE.len());
    if buf[..sig] != SIGNATURE[..sig] {
        return Proxy::Bad;
    }
    if buf.len() < 16 {
        return Proxy::Need;
    }
    let (version, command) = (buf[12] >> 4, buf[12] & 0x0F);
    if version != 2 || command > 1 {
        return Proxy::Bad;
    }
    let len = 16 + u16::from_be_bytes([buf[14], buf[15]]) as usize;
    if buf.len() < len {
        return Proxy::Need;
    }
    if command == 0 {
        return Proxy::Header { src: None, len };
    }
    let a = &buf[16..len];
    let src = match buf[13] >> 4 {
        1 if a.len() >= 12 => {
            let ip = Ipv4Addr::new(a[0], a[1], a[2], a[3]);
            Some(SocketAddr::new(IpAddr::V4(ip), u16::from_be_bytes([a[8], a[9]])))
        }
        2 if a.len() >= 36 => {
            let octets: [u8; 16] = a[..16].try_into().expect("16 bytes");
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), u16::from_be_bytes([a[32], a[33]])))
        }
        1 | 2 => return Proxy::Bad,
        _ => None,
    };
    Proxy::Header { src, len }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(ver_cmd: u8, fam: u8, addrs: &[u8]) -> Vec<u8> {
        let mut h = SIGNATURE.to_vec();
        h.extend_from_slice(&[ver_cmd, fam]);
        h.extend_from_slice(&(addrs.len() as u16).to_be_bytes());
        h.extend_from_slice(addrs);
        h
    }

    #[test]
    fn tcp4_header() {
        // The bytes Caddy's layer4 proxy sent in the Oct 8 spike: 10.51.2.4:52232 -> 10.51.2.3:443.
        let h = header(0x21, 0x11, &[10, 51, 2, 4, 10, 51, 2, 3, 0xcc, 0x08, 0x01, 0xbb]);
        let mut stream = h.clone();
        stream.extend_from_slice(b"hello");
        assert_eq!(parse_v2(&stream), Proxy::Header { src: Some("10.51.2.4:52232".parse().unwrap()), len: 28 });
    }

    #[test]
    fn tcp6_header_with_tlv() {
        let mut addrs = Vec::new();
        addrs.extend_from_slice(&"2001:db8::1".parse::<Ipv6Addr>().unwrap().octets());
        addrs.extend_from_slice(&"2001:db8::2".parse::<Ipv6Addr>().unwrap().octets());
        addrs.extend_from_slice(&[0x13, 0x88, 0x01, 0xbb]);
        addrs.extend_from_slice(&[0x04, 0x00, 0x01, 0x00]); // a NOOP TLV after the addresses
        let h = header(0x21, 0x21, &addrs);
        assert_eq!(parse_v2(&h), Proxy::Header { src: Some("[2001:db8::1]:5000".parse().unwrap()), len: h.len() });
    }

    #[test]
    fn local_and_unspec() {
        assert_eq!(parse_v2(&header(0x20, 0x00, &[])), Proxy::Header { src: None, len: 16 });
        assert_eq!(parse_v2(&header(0x21, 0x00, &[])), Proxy::Header { src: None, len: 16 });
        assert_eq!(parse_v2(&header(0x21, 0x31, &[0; 216])), Proxy::Header { src: None, len: 232 }, "AF_UNIX");
    }

    #[test]
    fn partial_and_bad() {
        let h = header(0x21, 0x11, &[1, 2, 3, 4, 5, 6, 7, 8, 0, 1, 0, 2]);
        for cut in [0, 1, 11, 12, 15, 16, 27] {
            assert_eq!(parse_v2(&h[..cut]), Proxy::Need, "cut at {cut}");
        }
        assert_eq!(parse_v2(b"PROXY TCP4 1.2.3.4 5.6.7.8 1 2\r\n"), Proxy::Bad, "v1 is not supported");
        assert_eq!(parse_v2(&[0x00, 0x01, 0x00, 0x00]), Proxy::Bad, "STUN");
        assert_eq!(parse_v2(&header(0x11, 0x11, &[0; 12])), Proxy::Bad, "version 1 in the binary format");
        assert_eq!(parse_v2(&header(0x22, 0x11, &[0; 12])), Proxy::Bad, "unknown command");
        assert_eq!(parse_v2(&header(0x21, 0x11, &[0; 8])), Proxy::Bad, "too short for IPv4 addresses");
        assert_eq!(parse_v2(&header(0x21, 0x21, &[0; 12])), Proxy::Bad, "too short for IPv6 addresses");
    }
}
