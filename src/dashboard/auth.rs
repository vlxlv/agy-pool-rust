//! Bearer-token checks for the dashboard API: constant-time compare and a wrong-token limiter.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use hyper::HeaderMap;

/// TCP peer of the connection, inserted by the accept loop. Never derived from request headers.
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub SocketAddr);

pub const FAILURE_WINDOW: Duration = Duration::from_secs(60);
pub const PER_IP_FAILURE_LIMIT: u32 = 5;
pub const GLOBAL_FAILURE_LIMIT: u32 = 100;
const MAX_TRACKED_IPS: usize = 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum BearerCheck {
    Ok,
    Missing,
    Invalid,
    Throttled,
}

/// Compares SHA-256 digests so timing reveals neither the token length nor a matching prefix.
fn token_matches(presented: &str, expected: &str) -> bool {
    use sha2::{Digest, Sha256};
    let a = Sha256::digest(presented.as_bytes());
    let b = Sha256::digest(expected.as_bytes());
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Ok(None): no Authorization header. Err: repeated, non-Bearer or empty.
fn parse_bearer(headers: &HeaderMap) -> Result<Option<&str>, ()> {
    let mut values = headers.get_all(hyper::header::AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    let (scheme, token) = value.to_str().map_err(|_| ())?.split_once(' ').ok_or(())?;
    let token = token.trim();
    if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() {
        return Err(());
    }
    Ok(Some(token))
}

/// Public IPv6 clients usually control a whole /64, so failures are counted per /64. Private ranges
/// (Tailscale's fd7a:115c:a1e0::/48, LAN ULAs, link-local) share one /64 across many hosts, so one
/// neighbour could lock everyone out; those are counted per address.
fn client_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) if v6.is_unique_local() || v6.is_unicast_link_local() => IpAddr::V6(v6),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
        v4 => v4,
    }
}

/// Fixed-window wrong-token counts per client and overall. The global budget stops guessing
/// spread over many addresses, at the cost of locking everyone out for up to one window.
pub struct FailureLimiter {
    window: Duration,
    per_ip_limit: u32,
    global_limit: u32,
    global: (Instant, u32),
    per_ip: HashMap<IpAddr, (Instant, u32)>,
}

impl FailureLimiter {
    pub fn new(window: Duration, per_ip_limit: u32, global_limit: u32, now: Instant) -> Self {
        Self {
            window,
            per_ip_limit,
            global_limit,
            global: (now, 0),
            per_ip: HashMap::new(),
        }
    }

    pub fn is_blocked(&self, ip: IpAddr, now: Instant) -> bool {
        let exhausted =
            |&(start, count): &(Instant, u32), limit: u32| now.duration_since(start) < self.window && count >= limit;
        exhausted(&self.global, self.global_limit)
            || self
                .per_ip
                .get(&client_key(ip))
                .is_some_and(|e| exhausted(e, self.per_ip_limit))
    }

    pub fn record_failure(&mut self, ip: IpAddr, now: Instant) {
        let window = self.window;
        let bump = |entry: &mut (Instant, u32)| {
            if now.duration_since(entry.0) >= window {
                *entry = (now, 0);
            }
            entry.1 = entry.1.saturating_add(1);
        };
        bump(&mut self.global);
        let key = client_key(ip);
        if !self.per_ip.contains_key(&key) && self.per_ip.len() >= MAX_TRACKED_IPS {
            self.per_ip.retain(|_, e| now.duration_since(e.0) < window);
            if self.per_ip.len() >= MAX_TRACKED_IPS {
                return; // still counted by the global budget
            }
        }
        bump(self.per_ip.entry(key).or_insert((now, 0)));
    }
}

static LIMITER: LazyLock<Mutex<FailureLimiter>> = LazyLock::new(|| Mutex::new(new_limiter()));

fn new_limiter() -> FailureLimiter {
    FailureLimiter::new(
        FAILURE_WINDOW,
        PER_IP_FAILURE_LIMIT,
        GLOBAL_FAILURE_LIMIT,
        Instant::now(),
    )
}

/// Test hook: forget every recorded failure.
pub fn reset_auth_failures() {
    *LIMITER.lock().unwrap_or_else(|e| e.into_inner()) = new_limiter();
}

/// Requests served without the production accept loop carry no PeerAddr; they share one bucket.
pub fn peer_ip(extensions: &hyper::http::Extensions) -> IpAddr {
    extensions
        .get::<PeerAddr>()
        .map(|p| p.0.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

/// Checks the request's Bearer token; wrong tokens count against the limiter, missing ones do not.
pub fn check_bearer(headers: &HeaderMap, peer: IpAddr, expected: &str) -> BearerCheck {
    let mut limiter = LIMITER.lock().unwrap_or_else(|e| e.into_inner());
    check_bearer_with(&mut limiter, headers, peer, expected, Instant::now())
}

fn check_bearer_with(
    limiter: &mut FailureLimiter,
    headers: &HeaderMap,
    peer: IpAddr,
    expected: &str,
    now: Instant,
) -> BearerCheck {
    if limiter.is_blocked(peer, now) {
        return BearerCheck::Throttled;
    }
    match parse_bearer(headers) {
        Ok(None) => BearerCheck::Missing,
        Ok(Some(token)) if token_matches(token, expected) => BearerCheck::Ok,
        _ => {
            limiter.record_failure(peer, now);
            BearerCheck::Invalid
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn auth_headers(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append(hyper::header::AUTHORIZATION, v.parse().unwrap());
        }
        h
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn limiter(now: Instant) -> FailureLimiter {
        FailureLimiter::new(FAILURE_WINDOW, PER_IP_FAILURE_LIMIT, GLOBAL_FAILURE_LIMIT, now)
    }

    #[test]
    fn token_matches_only_exact_token() {
        assert!(token_matches(TOKEN, TOKEN));
        assert!(!token_matches("0123456789abcdef0123456789abcdee", TOKEN));
        assert!(!token_matches("0123", TOKEN));
        assert!(!token_matches("", TOKEN));
    }

    #[test]
    fn parse_bearer_variants() {
        assert_eq!(parse_bearer(&HeaderMap::new()), Ok(None));
        assert_eq!(parse_bearer(&auth_headers(&["Bearer abc"])), Ok(Some("abc")));
        assert_eq!(parse_bearer(&auth_headers(&["bearer   abc  "])), Ok(Some("abc")));
        assert_eq!(parse_bearer(&auth_headers(&["BEARER abc"])), Ok(Some("abc")));
        assert!(parse_bearer(&auth_headers(&["Basic abc"])).is_err());
        assert!(parse_bearer(&auth_headers(&["Bearer"])).is_err());
        assert!(parse_bearer(&auth_headers(&["Bearer   "])).is_err());
        assert!(parse_bearer(&auth_headers(&["Bearer abc", "Bearer abc"])).is_err());
    }

    #[test]
    fn peer_ip_defaults_to_unspecified() {
        let mut ext = hyper::http::Extensions::new();
        assert_eq!(peer_ip(&ext), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        ext.insert(PeerAddr(SocketAddr::from(([100, 64, 0, 1], 5000))));
        assert_eq!(peer_ip(&ext), ip("100.64.0.1"));
    }

    #[test]
    fn per_ip_limit_blocks_then_expires() {
        let t0 = Instant::now();
        let mut l = limiter(t0);
        for _ in 0..PER_IP_FAILURE_LIMIT {
            assert!(!l.is_blocked(ip("10.0.0.1"), t0));
            l.record_failure(ip("10.0.0.1"), t0);
        }
        assert!(l.is_blocked(ip("10.0.0.1"), t0));
        assert!(!l.is_blocked(ip("10.0.0.2"), t0), "other clients are unaffected");
        assert!(
            !l.is_blocked(ip("10.0.0.1"), t0 + FAILURE_WINDOW),
            "block ends with the window"
        );
    }

    #[test]
    fn ipv6_counts_per_64_and_mapped_v4_matches_v4() {
        let t0 = Instant::now();
        let mut l = limiter(t0);
        for i in 0..PER_IP_FAILURE_LIMIT {
            l.record_failure(
                IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0, 0, 0, i as u16 + 1)),
                t0,
            );
        }
        assert!(l.is_blocked(ip("2001:db8:1:2::ffff"), t0));
        assert!(!l.is_blocked(ip("2001:db8:1:3::1"), t0));

        // Tailscale nodes share a /64; one node's failures must not block its neighbours.
        let mut l = limiter(t0);
        for _ in 0..PER_IP_FAILURE_LIMIT {
            l.record_failure(ip("fd7a:115c:a1e0::1"), t0);
        }
        assert!(l.is_blocked(ip("fd7a:115c:a1e0::1"), t0));
        assert!(!l.is_blocked(ip("fd7a:115c:a1e0::2"), t0));

        let mut l = limiter(t0);
        for _ in 0..PER_IP_FAILURE_LIMIT {
            l.record_failure(ip("::ffff:192.0.2.7"), t0);
        }
        assert!(l.is_blocked(ip("192.0.2.7"), t0));
    }

    #[test]
    fn global_limit_blocks_everyone() {
        let t0 = Instant::now();
        let mut l = limiter(t0);
        for i in 0..GLOBAL_FAILURE_LIMIT {
            l.record_failure(IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + i)), t0);
        }
        assert!(l.is_blocked(ip("192.0.2.1"), t0));
        assert!(!l.is_blocked(ip("192.0.2.1"), t0 + FAILURE_WINDOW));
    }

    #[test]
    fn tracked_clients_are_capped() {
        let t0 = Instant::now();
        let mut l = FailureLimiter::new(FAILURE_WINDOW, PER_IP_FAILURE_LIMIT, u32::MAX, t0);
        for i in 0..(MAX_TRACKED_IPS as u32 + 50) {
            l.record_failure(IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + i)), t0);
        }
        assert!(l.per_ip.len() <= MAX_TRACKED_IPS);
        // Expired entries are pruned to make room.
        l.record_failure(ip("192.0.2.1"), t0 + FAILURE_WINDOW);
        assert!(l.per_ip.contains_key(&ip("192.0.2.1")));
    }

    #[test]
    fn check_bearer_counts_only_wrong_tokens() {
        let t0 = Instant::now();
        let mut l = limiter(t0);
        let peer = ip("127.0.0.1");
        for _ in 0..(PER_IP_FAILURE_LIMIT * 2) {
            assert_eq!(
                check_bearer_with(&mut l, &HeaderMap::new(), peer, TOKEN, t0),
                BearerCheck::Missing
            );
        }
        let good_value = format!("Bearer {TOKEN}");
        let good = auth_headers(&[good_value.as_str()]);
        assert_eq!(check_bearer_with(&mut l, &good, peer, TOKEN, t0), BearerCheck::Ok);

        let bad = auth_headers(&["Bearer wrong"]);
        for _ in 0..PER_IP_FAILURE_LIMIT {
            assert_eq!(check_bearer_with(&mut l, &bad, peer, TOKEN, t0), BearerCheck::Invalid);
        }
        assert_eq!(
            check_bearer_with(&mut l, &good, peer, TOKEN, t0),
            BearerCheck::Throttled
        );
        let repeated = auth_headers(&[good_value.as_str(), good_value.as_str()]);
        assert_eq!(
            check_bearer_with(&mut l, &repeated, ip("127.0.0.2"), TOKEN, t0),
            BearerCheck::Invalid
        );
    }
}
