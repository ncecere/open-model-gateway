//! Sign-in abuse controls (#78): which client address to believe, and a
//! bounded per-address token bucket held in this replica's memory.
//!
//! Nothing here touches the database, so the limiter cannot itself be used to
//! load it. Each replica counts on its own (the effective limit grows with the
//! replica count); the installation-wide bound is the pending-attempt cap in
//! `identity.rs` `login`. There is deliberately no per-account lockout: the IdP
//! owns credentials, and a lockout would let anyone lock a known user out and
//! probe which accounts exist.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv6Addr},
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::http::HeaderMap;

/// Distinct client buckets kept per replica. Past this, new addresses share
/// one overflow bucket, so an attacker with many addresses cannot grow memory.
const MAX_BUCKETS: usize = 10_000;

/// Networks whose `X-Forwarded-For` is believed (`GATEWAY_TRUSTED_PROXIES`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TrustedProxies(Vec<(IpAddr, u8)>);

impl TrustedProxies {
    /// Comma-separated addresses or CIDR networks, e.g. `10.0.0.0/8,fd00::/8`.
    pub(crate) fn parse(value: &str) -> anyhow::Result<Self> {
        let mut networks = Vec::new();
        for item in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (addr, prefix) = match item.split_once('/') {
                Some((addr, prefix)) => (addr, Some(prefix)),
                None => (item, None),
            };
            let addr: IpAddr = addr.parse().map_err(|_| {
                anyhow::anyhow!("GATEWAY_TRUSTED_PROXIES: invalid address {item:?}")
            })?;
            let max = if addr.is_ipv4() { 32 } else { 128 };
            let prefix = match prefix {
                None => max,
                Some(p) => p.parse::<u8>().ok().filter(|p| *p <= max).ok_or_else(|| {
                    anyhow::anyhow!("GATEWAY_TRUSTED_PROXIES: invalid prefix in {item:?}")
                })?,
            };
            anyhow::ensure!(
                masked(addr, prefix) == addr,
                "GATEWAY_TRUSTED_PROXIES: {item:?} has host bits set"
            );
            networks.push((addr, prefix));
        }
        Ok(Self(networks))
    }

    pub(crate) fn contains(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        self.0
            .iter()
            .any(|&(net, prefix)| net.is_ipv4() == ip.is_ipv4() && masked(ip, prefix) == net)
    }
}

/// An IPv4-mapped IPv6 address is the IPv4 address.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

fn masked(ip: IpAddr, prefix: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            IpAddr::V4((u32::from(v4) & mask).into())
        }
        IpAddr::V6(v6) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            IpAddr::V6(Ipv6Addr::from(u128::from(v6) & mask))
        }
    }
}

/// The client address to rate-limit. The connection's peer, unless the peer
/// is a trusted proxy: then the right-most `X-Forwarded-For` entry that is not
/// itself a trusted proxy. Entries left of an unparsable one are never used
/// (anything there could be client-written), so such a request is counted
/// under the last trusted hop. Without a peer (in-process tests), `None`.
pub(crate) fn client_ip(
    peer: Option<IpAddr>,
    headers: &HeaderMap,
    trusted: &TrustedProxies,
) -> Option<IpAddr> {
    let mut current = canonical(peer?);
    if !trusted.contains(current) {
        return Some(current);
    }
    let hops: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .collect();
    for hop in hops.into_iter().rev() {
        let Ok(ip) = hop.parse::<IpAddr>() else {
            return Some(current);
        };
        current = canonical(ip);
        if !trusted.contains(current) {
            return Some(current);
        }
    }
    Some(current)
}

/// Bucket key: an IPv4 address, or an IPv6 /64 (one subscriber's allocation).
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum Key {
    Ip(IpAddr),
    /// No peer address known.
    Unknown,
    /// The bucket table is full.
    Overflow,
}

fn key(ip: Option<IpAddr>) -> Key {
    match ip.map(canonical) {
        None => Key::Unknown,
        Some(IpAddr::V6(v6)) => Key::Ip(masked(IpAddr::V6(v6), 64)),
        Some(v4) => Key::Ip(v4),
    }
}

#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

/// A token bucket per client address: `per_minute` requests a minute, with a
/// burst of the same size.
pub(crate) struct Limiter {
    buckets: Mutex<HashMap<Key, Bucket>>,
    per_second: f64,
    burst: f64,
    capacity: usize,
}

impl Limiter {
    pub(crate) fn new(per_minute: u32) -> Self {
        Self::with_capacity(per_minute, MAX_BUCKETS)
    }

    fn with_capacity(per_minute: u32, capacity: usize) -> Self {
        Self {
            buckets: Mutex::default(),
            per_second: f64::from(per_minute) / 60.0,
            burst: f64::from(per_minute),
            capacity,
        }
    }

    /// Take one token for this client, or say how long until one is free.
    pub(crate) fn check(&self, ip: Option<IpAddr>, now: Instant) -> Result<(), Duration> {
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let mut key = key(ip);
        if !buckets.contains_key(&key) && buckets.len() >= self.capacity {
            // Forget clients whose bucket has refilled: they are owed nothing.
            let (rate, burst) = (self.per_second, self.burst);
            buckets.retain(|_, b| {
                b.tokens + now.saturating_duration_since(b.updated).as_secs_f64() * rate < burst
            });
            if buckets.len() >= self.capacity {
                key = Key::Overflow;
            }
        }
        let bucket = buckets.entry(key).or_insert(Bucket {
            tokens: self.burst,
            updated: now,
        });
        let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.per_second).min(self.burst);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            // Whole seconds for `Retry-After`; the epsilon absorbs float noise.
            let wait = (1.0 - bucket.tokens) / self.per_second;
            Err(Duration::from_secs_f64((wait - 1e-9).ceil().max(1.0)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xff(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for v in values {
            headers.append("x-forwarded-for", v.parse().unwrap());
        }
        headers
    }
    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn trusted_proxies_parse_networks_and_refuse_ambiguity() {
        let t = TrustedProxies::parse(" 10.0.0.0/8, 192.168.1.7 ,fd00::/8").unwrap();
        assert!(t.contains(ip("10.2.3.4")));
        assert!(t.contains(ip("192.168.1.7")));
        assert!(!t.contains(ip("192.168.1.8")));
        assert!(t.contains(ip("fd12::1")));
        assert!(
            t.contains(ip("::ffff:10.9.9.9")),
            "IPv4-mapped IPv6 is IPv4"
        );
        assert!(!t.contains(ip("11.0.0.1")));
        assert_eq!(
            TrustedProxies::parse(" , ").unwrap(),
            TrustedProxies::default()
        );
        for bad in [
            "10.0.0.0/33",
            "10.0.0.1/8",
            "not-an-ip",
            "10.0.0.0/x",
            "::/129",
        ] {
            assert!(TrustedProxies::parse(bad).is_err(), "{bad}");
        }
        assert!(
            TrustedProxies::parse("0.0.0.0/0")
                .unwrap()
                .contains(ip("8.8.8.8"))
        );
    }

    #[test]
    fn forwarded_addresses_are_believed_only_from_trusted_proxies() {
        let trusted = TrustedProxies::parse("10.0.0.0/8").unwrap();
        let spoof = xff(&["203.0.113.9"]);
        // An untrusted peer's header is ignored: it cannot pick its own bucket.
        assert_eq!(
            client_ip(Some(ip("198.51.100.1")), &spoof, &trusted),
            Some(ip("198.51.100.1"))
        );
        assert_eq!(
            client_ip(Some(ip("198.51.100.1")), &spoof, &TrustedProxies::default()),
            Some(ip("198.51.100.1"))
        );
        // Through the proxy: the right-most entry the proxy appended.
        let chain = xff(&["1.1.1.1, 203.0.113.9"]);
        assert_eq!(
            client_ip(Some(ip("10.0.0.5")), &chain, &trusted),
            Some(ip("203.0.113.9"))
        );
        // Two trusted hops, and a header split over two lines.
        let two = xff(&["1.1.1.1, 203.0.113.9", "10.1.1.1"]);
        assert_eq!(
            client_ip(Some(ip("10.0.0.5")), &two, &trusted),
            Some(ip("203.0.113.9"))
        );
        // Garbage stops the walk at the last trusted hop; nothing left of it is used.
        let garbage = xff(&["1.1.1.1, nonsense"]);
        assert_eq!(
            client_ip(Some(ip("10.0.0.5")), &garbage, &trusted),
            Some(ip("10.0.0.5"))
        );
        // No header: the proxy itself.
        assert_eq!(
            client_ip(Some(ip("10.0.0.5")), &HeaderMap::new(), &trusted),
            Some(ip("10.0.0.5"))
        );
        assert_eq!(client_ip(None, &spoof, &trusted), None);
    }

    #[test]
    fn limiter_allows_a_burst_then_backs_off_and_recovers() {
        let limiter = Limiter::new(60);
        let (a, b, t0) = (
            Some(ip("203.0.113.1")),
            Some(ip("203.0.113.2")),
            Instant::now(),
        );
        for _ in 0..60 {
            assert!(limiter.check(a, t0).is_ok());
        }
        assert_eq!(limiter.check(a, t0), Err(Duration::from_secs(1)));
        assert!(limiter.check(b, t0).is_ok(), "another client is unaffected");
        // One token a second; valid recovery after backoff.
        assert!(limiter.check(a, t0 + Duration::from_secs(1)).is_ok());
        assert!(limiter.check(a, t0 + Duration::from_secs(1)).is_err());
        let slow = Limiter::new(20);
        let now = Instant::now();
        for _ in 0..20 {
            slow.check(a, now).unwrap();
        }
        assert_eq!(slow.check(a, now), Err(Duration::from_secs(3)));
    }

    #[test]
    fn ipv6_clients_share_their_64_and_the_table_is_bounded() {
        let limiter = Limiter::with_capacity(1, 2);
        let now = Instant::now();
        assert!(limiter.check(Some(ip("2001:db8:1:2::1")), now).is_ok());
        assert!(
            limiter.check(Some(ip("2001:db8:1:2::ffff")), now).is_err(),
            "same /64"
        );
        assert!(
            limiter.check(Some(ip("2001:db8:1:3::1")), now).is_ok(),
            "another /64"
        );
        // Table full of drained buckets: new clients share the overflow bucket.
        assert!(limiter.check(Some(ip("198.51.100.1")), now).is_ok());
        assert!(limiter.check(Some(ip("198.51.100.2")), now).is_err());
        assert_eq!(
            limiter.buckets.lock().unwrap().len(),
            3,
            "two clients plus overflow"
        );
        // Once old buckets refill they are forgotten and new clients get their own.
        let later = now + Duration::from_secs(120);
        assert!(limiter.check(Some(ip("198.51.100.3")), later).is_ok());
        assert!(limiter.buckets.lock().unwrap().len() <= 2);
    }
}
