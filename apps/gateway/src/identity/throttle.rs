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
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::http::HeaderMap;
use uuid::Uuid;

/// Distinct client buckets kept per replica. Past this, new addresses share
/// one overflow bucket, so an attacker with many addresses cannot grow memory.
const MAX_BUCKETS: usize = 10_000;

/// How often a full table may be swept for refilled buckets. In between, a
/// flood of new addresses goes to the overflow bucket instead of each paying
/// for a scan under the lock.
const SWEEP_EVERY: Duration = Duration::from_secs(1);

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
            // Incoming addresses are compared as IPv4, so a mapped network could never match.
            if let IpAddr::V6(v6) = addr {
                anyhow::ensure!(
                    v6.to_ipv4_mapped().is_none(),
                    "GATEWAY_TRUSTED_PROXIES: write IPv4-mapped {item:?} as an IPv4 network"
                );
            }
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

/// One `X-Forwarded-For` entry: an address, optionally with a port
/// (`1.2.3.4:5678`, `[2001:db8::1]:443`), as some load balancers send.
fn parse_hop(hop: &str) -> Option<IpAddr> {
    if let Ok(ip) = hop.parse() {
        return Some(ip);
    }
    if let Some(rest) = hop.strip_prefix('[') {
        let (addr, port) = rest.split_once("]:")?;
        port.parse::<u16>().ok()?;
        return addr.parse::<Ipv6Addr>().ok().map(IpAddr::V6);
    }
    let (addr, port) = hop.split_once(':')?;
    port.parse::<u16>().ok()?;
    addr.parse::<Ipv4Addr>().ok().map(IpAddr::V4)
}

/// The client address to rate-limit. The connection's peer, unless the peer
/// is a trusted proxy: then the right-most `X-Forwarded-For` entry that is not
/// itself a trusted proxy. The walk ends at an unparsable entry or at a header
/// line that isn't visible ASCII, and nothing left of it is used (the client
/// may have written it), so such a request counts under the last trusted hop.
/// Without a peer (in-process tests), `None`.
pub(crate) fn client_ip(
    peer: Option<IpAddr>,
    headers: &HeaderMap,
    trusted: &TrustedProxies,
) -> Option<IpAddr> {
    let mut current = canonical(peer?);
    if !trusted.contains(current) {
        return Some(current);
    }
    // Right to left over every line, then every entry of the line.
    let lines: Vec<Option<&str>> = headers
        .get_all("x-forwarded-for")
        .iter()
        .map(|v| v.to_str().ok())
        .collect();
    for line in lines.into_iter().rev() {
        let Some(line) = line else {
            return Some(current);
        };
        for hop in line.rsplit(',') {
            let Some(ip) = parse_hop(hop.trim()) else {
                return Some(current);
            };
            current = canonical(ip);
            if !trusted.contains(current) {
                return Some(current);
            }
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

#[derive(Default)]
struct Table {
    buckets: HashMap<Key, Bucket>,
    swept: Option<Instant>,
}

/// A token bucket per client address: `per_minute` requests a minute, with a
/// burst of the same size.
pub(crate) struct Limiter {
    table: Mutex<Table>,
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
            table: Mutex::default(),
            per_second: f64::from(per_minute) / 60.0,
            burst: f64::from(per_minute),
            capacity,
        }
    }

    /// Take one token for this client, or say how long until one is free.
    pub(crate) fn check(&self, ip: Option<IpAddr>, now: Instant) -> Result<(), Duration> {
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        let mut key = key(ip);
        if !table.buckets.contains_key(&key) && table.buckets.len() >= self.capacity {
            // Forget clients whose bucket has refilled (they are owed nothing),
            // at most once per SWEEP_EVERY.
            if table
                .swept
                .is_none_or(|at| now.saturating_duration_since(at) >= SWEEP_EVERY)
            {
                table.swept = Some(now);
                let (rate, burst) = (self.per_second, self.burst);
                table.buckets.retain(|_, b| {
                    b.tokens + now.saturating_duration_since(b.updated).as_secs_f64() * rate < burst
                });
            }
            if table.buckets.len() >= self.capacity {
                key = Key::Overflow;
            }
        }
        let bucket = table.buckets.entry(key).or_insert(Bucket {
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

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.table.lock().unwrap().buckets.len()
    }
}

/// A bucket keyed by an admin-token id, or one shared overflow bucket once
/// the table is full (#37 PR2 finding 3): bounded-memory per-token admission
/// limiting, with the same capped/sweep/overflow design as [`Limiter`]
/// above, reused rather than duplicated as a second map-keyed-by-address
/// type. Checked only *after* a token has already authenticated (the key
/// here is never an attacker-chosen value pre-authentication: nothing
/// before that point allocates an entry for an arbitrary unauthenticated
/// token id), and before the attempt is started or dispatched.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum TokenKey {
    Token(Uuid),
    /// The bucket table is full; every token past the cap shares this one
    /// bucket, so an attacker minting many tokens cannot grow memory.
    Overflow,
}

#[derive(Default)]
struct TokenTable {
    buckets: HashMap<TokenKey, Bucket>,
    swept: Option<Instant>,
}

pub(crate) struct TokenLimiter {
    table: Mutex<TokenTable>,
    per_second: f64,
    burst: f64,
    capacity: usize,
}

impl TokenLimiter {
    pub(crate) fn new(per_minute: u32) -> Self {
        Self::with_capacity(per_minute, MAX_BUCKETS)
    }

    fn with_capacity(per_minute: u32, capacity: usize) -> Self {
        Self {
            table: Mutex::default(),
            per_second: f64::from(per_minute) / 60.0,
            burst: f64::from(per_minute),
            capacity,
        }
    }

    /// Take one token for this admin-token id, or say how long until one is
    /// free. Identical sweep-then-overflow semantics to [`Limiter::check`].
    pub(crate) fn check(&self, token_id: Uuid, now: Instant) -> Result<(), Duration> {
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        let mut key = TokenKey::Token(token_id);
        if !table.buckets.contains_key(&key) && table.buckets.len() >= self.capacity {
            if table
                .swept
                .is_none_or(|at| now.saturating_duration_since(at) >= SWEEP_EVERY)
            {
                table.swept = Some(now);
                let (rate, burst) = (self.per_second, self.burst);
                table.buckets.retain(|_, b| {
                    b.tokens + now.saturating_duration_since(b.updated).as_secs_f64() * rate < burst
                });
            }
            if table.buckets.len() >= self.capacity {
                key = TokenKey::Overflow;
            }
        }
        let bucket = table.buckets.entry(key).or_insert(Bucket {
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
            let wait = (1.0 - bucket.tokens) / self.per_second;
            Err(Duration::from_secs_f64((wait - 1e-9).ceil().max(1.0)))
        }
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.table.lock().unwrap().buckets.len()
    }
}

/// One shared token bucket for every admin-token authentication attempt on
/// this replica, with no per-key map at all (#37 PR2 finding 3): O(1)
/// memory, so there is nothing here for a flood of distinct client
/// addresses or token-shaped credentials to grow unbounded. Checked
/// *before* the expensive, one-database-round-trip authentication lookup,
/// bounding distributed floods (many addresses, each under the per-IP
/// limit) that the per-IP [`Limiter`] alone cannot.
pub(crate) struct ReplicaLimiter {
    bucket: Mutex<Bucket>,
    per_second: f64,
    burst: f64,
}

impl ReplicaLimiter {
    pub(crate) fn new(per_minute: u32) -> Self {
        Self {
            bucket: Mutex::new(Bucket {
                tokens: f64::from(per_minute),
                updated: Instant::now(),
            }),
            per_second: f64::from(per_minute) / 60.0,
            burst: f64::from(per_minute),
        }
    }

    pub(crate) fn check(&self, now: Instant) -> Result<(), Duration> {
        let mut bucket = self.bucket.lock().unwrap_or_else(|e| e.into_inner());
        let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.per_second).min(self.burst);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let wait = (1.0 - bucket.tokens) / self.per_second;
            Err(Duration::from_secs_f64((wait - 1e-9).ceil().max(1.0)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

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
            "::ffff:10.0.0.0/104",
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
        let peer = |s| Some(ip(s));
        let spoof = xff(&["203.0.113.9"]);
        // An untrusted peer's header is ignored: it cannot pick its own bucket.
        assert_eq!(
            client_ip(peer("198.51.100.1"), &spoof, &trusted),
            peer("198.51.100.1")
        );
        let none = TrustedProxies::default();
        assert_eq!(
            client_ip(peer("198.51.100.1"), &spoof, &none),
            peer("198.51.100.1")
        );
        // Through the proxy: the right-most entry the proxy appended.
        let chain = xff(&["1.1.1.1, 203.0.113.9"]);
        assert_eq!(
            client_ip(peer("10.0.0.5"), &chain, &trusted),
            peer("203.0.113.9")
        );
        // Two trusted hops, and a header split over two lines.
        let two = xff(&["1.1.1.1, 203.0.113.9", "10.1.1.1"]);
        assert_eq!(
            client_ip(peer("10.0.0.5"), &two, &trusted),
            peer("203.0.113.9")
        );
        // Garbage stops the walk at the last trusted hop; nothing left of it is used.
        let garbage = xff(&["1.1.1.1, nonsense"]);
        assert_eq!(
            client_ip(peer("10.0.0.5"), &garbage, &trusted),
            peer("10.0.0.5")
        );
        // So does a line that isn't visible ASCII, even if lines before it parse.
        let mut opaque = xff(&["9.9.9.9"]);
        opaque.append(
            "x-forwarded-for",
            HeaderValue::from_bytes(b"\xff, 203.0.113.9").unwrap(),
        );
        assert_eq!(
            client_ip(peer("10.0.0.5"), &opaque, &trusted),
            peer("10.0.0.5")
        );
        // No header: the proxy itself.
        let empty = HeaderMap::new();
        assert_eq!(
            client_ip(peer("10.0.0.5"), &empty, &trusted),
            peer("10.0.0.5")
        );
        assert_eq!(client_ip(None, &spoof, &trusted), None);
        // Ports are ignored; an IPv4-mapped peer is its IPv4 address.
        let ported = xff(&["9.9.9.9, 203.0.113.9:5678"]);
        assert_eq!(
            client_ip(peer("10.0.0.5"), &ported, &trusted),
            peer("203.0.113.9")
        );
        let v6 = xff(&["[2001:db8::7]:443"]);
        assert_eq!(
            client_ip(peer("::ffff:10.0.0.5"), &v6, &trusted),
            peer("2001:db8::7")
        );
        assert_eq!(
            client_ip(peer("::ffff:198.51.100.1"), &spoof, &trusted),
            peer("198.51.100.1")
        );
        assert_eq!(parse_hop("1.2.3.4:notaport"), None);
        assert_eq!(parse_hop("[2001:db8::1]"), None);
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
        assert_eq!(limiter.tracked(), 3, "two clients plus overflow");
        // Once old buckets refill they are forgotten and new clients get their own.
        let later = now + Duration::from_secs(120);
        assert!(limiter.check(Some(ip("198.51.100.3")), later).is_ok());
        assert!(limiter.tracked() <= 2);
    }

    #[test]
    fn token_limiter_is_bounded_per_token_and_shares_an_overflow_bucket() {
        let limiter = TokenLimiter::with_capacity(1, 2);
        let now = Instant::now();
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        assert!(limiter.check(a, now).is_ok());
        assert!(
            limiter.check(a, now).is_err(),
            "this token's own bucket is empty"
        );
        assert!(
            limiter.check(b, now).is_ok(),
            "a different token is unaffected"
        );
        // Table full: a third distinct token id shares the overflow bucket.
        assert!(limiter.check(c, now).is_ok());
        assert_eq!(limiter.tracked(), 3, "two tokens plus overflow");
        let mut other = Uuid::new_v4();
        while other == a || other == b || other == c {
            other = Uuid::new_v4();
        }
        assert!(
            limiter.check(other, now).is_err(),
            "the overflow bucket is already spent by c"
        );
        // Once old buckets refill they are forgotten and new tokens get their own.
        let later = now + Duration::from_secs(120);
        assert!(limiter.check(other, later).is_ok());
        assert!(limiter.tracked() <= 2);
    }

    #[test]
    fn replica_limiter_is_one_shared_bucket_with_no_per_key_map() {
        let limiter = ReplicaLimiter::new(2);
        let now = Instant::now();
        assert!(limiter.check(now).is_ok());
        assert!(limiter.check(now).is_ok());
        assert_eq!(
            limiter.check(now),
            Err(Duration::from_secs(30)),
            "the shared bucket is now empty, regardless of who is asking"
        );
        assert!(limiter.check(now + Duration::from_secs(30)).is_ok());
    }

    #[test]
    fn a_flood_of_new_addresses_sweeps_at_most_once_a_second() {
        // Ten tokens a second, so a used bucket refills within 0.1 s.
        let limiter = Limiter::with_capacity(600, 2);
        let now = Instant::now();
        for client in ["198.51.100.1", "198.51.100.2"] {
            limiter.check(Some(ip(client)), now).unwrap();
        }
        // Full: this sweep finds nothing refilled, so overflow.
        limiter.check(Some(ip("198.51.100.3")), now).unwrap();
        assert_eq!(limiter.tracked(), 3);
        // The first two have refilled by now, but within a second of the last
        // sweep new clients go straight to overflow instead of scanning.
        let soon = now + Duration::from_millis(900);
        limiter.check(Some(ip("198.51.100.4")), soon).unwrap();
        assert_eq!(limiter.tracked(), 3, "not swept again yet");
        let after = now + Duration::from_secs(1);
        limiter.check(Some(ip("198.51.100.5")), after).unwrap();
        assert_eq!(limiter.tracked(), 1, "swept: only the new client remains");
    }
}
