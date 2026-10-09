//! TURN long-term credentials (RFC 8489 §9.2) over the usernames and passwords `/turn` mints: username
//! `<expiry unix>:<app id>`, password `base64(HMAC-SHA1(secret, username))` (coturn's `use-auth-secret` scheme), and
//! stateless nonces bound to the client's IP.

use std::{collections::HashSet, net::IpAddr};

use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use rand::RngCore;
use sha1::Sha1;
use sha2::Sha256;

pub const REALM: &str = "handshake";

/// The password for `username`: base64(HMAC-SHA1(secret, username)).
pub fn password(secret: &[u8], username: &str) -> String {
    let mut mac = <Hmac<Sha1>>::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(username.as_bytes());
    STANDARD.encode(mac.finalize().into_bytes())
}

/// The MESSAGE-INTEGRITY key: MD5(username ":" realm ":" password).
pub fn long_term_key(username: &str, realm: &str, password: &str) -> [u8; 16] {
    Md5::digest(format!("{username}:{realm}:{password}").as_bytes()).into()
}

#[derive(Debug, PartialEq, Eq)]
pub enum NonceCheck {
    Valid,
    /// Ours, but expired: answer 438 with a fresh one.
    Stale,
    Bad,
}

pub struct Auth {
    secret: Vec<u8>,
    apps: HashSet<String>,
    nonce_key: [u8; 32],
    nonce_secs: u64,
}

impl Auth {
    /// `apps`: the app ids with `turn = true`. Nonces last `nonce_secs`.
    pub fn new(secret: Vec<u8>, apps: HashSet<String>, nonce_secs: u64) -> Self {
        let mut nonce_key = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut nonce_key);
        Auth { secret, apps, nonce_key, nonce_secs }
    }

    fn nonce_mac(&self, expiry: u64, client: IpAddr) -> String {
        let mut mac = <Hmac<Sha256>>::new_from_slice(&self.nonce_key).expect("hmac accepts any key length");
        mac.update(format!("{expiry} {client}").as_bytes());
        URL_SAFE_NO_PAD.encode(&mac.finalize().into_bytes()[..12])
    }

    /// A nonce for `client`, valid `nonce_secs` from `now` (unix seconds).
    pub fn nonce(&self, client: IpAddr, now: u64) -> String {
        let expiry = now + self.nonce_secs;
        format!("{expiry}.{}", self.nonce_mac(expiry, client))
    }

    pub fn check_nonce(&self, nonce: &[u8], client: IpAddr, now: u64) -> NonceCheck {
        let Some((expiry, mac)) = std::str::from_utf8(nonce).ok().and_then(|n| n.split_once('.')) else {
            return NonceCheck::Bad;
        };
        let Ok(expiry) = expiry.parse::<u64>() else {
            return NonceCheck::Bad;
        };
        match (crate::ct_eq(&self.nonce_mac(expiry, client), mac), expiry > now) {
            (false, _) => NonceCheck::Bad,
            (true, false) => NonceCheck::Stale,
            (true, true) => NonceCheck::Valid,
        }
    }

    /// The MESSAGE-INTEGRITY key for `username`, or `None` when it is malformed, expired or not for a TURN app.
    pub fn user_key(&self, username: &str, now: u64) -> Option<[u8; 16]> {
        let (expiry, app) = username.split_once(':')?;
        let expiry: u64 = expiry.parse().ok()?;
        (expiry > now && self.apps.contains(app)).then(|| long_term_key(username, REALM, &password(&self.secret, username)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> Auth {
        Auth::new(b"turn-secret".to_vec(), HashSet::from(["game".to_string()]), 600)
    }

    #[test]
    fn password_matches_coturn_scheme() {
        // Known-answer HMAC-SHA1, computed independently:
        //   printf '1700000000:game' | openssl dgst -sha1 -hmac turn-secret -binary | base64
        assert_eq!(password(b"turn-secret", "1700000000:game"), "n12nWdEgEYR9Wpg+vbxX8n3V8VA=");
        // 20-byte SHA1 digest -> 28 base64 chars with padding.
        let cred = password(b"k", "1:a");
        assert_eq!(STANDARD.decode(&cred).unwrap().len(), 20);
        assert_ne!(cred, password(b"k", "2:a"));
    }

    #[test]
    fn long_term_key_is_md5_of_user_realm_password() {
        // printf 'user:realm:pass' | md5sum
        let hex: String = long_term_key("user", "realm", "pass").iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "8493fbc53ba582fb4c044c456bdc40eb");
    }

    #[test]
    fn nonces() {
        let a = auth();
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        let n = a.nonce(ip, 1000);
        assert_eq!(a.check_nonce(n.as_bytes(), ip, 1000), NonceCheck::Valid);
        assert_eq!(a.check_nonce(n.as_bytes(), ip, 1599), NonceCheck::Valid);
        assert_eq!(a.check_nonce(n.as_bytes(), ip, 1600), NonceCheck::Stale);
        assert_eq!(a.check_nonce(n.as_bytes(), "203.0.113.8".parse().unwrap(), 1000), NonceCheck::Bad, "bound to the IP");
        let forged = n.replacen("1600", "9999", 1);
        assert_eq!(a.check_nonce(forged.as_bytes(), ip, 1000), NonceCheck::Bad, "expiry is under the MAC");
        assert_eq!(auth().check_nonce(n.as_bytes(), ip, 1000), NonceCheck::Bad, "another process's key");
        for junk in [&b""[..], b"x", b"1.2.3", b"abc.def", &[0xff, 0xfe]] {
            assert_eq!(a.check_nonce(junk, ip, 1000), NonceCheck::Bad);
        }
    }

    #[test]
    fn user_keys() {
        let a = auth();
        let user = "2000:game";
        let want = long_term_key(user, REALM, &password(b"turn-secret", user));
        assert_eq!(a.user_key(user, 1999), Some(want));
        assert_eq!(a.user_key(user, 2000), None, "expired");
        assert_eq!(a.user_key("2000:other", 1000), None, "app without turn");
        for bad in ["", "game", "x:game", "2000game", ":game"] {
            assert_eq!(a.user_key(bad, 1000), None, "{bad}");
        }
    }
}
