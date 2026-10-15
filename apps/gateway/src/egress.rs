//! Outbound address policy (#33, #167): which addresses the gateway may
//! connect to, enforced at DNS resolution for every HTTP client.
//!
//! - Always refused: unspecified, link-local (which includes cloud metadata
//!   at 169.254.169.254), the IPv6 metadata address `fd00:ec2::254`,
//!   multicast, broadcast, reserved and translation ranges.
//! - Loopback: only with `GATEWAY_ENV=development`.
//! - Private (RFC 1918, `100.64.0.0/10` CGNAT and Tailscale, IPv6 unique
//!   local): only inside `GATEWAY_EGRESS_PRIVATE_ALLOW`.
//! - Everything else (public unicast): allowed.
//!
//! The resolver drops refused addresses before the client connects, so the
//! address that was checked is the one connected to (DNS rebinding can't swap
//! it). A URL whose host is an IP literal never reaches a resolver; such URLs
//! come from configuration and are checked with [`check_url`] when loaded.
//! Approved local upstreams keep their own stricter, pinned approval
//! (`providers/local/endpoints.rs`).

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, OnceLock},
};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Env var listing the private networks the gateway may reach.
pub const PRIVATE_ALLOW_ENV: &str = "GATEWAY_EGRESS_PRIVATE_ALLOW";

/// What an outbound connection is for: a metric label and a log field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Provider,
    Oidc,
    Storage,
    Catalog,
    Smtp,
    Audit,
}
impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Oidc => "oidc",
            Self::Storage => "storage",
            Self::Catalog => "catalog",
            Self::Smtp => "smtp",
            Self::Audit => "audit",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Refused,
    Loopback,
    Private,
    Public,
}

fn classify(ip: IpAddr) -> Class {
    match ip {
        IpAddr::V4(ip) => classify_v4(ip),
        IpAddr::V6(ip) => {
            if ip.is_loopback() {
                return Class::Loopback;
            }
            // Mapped and deprecated IPv4-compatible encodings are the IPv4 address.
            if let Some(v4) = ip.to_ipv4() {
                return classify_v4(v4);
            }
            let s = ip.segments();
            // Translation ranges carry an IPv4 destination: judge that one, so
            // IPv6-only clusters (DNS64/NAT64) still reach IPv4-only upstreams
            // and nothing private or metadata hides behind them.
            let low = (u32::from(s[6]) << 16) | u32::from(s[7]);
            if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
                return classify_v4(Ipv4Addr::from(low)); // NAT64 64:ff9b::/96
            }
            if s[0] == 0x2002 {
                return classify_v4(Ipv4Addr::from((u32::from(s[1]) << 16) | u32::from(s[2]))); // 6to4
            }
            if s[0] == 0x2001 && s[1] == 0 {
                return classify_v4(Ipv4Addr::from(!low)); // Teredo: the client's address, inverted
            }
            if ip.is_unspecified()
                || ip.is_unicast_link_local()
                || ip.is_multicast()
                // AWS metadata lives at fd00:ec2::254; refuse its whole /32.
                || (s[0] == 0xfd00 && s[1] == 0x0ec2)
                // Deprecated site-local fec0::/10.
                || (s[0] & 0xffc0) == 0xfec0
            {
                return Class::Refused;
            }
            if ip.is_unique_local() {
                return Class::Private;
            }
            // Public unicast is 2000::/3, minus documentation (2001:db8::/32,
            // 3fff::/20), benchmarking (2001:2::/48) and other translation ranges.
            let documentation = (s[0] == 0x2001 && s[1] == 0xdb8) || (s[0] & 0xfff0) == 0x3ff0;
            let benchmarking = s[0] == 0x2001 && s[1] == 2 && s[2] == 0;
            if (s[0] & 0xe000) == 0x2000 && !documentation && !benchmarking {
                Class::Public
            } else {
                Class::Refused
            }
        }
    }
}

fn classify_v4(ip: Ipv4Addr) -> Class {
    let o = ip.octets();
    if ip.is_unspecified()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || o[0] == 0
        || o[0] >= 240
        // 192.0.0.0/24 (IETF protocol assignments), 198.18.0.0/15
        // (benchmarking), 192.88.99.0/24 (deprecated 6to4 relays).
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
        || (o[0] == 192 && o[1] == 88 && o[2] == 99)
        // Alibaba Cloud metadata, inside 100.64.0.0/10: never, even when that
        // range is listed for Tailscale.
        || ip == Ipv4Addr::new(100, 100, 100, 200)
    {
        return Class::Refused;
    }
    if ip.is_loopback() {
        return Class::Loopback;
    }
    if ip.is_private() || (o[0] == 100 && (64..=127).contains(&o[1])) {
        return Class::Private;
    }
    Class::Public
}

/// Comma-separated CIDR networks (or single addresses).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Networks(Vec<(IpAddr, u8)>);

impl Networks {
    pub fn parse(name: &str, value: &str) -> anyhow::Result<Self> {
        let mut networks = Vec::new();
        for item in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (addr, prefix) = item
                .split_once('/')
                .map_or((item, None), |(a, p)| (a, Some(p)));
            let addr: IpAddr = addr
                .parse()
                .map_err(|_| anyhow::anyhow!("{name}: invalid address {item:?}"))?;
            if let IpAddr::V6(v6) = addr {
                anyhow::ensure!(
                    v6.to_ipv4_mapped().is_none(),
                    "{name}: write IPv4-mapped {item:?} as an IPv4 network"
                );
            }
            let max = if addr.is_ipv4() { 32 } else { 128 };
            let prefix = match prefix {
                None => max,
                Some(p) => p
                    .parse::<u8>()
                    .ok()
                    .filter(|p| *p <= max)
                    .ok_or_else(|| anyhow::anyhow!("{name}: invalid prefix in {item:?}"))?,
            };
            anyhow::ensure!(
                masked(addr, prefix) == addr,
                "{name}: {item:?} has host bits set"
            );
            networks.push((addr, prefix));
        }
        Ok(Self(networks))
    }

    fn contains(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        self.0
            .iter()
            .any(|&(net, prefix)| net.is_ipv4() == ip.is_ipv4() && masked(ip, prefix) == net)
    }
}

/// IPv4-mapped and IPv4-compatible forms are the IPv4 address (as in
/// `classify`); `::` and `::1` stay IPv6.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) if !v6.is_loopback() && !v6.is_unspecified() => {
            v6.to_ipv4().map_or(ip, IpAddr::V4)
        }
        other => other,
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

/// The installation's outbound policy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    development: bool,
    private_allow: Networks,
}

impl Policy {
    pub fn new(development: bool, private_allow: Networks) -> Self {
        Self {
            development,
            private_allow,
        }
    }

    /// `GATEWAY_ENV=development` and `GATEWAY_EGRESS_PRIVATE_ALLOW`.
    pub fn from_env() -> anyhow::Result<Self> {
        let development = std::env::var("GATEWAY_ENV").as_deref() == Ok("development");
        let private_allow = match std::env::var(PRIVATE_ALLOW_ENV) {
            Ok(value) => Networks::parse(PRIVATE_ALLOW_ENV, &value)?,
            Err(std::env::VarError::NotPresent) => Networks::default(),
            Err(_) => anyhow::bail!("{PRIVATE_ALLOW_ENV} is not valid Unicode"),
        };
        Ok(Self::new(development, private_allow))
    }

    /// This policy, with loopback also allowed when `loopback` (OIDC: a
    /// development issuer on loopback was already approved by its URL rules).
    pub fn permit_loopback(&self, loopback: bool) -> Self {
        Self {
            development: self.development || loopback,
            private_allow: self.private_allow.clone(),
        }
    }

    pub fn allows(&self, ip: IpAddr) -> bool {
        match classify(ip) {
            Class::Refused => false,
            Class::Loopback => self.development,
            Class::Private => self.private_allow.contains(ip),
            Class::Public => true,
        }
    }

    /// Why `ip` is refused, in words an operator can act on.
    fn refusal(&self, ip: IpAddr) -> &'static str {
        match classify(ip) {
            Class::Refused => "a reserved, link-local or metadata address, never allowed",
            Class::Loopback => "loopback, allowed only with GATEWAY_ENV=development",
            Class::Private => "a private address not in GATEWAY_EGRESS_PRIVATE_ALLOW",
            Class::Public => "allowed",
        }
    }
}

static POLICY: OnceLock<Arc<Policy>> = OnceLock::new();

/// Set the process-wide policy once, at startup (before any client is built).
pub fn init(policy: Policy) -> anyhow::Result<()> {
    POLICY
        .set(Arc::new(policy))
        .map_err(|_| anyhow::anyhow!("egress policy already initialised"))
}

/// The process-wide policy; the strictest one (no loopback, no private
/// networks) until [`init`] runs.
pub fn policy() -> Arc<Policy> {
    POLICY.get_or_init(|| Arc::new(Policy::default())).clone()
}

fn denied(purpose: Purpose, host: &str, ip: IpAddr, policy: &Policy) {
    crate::metrics::METRICS.egress_denied(purpose.as_str());
    tracing::warn!(
        purpose = purpose.as_str(),
        host,
        address = %ip,
        reason = policy.refusal(ip),
        "outbound connection refused by the egress policy"
    );
}

/// Keep the addresses the policy allows. Refusal is counted and logged only
/// when none remain (a dual-stack host with one refused record still works,
/// and that's not an incident); partial drops are debug logs.
fn filter(
    policy: &Policy,
    purpose: Purpose,
    host: &str,
    found: Vec<SocketAddr>,
) -> anyhow::Result<Vec<SocketAddr>> {
    let (allowed, refused): (Vec<_>, Vec<_>) =
        found.iter().copied().partition(|a| policy.allows(a.ip()));
    if !allowed.is_empty() {
        for addr in &refused {
            tracing::debug!(purpose = purpose.as_str(), host, address = %addr.ip(), reason = policy.refusal(addr.ip()), "egress policy dropped one resolved address");
        }
        return Ok(allowed);
    }
    let Some(first) = refused.first() else {
        anyhow::bail!("could not resolve {host}");
    };
    denied(purpose, host, first.ip(), policy);
    anyhow::bail!(
        "{} connection to {host} refused: {} is {} (see {PRIVATE_ALLOW_ENV})",
        purpose.as_str(),
        first.ip(),
        policy.refusal(first.ip())
    )
}

/// Resolve `host`, keep only the addresses the policy allows. An error when
/// none remain names the setting to change.
pub async fn resolve(
    policy: &Policy,
    purpose: Purpose,
    host: &str,
    port: u16,
) -> anyhow::Result<Vec<SocketAddr>> {
    let found: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| anyhow::anyhow!("could not resolve {host}"))?
        .collect();
    filter(policy, purpose, host, found)
}

/// Check a configured URL whose host may be an IP literal (those never reach
/// the resolver). Hostnames pass here and are checked at resolution.
pub fn check_url(policy: &Policy, purpose: Purpose, url: &reqwest::Url) -> anyhow::Result<()> {
    // IPv6 literals come back bracketed from `host_str`.
    let Some(ip) = url.host_str().and_then(|h| {
        h.trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .ok()
    }) else {
        return Ok(());
    };
    if policy.allows(ip) {
        return Ok(());
    }
    denied(purpose, &ip.to_string(), ip, policy);
    anyhow::bail!(
        "{} URL refused: {ip} is {} (see {PRIVATE_ALLOW_ENV})",
        purpose.as_str(),
        policy.refusal(ip)
    )
}

/// Startup check of configured endpoints that may be IP literals (which the
/// resolver never sees): the S3 endpoint and the Bedrock endpoint allowlist.
/// Hostnames are checked when they resolve.
pub fn check_configured_endpoints(
    policy: &Policy,
    get: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<()> {
    let mut urls = Vec::new();
    if let Some(endpoint) = get(crate::filestore::s3::ENDPOINT_ENV) {
        urls.push((Purpose::Storage, endpoint));
    }
    if let Some(list) = get(crate::providers::bedrock::auth::ENDPOINT_ALLOWLIST_ENV) {
        urls.extend(
            list.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| (Purpose::Provider, s.to_owned())),
        );
    }
    for (purpose, raw) in urls {
        // Malformed values are reported by their own validation.
        if let Ok(url) = reqwest::Url::parse(&raw) {
            check_url(policy, purpose, &url)?;
        }
    }
    Ok(())
}

/// A reqwest DNS resolver that applies the policy.
pub struct Resolver {
    purpose: Purpose,
    policy: Option<Arc<Policy>>,
}

/// The resolver for every reqwest client: `.dns_resolver(egress::resolver(..))`.
/// Uses the process-wide policy as it is when the request resolves.
pub fn resolver(purpose: Purpose) -> Arc<Resolver> {
    Arc::new(Resolver {
        purpose,
        policy: None,
    })
}

/// A resolver with an explicit policy (OIDC uses its own development flag).
pub fn resolver_with(purpose: Purpose, policy: Policy) -> Arc<Resolver> {
    Arc::new(Resolver {
        purpose,
        policy: Some(Arc::new(policy)),
    })
}

impl Resolve for Resolver {
    fn resolve(&self, name: Name) -> Resolving {
        let (purpose, policy) = (self.purpose, self.policy.clone().unwrap_or_else(policy));
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs = resolve(&policy, purpose, &host, 0).await?;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn addresses_are_classified_strictly() {
        let strict = Policy::default();
        for refused in [
            "0.0.0.0",
            "169.254.169.254",
            "169.254.1.1",
            "::ffff:169.254.169.254",
            "::169.254.169.254",
            "fd00:ec2::254",
            "fe80::1",
            "224.0.0.1",
            "255.255.255.255",
            "240.0.0.1",
            "0.1.2.3",
            "192.0.0.8",
            "198.18.0.1",
            "192.0.2.1",
            "ff02::1",
            "::",
            "64:ff9b::a9fe:a9fe",
            "2002:a9fe:a9fe::1",
            "2001:0:4136:e378:8000:63bf:5601:5601",
            "2001:db8::1",
            "3fff::1",
            "2001:2::1",
            "fec0::1",
            "fd00:ec2::1",
            "100.100.100.200",
            "192.88.99.1",
        ] {
            assert_eq!(classify(ip(refused)), Class::Refused, "{refused}");
            assert!(!strict.allows(ip(refused)));
        }
        for private in [
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "100.127.255.254",
            "fd12::1",
            "::ffff:10.0.0.1",
        ] {
            assert_eq!(classify(ip(private)), Class::Private, "{private}");
            assert!(!strict.allows(ip(private)), "{private}");
        }
        for loopback in ["127.0.0.1", "127.1.2.3", "::1", "::ffff:127.0.0.1"] {
            assert_eq!(classify(ip(loopback)), Class::Loopback, "{loopback}");
            assert!(!strict.allows(ip(loopback)));
        }
        for public in [
            "8.8.8.8",
            "1.1.1.1",
            "100.128.0.1",
            "2606:4700::1111",
            "2a00:1450::1",
        ] {
            assert_eq!(classify(ip(public)), Class::Public, "{public}");
            assert!(strict.allows(ip(public)));
        }
    }

    #[test]
    fn private_networks_and_loopback_need_explicit_approval() {
        let allow = Networks::parse(
            PRIVATE_ALLOW_ENV,
            "10.20.0.0/16, 100.64.0.0/10, fd00:1::/32",
        )
        .unwrap();
        let policy = Policy::new(false, allow.clone());
        assert!(policy.allows(ip("10.20.3.4")));
        assert!(
            !policy.allows(ip("10.21.0.1")),
            "outside the listed network"
        );
        assert!(policy.allows(ip("100.100.1.1")), "Tailscale, when listed");
        assert!(policy.allows(ip("fd00:1::5")));
        assert!(!policy.allows(ip("127.0.0.1")));
        // Listing a network never opens refused ranges.
        let everything = Policy::new(
            true,
            Networks::parse(PRIVATE_ALLOW_ENV, "0.0.0.0/0,::/0").unwrap(),
        );
        for refused in [
            "169.254.169.254",
            "fd00:ec2::254",
            "0.0.0.0",
            "224.0.0.1",
            "100.100.100.200",
        ] {
            assert!(!everything.allows(ip(refused)), "{refused}");
        }
        // IPv4-compatible forms match the IPv4 network they encode.
        assert!(policy.allows(ip("::10.20.3.4")));
        assert!(
            !Policy::new(false, Networks::parse(PRIVATE_ALLOW_ENV, "::/0").unwrap())
                .allows(ip("::10.20.3.4"))
        );
        assert!(
            Policy::new(true, Networks::default()).allows(ip("127.0.0.1")),
            "development loopback"
        );
        for bad in ["10.0.0.1/8", "10.0.0.0/33", "nope", "::ffff:10.0.0.0/104"] {
            assert!(Networks::parse(PRIVATE_ALLOW_ENV, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ip_literal_urls_are_checked_and_hostnames_deferred() {
        let strict = Policy::default();
        let url = |s: &str| reqwest::Url::parse(s).unwrap();
        assert!(
            check_url(
                &strict,
                Purpose::Storage,
                &url("https://169.254.169.254/latest")
            )
            .is_err()
        );
        let error = check_url(&strict, Purpose::Oidc, &url("https://10.0.0.5/realm"))
            .unwrap_err()
            .to_string();
        assert!(error.contains(PRIVATE_ALLOW_ENV), "{error}");
        assert!(check_url(&strict, Purpose::Oidc, &url("https://[fd00:ec2::254]/")).is_err());
        assert!(check_url(&strict, Purpose::Provider, &url("https://8.8.8.8/")).is_ok());
        assert!(
            check_url(
                &strict,
                Purpose::Provider,
                &url("https://internal.example/")
            )
            .is_ok(),
            "checked at resolution"
        );
    }

    #[test]
    fn mixed_answers_keep_the_allowed_addresses_and_count_only_total_refusal() {
        // Its own metric label: the counter is process-wide and tests run in parallel.
        let strict = Policy::default();
        let sock = |s: &str| SocketAddr::new(ip(s), 443);
        let before = crate::metrics::METRICS.egress_denied_count("smtp");
        // A rebinding-style answer: the private address is dropped, the public one kept.
        let kept = filter(
            &strict,
            Purpose::Smtp,
            "mixed.example",
            vec![sock("10.0.0.1"), sock("8.8.8.8"), sock("169.254.169.254")],
        )
        .unwrap();
        assert_eq!(kept, vec![sock("8.8.8.8")]);
        assert_eq!(
            crate::metrics::METRICS.egress_denied_count("smtp"),
            before,
            "a working connection is not a refusal"
        );
        assert!(
            filter(
                &strict,
                Purpose::Smtp,
                "private.example",
                vec![sock("10.0.0.1")]
            )
            .is_err()
        );
        assert_eq!(
            crate::metrics::METRICS.egress_denied_count("smtp"),
            before + 1
        );
        assert!(
            filter(&strict, Purpose::Smtp, "empty.example", vec![])
                .unwrap_err()
                .to_string()
                .contains("could not resolve")
        );
    }

    /// Every HTTP client the gateway builds goes through the policy: a new or
    /// edited `Client::builder()` without the resolver fails this test.
    #[test]
    fn every_http_client_uses_the_egress_resolver() {
        fn files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    files(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut all = Vec::new();
        files(&root, &mut all);
        let mut checked = 0;
        for path in all {
            let name = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if name.ends_with("tests.rs")
                || name.contains("_tests")
                || name.contains("/tests/")
                || name == "egress.rs"
            {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            // Test code inside a production file starts at its `#[cfg(test)]` module.
            let production = source.split("#[cfg(test)]\nmod tests").next().unwrap();
            for (at, _) in production.match_indices("Client::builder()") {
                let before = &production[..at];
                if before
                    .rsplit_once("fn ")
                    .is_some_and(|(_, f)| f.starts_with("for_test"))
                {
                    continue; // a test-only constructor
                }
                let window: String = production[at..].chars().take(700).collect();
                assert!(
                    window.contains("dns_resolver(") || window.contains("resolve_to_addrs("),
                    "{name}: a reqwest client without the egress resolver"
                );
                checked += 1;
            }
        }
        assert!(checked >= 8, "found {checked} clients; did the scan break?");
    }

    #[test]
    fn configured_ip_literal_endpoints_are_checked_at_startup() {
        let strict = Policy::default();
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        assert!(check_configured_endpoints(&strict, env(&[])).is_ok());
        assert!(
            check_configured_endpoints(
                &strict,
                env(&[("GATEWAY_S3_ENDPOINT", "https://s3.example.org")])
            )
            .is_ok()
        );
        let error = check_configured_endpoints(
            &strict,
            env(&[("GATEWAY_S3_ENDPOINT", "http://10.1.2.3:9000")]),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("storage") && error.contains(PRIVATE_ALLOW_ENV),
            "{error}"
        );
        assert!(
            check_configured_endpoints(
                &strict,
                env(&[(
                    "GATEWAY_BEDROCK_ENDPOINT_ALLOWLIST",
                    "https://bedrock.example.org, https://169.254.169.254"
                )])
            )
            .is_err()
        );
        let allowed = Policy::new(
            false,
            Networks::parse(PRIVATE_ALLOW_ENV, "10.0.0.0/8").unwrap(),
        );
        assert!(
            check_configured_endpoints(
                &allowed,
                env(&[("GATEWAY_S3_ENDPOINT", "http://10.1.2.3:9000")])
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn resolution_keeps_only_allowed_addresses() {
        // `localhost` resolves to loopback: refused unless development.
        let strict = Policy::default();
        let error = resolve(&strict, Purpose::Oidc, "localhost", 443)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("loopback"), "{error}");
        let development = Policy::new(true, Networks::default());
        let addrs = resolve(&development, Purpose::Oidc, "localhost", 443)
            .await
            .unwrap();
        assert!(
            addrs
                .iter()
                .all(|a| a.ip().is_loopback() && a.port() == 443)
        );
        // A literal resolves to itself and is filtered the same way.
        assert!(
            resolve(&strict, Purpose::Provider, "169.254.169.254", 80)
                .await
                .is_err()
        );
        let before = crate::metrics::METRICS.egress_denied_count("catalog");
        assert!(
            resolve(&strict, Purpose::Catalog, "10.9.9.9", 443)
                .await
                .is_err()
        );
        assert!(crate::metrics::METRICS.egress_denied_count("catalog") > before);
    }

    /// The resolver is what a real client connects through: a refused
    /// address is never connected to, an allowed one is.
    #[tokio::test]
    async fn a_client_never_connects_to_a_refused_address() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = accepted.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                use tokio::io::AsyncWriteExt;
                let _ = stream
                    .write_all(b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n")
                    .await;
            }
        });
        let client = |policy: Policy| {
            reqwest::Client::builder()
                .no_proxy()
                .dns_resolver(resolver_with(Purpose::Provider, policy))
                .build()
                .unwrap()
        };
        let url = format!("http://localhost:{port}/");
        let before = crate::metrics::METRICS.egress_denied_count("provider");
        assert!(client(Policy::default()).get(&url).send().await.is_err());
        assert_eq!(
            accepted.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "never connected"
        );
        assert!(crate::metrics::METRICS.egress_denied_count("provider") > before);
        let ok = client(Policy::new(true, Networks::default()))
            .get(&url)
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 204);
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
