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
//!
//! Refusal logging has a per-[`Purpose`] policy ([`Purpose::logging_policy`],
//! #321): most purposes name the refused host and resolved address on every
//! attempt, which is what operators expect for provider, OIDC, storage,
//! catalog, SMTP, audit (webhook/syslog) and alert-webhook egress.
//! Telemetry/OTLP export destinations, and the OTLP audit exporter's
//! destination (a distinct [`Purpose::AuditOtlp`], never the webhook/syslog
//! [`Purpose::Audit`]), are resolved on every export attempt - as often as
//! the metrics push interval - so those two purposes instead log a fixed,
//! host-free warning only when connectivity actually changes (into or out
//! of refusal), matching the exporters' own transition-only, fixed-enum
//! diagnostics. This also applies to the dual-stack partial-drop DEBUG
//! diagnostic (mixed allowed/refused DNS answers): on every attempt for
//! these two purposes, never the dropped address. The
//! `gateway_egress_denied_total{purpose}` counter and the address
//! enforcement itself are unchanged for every purpose.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
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
    /// The audit export destination only when it is an OTLP collector
    /// (#321 round 2): a distinct purpose from [`Self::Audit`] (which stays
    /// the webhook/syslog destination's purpose) so the OTLP collector's
    /// resolver, built once and reused for every delivery, can apply the
    /// same transition-only, host-free logging the telemetry exporters use
    /// without quieting the webhook/syslog diagnostics operators rely on.
    AuditOtlp,
    Telemetry,
    AlertWebhook,
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
            Self::AuditOtlp => "audit_otlp",
            Self::Telemetry => "telemetry",
            Self::AlertWebhook => "alert_webhook",
        }
    }

    /// Whether this purpose's egress refusals are logged (host and resolved
    /// address) on every attempt, or only when its connectivity state
    /// changes (#321: telemetry export's shared egress refusal logs
    /// repeated and named the collector host on every periodic attempt;
    /// round 2 extended this to the OTLP audit exporter, #321 LOW finding
    /// 2). Telemetry/OTLP export and the OTLP audit exporter (both build
    /// their one, reused-per-client resolver through
    /// `otlp::export_http_client`/`audit_export::https_client` with their
    /// own purpose) use the quieter policy; every other purpose, including
    /// the audit webhook/syslog destination's own [`Self::Audit`], is
    /// unchanged.
    fn logging_policy(self) -> LoggingPolicy {
        match self {
            Self::Telemetry | Self::AuditOtlp => LoggingPolicy::TransitionOnly,
            Self::Provider
            | Self::Oidc
            | Self::Storage
            | Self::Catalog
            | Self::Smtp
            | Self::Audit
            | Self::AlertWebhook => LoggingPolicy::Always,
        }
    }
}

/// See [`Purpose::logging_policy`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoggingPolicy {
    /// Name the refused host and resolved address on every refusal.
    Always,
    /// Log a fixed, host-free warning only on a connectivity transition
    /// (into or out of refusal); repeated refusals of the same state are
    /// silent (beyond the `gateway_egress_denied_total` counter, which
    /// still counts every one).
    TransitionOnly,
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
            // Translation ranges carry an IPv4 destination: judge that one, so
            // IPv6-only clusters (DNS64/NAT64) still reach IPv4-only upstreams
            // and nothing private or metadata hides behind them.
            if let Some(v4) = translated_v4(ip) {
                return classify_v4(v4);
            }
            // The shared special-use table (#203): AWS metadata lives at
            // fd00:ec2::254, but egress refuses its whole /32 as defense in
            // depth (`Category::AwsMetadataRange` and `Category::Metadata`
            // both land in the catch-all `Class::Refused` arm below).
            class_for(crate::special_use::classify_v6(ip))
        }
    }
}

/// `Category` has no notion of a private allowlist or a development flag:
/// this is egress's own mapping from "what kind of range" to "what the
/// policy does with it" (`Policy::allows`, gated separately).
fn class_for(category: crate::special_use::Category) -> Class {
    use crate::special_use::Disposition;
    match category.disposition() {
        Disposition::Public => Class::Public,
        // RFC1918 and carrier-grade NAT (IPv4), unique-local (IPv6): the same
        // gate, `GATEWAY_EGRESS_PRIVATE_ALLOW`.
        Disposition::Private | Disposition::Cgnat => Class::Private,
        Disposition::Loopback => Class::Loopback,
        // Never allowed, regardless of policy; decided once for both this
        // policy and local-upstream approvals by `Category::disposition`.
        Disposition::Refused => Class::Refused,
    }
}

/// The IPv4 destination behind an IPv6 translation range: NAT64
/// (`64:ff9b::/96`), 6to4 (`2002::/16`) or Teredo (`2001::/32`, the client's
/// address, inverted). Mapped and compatible forms are `Ipv6Addr::to_ipv4`.
pub(crate) fn translated_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    let low = (u32::from(s[6]) << 16) | u32::from(s[7]);
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        return Some(Ipv4Addr::from(low));
    }
    if s[0] == 0x2002 {
        return Some(Ipv4Addr::from((u32::from(s[1]) << 16) | u32::from(s[2])));
    }
    (s[0] == 0x2001 && s[1] == 0).then(|| Ipv4Addr::from(!low))
}

fn classify_v4(ip: Ipv4Addr) -> Class {
    class_for(crate::special_use::classify_v4(ip))
}

/// Comma-separated CIDR networks (or single addresses).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Networks(Vec<(IpAddr, u8)>);

/// One `addr/len` (or bare address) network: a parseable IP, `len` within
/// bounds (32 for IPv4, 128 for IPv6, defaulting to the host prefix when
/// omitted), no set host bits, and no IPv4-mapped IPv6 form (write it as
/// plain IPv4 instead). Shared by [`Networks::parse`] and
/// `admin_tokens`'s own allowlist validator (#37 PR1 coordinator review
/// issue 5b), so both apply exactly the same rules PostgreSQL's `cidr`
/// type would otherwise be the only thing enforcing.
pub(crate) fn parse_network(name: &str, item: &str) -> anyhow::Result<(IpAddr, u8)> {
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
    Ok((addr, prefix))
}

impl Networks {
    pub fn parse(name: &str, value: &str) -> anyhow::Result<Self> {
        let mut networks = Vec::new();
        for item in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            networks.push(parse_network(name, item)?);
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
    loopback_only: bool,
}

impl Policy {
    pub fn new(development: bool, private_allow: Networks) -> Self {
        Self {
            development,
            private_allow,
            loopback_only: false,
        }
    }

    /// Tighten development HTTP to actual loopback connections, even when
    /// a hostname approved as `localhost` resolves to a different address.
    pub(crate) fn only_loopback(mut self) -> Self {
        self.loopback_only = true;
        self
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
            loopback_only: self.loopback_only,
        }
    }

    pub fn allows(&self, ip: IpAddr) -> bool {
        if self.loopback_only && !canonical(ip).is_loopback() {
            return false;
        }
        match classify(ip) {
            Class::Refused => false,
            Class::Loopback => self.development,
            Class::Private => self.private_allow.contains(ip),
            Class::Public => true,
        }
    }

    /// Why `ip` is refused, in words an operator can act on.
    fn refusal(&self, ip: IpAddr) -> &'static str {
        if self.loopback_only && !canonical(ip).is_loopback() {
            return "not a loopback address (required for development HTTP)";
        }
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

fn denied(purpose: Purpose, host: &str, ip: IpAddr, policy: &Policy, transition: &AtomicBool) {
    crate::metrics::METRICS.egress_denied(purpose.as_str());
    if purpose.logging_policy() == LoggingPolicy::TransitionOnly {
        // #321: logged only on the transition into refusal, never the host
        // or resolved address, so a periodic export attempt that keeps
        // resolving to the same refused destination does not repeat it.
        // Relaxed: the flag only gates whether this log line itself is
        // printed; no other memory access is ordered against it, so the
        // swap's own atomicity (one logger wins the transition) is enough.
        if !transition.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                purpose = purpose.as_str(),
                reason = policy.refusal(ip),
                "outbound connection refused by the egress policy (destination omitted; repeated refusals are not re-logged until it recovers)"
            );
        }
        return;
    }
    tracing::warn!(
        purpose = purpose.as_str(),
        host,
        address = %ip,
        reason = policy.refusal(ip),
        "outbound connection refused by the egress policy"
    );
}

/// The transition-only counterpart to [`denied`]: logs once, host-free, the
/// first time a [`LoggingPolicy::TransitionOnly`] purpose becomes reachable
/// again after having been refused. A no-op for `Always` purposes and for a
/// transition flag that is already clear.
fn recovered(purpose: Purpose, transition: &AtomicBool) {
    if purpose.logging_policy() == LoggingPolicy::TransitionOnly
        && transition.swap(false, Ordering::Relaxed)
    {
        tracing::warn!(
            purpose = purpose.as_str(),
            "outbound connection to this destination is allowed again by the egress policy"
        );
    }
}

/// Keep the addresses the policy allows. Loopback-only policies reject the
/// entire answer if any address is refused. Otherwise, refusal is counted and
/// logged only when none remain (dual-stack partial drops are debug logs).
fn filter(
    policy: &Policy,
    purpose: Purpose,
    host: &str,
    found: Vec<SocketAddr>,
) -> anyhow::Result<Vec<SocketAddr>> {
    // A fresh flag every call: `resolve`'s and `resolve_classified`'s own
    // (non-`Resolver`) callers resolve once per connection, so they keep
    // naming the host and address on every refusal exactly as before #321.
    // Only `Resolver::resolve`'s persistent, per-client flag below can ever
    // observe a transition.
    filter_tracked(policy, purpose, host, found, &AtomicBool::new(false))
}

/// Same partitioning as [`filter`], with an explicit, caller-owned
/// transition flag (#321): a caller that resolves the same destination
/// repeatedly for one purpose (telemetry/OTLP export, through
/// [`Resolver::resolve`]) passes its own persistent flag so a refusal (or
/// recovery) is logged only when that purpose's connectivity actually
/// changes, never on every attempt.
fn filter_tracked(
    policy: &Policy,
    purpose: Purpose,
    host: &str,
    found: Vec<SocketAddr>,
    transition: &AtomicBool,
) -> anyhow::Result<Vec<SocketAddr>> {
    let (allowed, refused): (Vec<_>, Vec<_>) =
        found.iter().copied().partition(|a| policy.allows(a.ip()));
    if !allowed.is_empty() && (!policy.loopback_only || refused.is_empty()) {
        // #321 round 2 (LOW finding 1): a mixed DNS answer's dropped
        // addresses are a DEBUG diagnostic, but for a `TransitionOnly`
        // purpose (telemetry/OTLP export, the OTLP audit exporter) this is
        // the same resolve-on-every-attempt path as a full refusal, so it
        // gets the same destination-free treatment - a fixed classification
        // only, never the host or dropped address.
        for addr in &refused {
            if purpose.logging_policy() == LoggingPolicy::TransitionOnly {
                tracing::debug!(
                    purpose = purpose.as_str(),
                    reason = policy.refusal(addr.ip()),
                    "egress policy dropped one resolved address (destination omitted)"
                );
            } else {
                tracing::debug!(purpose = purpose.as_str(), host, address = %addr.ip(), reason = policy.refusal(addr.ip()), "egress policy dropped one resolved address");
            }
        }
        recovered(purpose, transition);
        return Ok(allowed);
    }
    let Some(first) = refused.first() else {
        anyhow::bail!("could not resolve {host}");
    };
    denied(purpose, host, first.ip(), policy, transition);
    anyhow::bail!(
        "{} connection to {host} refused: {} is {} (see {PRIVATE_ALLOW_ENV})",
        purpose.as_str(),
        first.ip(),
        policy.refusal(first.ip())
    )
}

/// Exercise the production post-DNS filter with deterministic answers, no DNS I/O.
#[cfg(test)]
pub(crate) fn filter_for_test(
    policy: &Policy,
    purpose: Purpose,
    host: &str,
    found: Vec<SocketAddr>,
) -> anyhow::Result<Vec<SocketAddr>> {
    filter(policy, purpose, host, found)
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

/// SMTP must not retry an operator-policy refusal as a network failure.
#[derive(Clone, Copy)]
pub(crate) enum ResolveError {
    Dns,
    Policy,
}
pub(crate) async fn resolve_classified(
    policy: &Policy,
    purpose: Purpose,
    host: &str,
    port: u16,
) -> Result<Vec<SocketAddr>, ResolveError> {
    // A fresh flag every call: SMTP (this function's other caller) keeps
    // naming the host and address on every refusal exactly as before #321.
    resolve_classified_tracked(policy, purpose, host, port, &AtomicBool::new(false)).await
}

/// [`resolve_classified`] with an explicit transition flag (#321), used only
/// by [`Resolver::resolve`]'s persistent, per-client flag.
async fn resolve_classified_tracked(
    policy: &Policy,
    purpose: Purpose,
    host: &str,
    port: u16,
    transition: &AtomicBool,
) -> Result<Vec<SocketAddr>, ResolveError> {
    let found: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| ResolveError::Dns)?
        .collect();
    if found.is_empty() {
        return Err(ResolveError::Dns);
    }
    filter_tracked(policy, purpose, host, found, transition).map_err(|_| ResolveError::Policy)
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
    // A one-time startup check of a configured IP-literal URL, not a
    // periodic attempt: a fresh flag always reports this as a transition,
    // matching the previous unconditional log for every purpose that can
    // reach this (including telemetry's own startup validation).
    denied(
        purpose,
        &ip.to_string(),
        ip,
        policy,
        &AtomicBool::new(false),
    );
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

/// Typed sentinel retained through reqwest's source chain. Policy refusals
/// must never be retried as connection failures (nor classified by text).
#[derive(Debug)]
pub(crate) struct PolicyRefusal;
impl std::fmt::Display for PolicyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("outbound address policy refused destination")
    }
}
impl std::error::Error for PolicyRefusal {}

/// A reqwest DNS resolver that applies the policy. `refused` is this
/// resolver's own persistent transition flag (#321): one `Resolver` lives
/// for the whole lifetime of the reqwest `Client` built from it, so a
/// telemetry/OTLP exporter's resolver sees every connection attempt it ever
/// makes and can tell a repeated refusal from a real connectivity change.
pub struct Resolver {
    purpose: Purpose,
    policy: Option<Arc<Policy>>,
    refused: Arc<AtomicBool>,
}

/// The resolver for every reqwest client: `.dns_resolver(egress::resolver(..))`.
/// Uses the process-wide policy as it is when the request resolves.
pub fn resolver(purpose: Purpose) -> Arc<Resolver> {
    Arc::new(Resolver {
        purpose,
        policy: None,
        refused: Arc::new(AtomicBool::new(false)),
    })
}

/// A resolver with an explicit policy (OIDC uses its own development flag).
pub fn resolver_with(purpose: Purpose, policy: Policy) -> Arc<Resolver> {
    Arc::new(Resolver {
        purpose,
        policy: Some(Arc::new(policy)),
        refused: Arc::new(AtomicBool::new(false)),
    })
}

impl Resolve for Resolver {
    fn resolve(&self, name: Name) -> Resolving {
        let (purpose, policy, refused) = (
            self.purpose,
            self.policy.clone().unwrap_or_else(policy),
            self.refused.clone(),
        );
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs = resolve_classified_tracked(&policy, purpose, &host, 0, &refused)
                .await
                .map_err(|error| match error {
                    ResolveError::Policy => {
                        Box::new(PolicyRefusal) as Box<dyn std::error::Error + Send + Sync>
                    }
                    ResolveError::Dns => Box::new(std::io::Error::other("DNS resolution failed")),
                })?;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
#[path = "egress_property_tests.rs"]
mod property_tests;

#[cfg(test)]
#[path = "egress_source_scan_tests.rs"]
mod source_scan_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn development_http_is_loopback_only_after_resolution() {
        let p = Policy::new(true, Networks::parse("test", "10.0.0.0/8").unwrap()).only_loopback();
        for addr in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
            assert!(p.allows(ip(addr)));
        }
        for addr in ["8.8.8.8", "10.0.0.1", "169.254.169.254"] {
            assert!(!p.allows(ip(addr)));
        }
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

    /// #244: the IPv6 documentation range `3fff::/20` (RFC 9637) is refused,
    /// but the rest of `3ff0::/12` (which the old `(s[0] & 0xfff0) ==
    /// 0x3ff0` mask wrongly matched too) is ordinary global unicast and is
    /// allowed. This intentionally changes egress's outcome for those
    /// addresses: they were refused before #244 and are allowed after.
    #[test]
    fn issue_244_3fff_20_documentation_boundary() {
        let strict = Policy::default();
        for refused in ["3fff::1", "3fff:0fff:ffff:ffff:ffff:ffff:ffff:ffff"] {
            assert_eq!(classify(ip(refused)), Class::Refused, "{refused}");
            assert!(!strict.allows(ip(refused)), "{refused}");
        }
        for allowed in ["3fff:1000::1", "3ff0::1", "3ffe:ffff::1", "3ff1:dead::1"] {
            assert_eq!(classify(ip(allowed)), Class::Public, "{allowed}");
            assert!(strict.allows(ip(allowed)), "{allowed}");
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
    fn loopback_only_never_grants_access_and_survives_policy_derivation() {
        let strict = Policy::default().only_loopback();
        for address in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
            assert!(!strict.allows(ip(address)), "{address}");
        }
        let narrowed = Policy::new(
            true,
            Networks::parse(PRIVATE_ALLOW_ENV, "0.0.0.0/0,::/0").unwrap(),
        )
        .only_loopback();
        for policy in [
            narrowed.clone(),
            narrowed.permit_loopback(false),
            narrowed.permit_loopback(true),
            strict.permit_loopback(true),
        ] {
            for address in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
                assert!(policy.allows(ip(address)), "{address}");
            }
            for address in [
                "8.8.8.8",
                "10.1.2.3",
                "fd12::1",
                "64:ff9b::7f00:1",
                "2002:7f00:1::1",
            ] {
                assert!(!policy.allows(ip(address)), "{address}");
            }
        }
        assert!(
            filter(&narrowed, Purpose::Telemetry, "empty.example", vec![])
                .unwrap_err()
                .to_string()
                .contains("could not resolve")
        );
    }

    #[test]
    fn telemetry_literal_and_dns_policies_cannot_bypass_metadata_or_private_approval() {
        let strict = Policy::default();
        let allowed = Policy::new(
            false,
            Networks::parse(PRIVATE_ALLOW_ENV, "10.20.0.0/16").unwrap(),
        );
        let url = reqwest::Url::parse("https://10.20.1.1:4318/v1/traces").unwrap();
        assert!(check_url(&strict, Purpose::Telemetry, &url).is_err());
        assert!(check_url(&allowed, Purpose::Telemetry, &url).is_ok());
        let sockets = |addresses: &[&str]| {
            addresses
                .iter()
                .map(|address| SocketAddr::new(ip(address), 4318))
                .collect()
        };
        assert_eq!(
            filter(
                &allowed,
                Purpose::Telemetry,
                "collector.invalid",
                sockets(&["10.20.1.1", "169.254.169.254", "10.21.1.1"])
            )
            .unwrap(),
            sockets(&["10.20.1.1"])
        );
        let all = Policy::new(
            true,
            Networks::parse(PRIVATE_ALLOW_ENV, "0.0.0.0/0,::/0").unwrap(),
        );
        assert!(
            filter(
                &all,
                Purpose::Telemetry,
                "collector.invalid",
                sockets(&[
                    "169.254.169.254",
                    "fd00:ec2::254",
                    "::ffff:169.254.169.254",
                    "64:ff9b::a9fe:a9fe"
                ])
            )
            .is_err()
        );
        assert!(
            filter(
                &strict,
                Purpose::Telemetry,
                "collector.invalid",
                sockets(&["127.0.0.1"])
            )
            .is_err()
        );
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

    /// #321 (GitHub issue #321, "Telemetry export: shared egress refusal
    /// logs repeat and name the collector host"): a real HTTP client -
    /// exactly the `reqwest::Client`/`Resolver` path every exporter and
    /// provider adapter uses (`egress::resolver_with` +
    /// `.dns_resolver(..)`) - pointed at `localhost`, a real hostname
    /// resolved by real DNS and refused by a strict (production-like)
    /// policy (loopback needs `GATEWAY_ENV=development`), not a synthetic
    /// address. Several repeated connection attempts against that same
    /// refused destination - what a periodic OTLP exporter does on every
    /// export interval - currently log the telemetry purpose's refusal
    /// (naming the collector host and resolved address) on every single
    /// attempt, instead of once on the transition into refusal. An
    /// identical provider-purpose client must keep naming the host and
    /// address on every attempt (unaffected).
    ///
    /// #352: captured via `crate::log_capture_test::Capture`, not a
    /// per-test thread-local subscriber - see that module's doc comment
    /// for why (this test's DNS resolution runs on a Tokio blocking-pool
    /// thread, which a thread-local capture would never see).
    #[tokio::test]
    async fn telemetry_refusal_is_transition_only_and_host_free_across_repeated_real_attempts() {
        let capture = crate::log_capture_test::Capture::start();
        let strict = Policy::default();
        let telemetry_client = reqwest::Client::builder()
            .no_proxy()
            .dns_resolver(resolver_with(Purpose::Telemetry, strict.clone()))
            .build()
            .unwrap();
        let provider_client = reqwest::Client::builder()
            .no_proxy()
            .dns_resolver(resolver_with(Purpose::Provider, strict))
            .build()
            .unwrap();

        for attempt in 0..4 {
            assert!(
                telemetry_client
                    .get("http://localhost:4318/v1/traces")
                    .send()
                    .await
                    .is_err(),
                "attempt {attempt}"
            );
            // An identical client for a different purpose: unaffected by #321.
            assert!(
                provider_client
                    .get("http://localhost:9/")
                    .send()
                    .await
                    .is_err(),
                "attempt {attempt}"
            );
        }

        let logged = capture.text();
        let telemetry_warnings: Vec<&str> = logged
            .lines()
            .filter(|line| line.contains("\"purpose\":\"telemetry\""))
            .collect();
        assert_eq!(
            telemetry_warnings.len(),
            1,
            "4 repeated telemetry refusals of the same destination must log only the one transition into refusal: {logged}"
        );
        for forbidden in ["localhost", "\"host\"", "\"address\""] {
            assert!(
                !telemetry_warnings[0].contains(forbidden),
                "telemetry refusal must never name the destination (found {forbidden:?}): {}",
                telemetry_warnings[0]
            );
        }

        let provider_warnings: Vec<&str> = logged
            .lines()
            .filter(|line| line.contains("\"purpose\":\"provider\""))
            .collect();
        assert_eq!(
            provider_warnings.len(),
            4,
            "provider refusal logging is unchanged: every attempt logs: {logged}"
        );
        for line in &provider_warnings {
            assert!(
                line.contains("localhost"),
                "provider refusal must still name the host: {line}"
            );
        }
    }

    /// #321: the recovery transition (a previously refused destination
    /// becoming reachable again) logs its own one-time, host-free warning,
    /// and further successful attempts after that stay silent. Exercised
    /// directly against the shared partition/transition logic
    /// (`filter_tracked`) with synthetic addresses: the refusal transition
    /// and the real-DNS, real-client path are already covered above by
    /// `telemetry_refusal_is_transition_only_and_host_free_across_repeated_real_attempts`;
    /// this test only needs to prove the pure recovery state machine.
    /// #352: captured via `crate::log_capture_test::Capture`, see its doc
    /// comment.
    #[test]
    fn telemetry_recovery_logs_one_host_free_transition_warning() {
        let capture = crate::log_capture_test::Capture::start();
        let refused = SocketAddr::new(ip("127.0.0.1"), 4318);
        let allowed = SocketAddr::new(ip("8.8.8.8"), 4318);
        let transition = AtomicBool::new(false);

        // Three repeated refusals of the same (synthetic) destination: one
        // transition warning only.
        for attempt in 0..3 {
            assert!(
                filter_tracked(
                    &Policy::default(),
                    Purpose::Telemetry,
                    "collector.invalid",
                    vec![refused],
                    &transition
                )
                .is_err(),
                "attempt {attempt}"
            );
        }
        // Recovery: the same destination is now reachable. The first
        // success logs the recovery transition; further successes stay
        // silent.
        for attempt in 0..3 {
            assert!(
                filter_tracked(
                    &Policy::default(),
                    Purpose::Telemetry,
                    "collector.invalid",
                    vec![allowed],
                    &transition
                )
                .is_ok(),
                "attempt {attempt}"
            );
        }

        let logged = capture.text();
        let telemetry_warnings: Vec<&str> = logged
            .lines()
            .filter(|line| line.contains("\"purpose\":\"telemetry\""))
            .collect();
        assert_eq!(
            telemetry_warnings.len(),
            2,
            "exactly one refusal transition and one recovery transition, never one per attempt: {logged}"
        );
        for line in &telemetry_warnings {
            for forbidden in ["127.0.0.1", "8.8.8.8", "collector.invalid"] {
                assert!(!line.contains(forbidden), "{forbidden} leaked: {line}");
            }
        }
    }

    /// #321 round 2 (LOW finding 1, adversarial review): a mixed
    /// allowed/refused DNS answer logs its dropped addresses at DEBUG even
    /// when the overall answer succeeds. Before this fix that DEBUG line
    /// named the telemetry/audit-OTLP collector host and the refused
    /// address on every attempt, bypassing the destination-free policy
    /// `denied` already applies to a full refusal. A provider-purpose
    /// client's identical partial drop is unaffected.
    /// #352: captured via `crate::log_capture_test::Capture`, see its doc
    /// comment.
    #[test]
    fn telemetry_partial_drop_is_destination_free_but_provider_partial_drop_still_names_it() {
        let capture = crate::log_capture_test::Capture::start();
        let refused = SocketAddr::new(ip("127.0.0.1"), 4318);
        let allowed = SocketAddr::new(ip("8.8.8.8"), 4318);
        assert!(
            filter_tracked(
                &Policy::default(),
                Purpose::Telemetry,
                "collector.invalid",
                vec![allowed, refused],
                &AtomicBool::new(false)
            )
            .is_ok()
        );
        assert!(
            filter_tracked(
                &Policy::default(),
                Purpose::Provider,
                "collector.invalid",
                vec![allowed, refused],
                &AtomicBool::new(false)
            )
            .is_ok()
        );

        let logged = capture.text();
        let telemetry_drops: Vec<&str> = logged
            .lines()
            .filter(|line| line.contains("\"purpose\":\"telemetry\""))
            .collect();
        assert_eq!(
            telemetry_drops.len(),
            1,
            "one partial-drop debug line: {logged}"
        );
        for forbidden in ["127.0.0.1", "collector.invalid", "\"host\"", "\"address\""] {
            assert!(
                !telemetry_drops[0].contains(forbidden),
                "telemetry partial-drop must not name the destination (found {forbidden:?}): {}",
                telemetry_drops[0]
            );
        }

        let provider_drops: Vec<&str> = logged
            .lines()
            .filter(|line| line.contains("\"purpose\":\"provider\""))
            .collect();
        assert_eq!(provider_drops.len(), 1, "{logged}");
        assert!(
            provider_drops[0].contains("127.0.0.1")
                && provider_drops[0].contains("collector.invalid"),
            "provider partial-drop logging is unchanged: {}",
            provider_drops[0]
        );
    }
}
