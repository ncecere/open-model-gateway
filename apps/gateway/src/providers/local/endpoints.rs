//! Out-of-band endpoint approvals: exact canonical URL and fixed destinations.
//! No DNS lookup is performed while loading approvals or executing requests.
use crate::inference::error::InferenceError;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, SocketAddr},
    time::Duration,
};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Approval {
    endpoint: String,
    addresses: Vec<IpAddr>,
}
struct Endpoint {
    client: reqwest::Client,
    base: String,
}
/// An approved local upstream in an address range #203 lists as pending
/// refusal: accepted this release, refused in the next minor release.
/// `endpoint` is the approval's canonical base URL (host and port only, no
/// credentials: `canonical` already rejects userinfo); never a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    pub endpoint: String,
    pub category: &'static str,
}
pub struct ApprovedEndpoints {
    endpoints: BTreeMap<String, Endpoint>,
    warnings: Vec<Warning>,
}
impl ApprovedEndpoints {
    pub fn from_env(environment: &str) -> anyhow::Result<Self> {
        let value = match std::env::var("GATEWAY_LOCAL_UPSTREAMS") {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => "[]".into(),
            Err(_) => anyhow::bail!("Invalid local endpoint approvals"),
        };
        Self::parse(&value, environment)
            .map_err(|_| anyhow::anyhow!("Invalid local endpoint approvals"))
    }
    pub(super) fn parse(value: &str, environment: &str) -> Result<Self, InferenceError> {
        let invalid = InferenceError::Configuration;
        if value.len() > 65536 {
            return Err(invalid);
        }
        let approvals: Vec<Approval> = serde_json::from_str(value).map_err(|_| invalid)?;
        if approvals.len() > 64 {
            return Err(invalid);
        }
        let mut endpoints = BTreeMap::new();
        let mut warnings = Vec::new();
        for approval in approvals {
            let (base, url) = canonical(&approval.endpoint)?;
            if endpoints.contains_key(&base)
                || approval.addresses.is_empty()
                || approval.addresses.len() > 16
            {
                return Err(invalid);
            }
            let mut seen = BTreeSet::new();
            let host = url.host_str().ok_or(invalid)?;
            let literal = host.trim_matches(['[', ']']).parse::<IpAddr>().ok();
            let port = url.port_or_known_default().ok_or(invalid)?;
            let mut destinations = Vec::new();
            for ip in approval.addresses {
                if !allowed(ip, environment == "development", url.scheme() == "http")
                    || literal.is_some_and(|host| host != ip)
                    || !seen.insert(ip)
                {
                    return Err(invalid);
                }
                destinations.push(SocketAddr::new(ip, port));
                // #203: this address is accepted this release but is in a
                // range the egress policy already refuses for every other
                // client; it will be refused at startup from the next minor
                // release. Named here (host only, no credentials) so an
                // operator can act before that release.
                //
                // Pending-refusal membership is independent of `Category`
                // (review finding): it must fire for exactly the issue's
                // ranges, including the exact `3fff::/20`. (Before #244,
                // `classify_v6` used a wider `3ff0::/12` mask for
                // `Category::Documentation`; #244 narrowed it to the exact
                // `/20`, so the two now agree here too, but this check stays
                // independent of `Category` on principle, see
                // `is_pending_refusal_v6`'s doc comment in `special_use.rs`.)
                let effective = effective_address(ip);
                let pending = match effective {
                    IpAddr::V4(v4) => crate::special_use::is_pending_refusal_v4(v4),
                    IpAddr::V6(v6) => crate::special_use::is_pending_refusal_v6(v6),
                };
                if pending {
                    warnings.push(Warning {
                        endpoint: base.clone(),
                        category: category_of(ip).label(),
                    });
                }
            }
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .connect_timeout(Duration::from_secs(10))
                .resolve_to_addrs(host, &destinations)
                .build()
                .map_err(|_| invalid)?;
            endpoints.insert(base.clone(), Endpoint { client, base });
        }
        Ok(Self {
            endpoints,
            warnings,
        })
    }
    /// Approved local upstreams in a range #203 lists as pending refusal: a
    /// startup warning should name each one (never a secret) and readiness
    /// should flag it (see `log_pending_refusal_warnings`).
    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
    /// Management-time validation. Does not read credentials or contact upstreams.
    /// The caller must also check EnvSecrets::allows for any environment reference.
    pub fn validate_connection(
        &self,
        endpoint: &str,
        credential_ref: &str,
        region: Option<&str>,
    ) -> Result<(), InferenceError> {
        if region.is_some_and(|r| !r.is_empty())
            || (credential_ref != "none"
                && !credential_ref.strip_prefix("env:").is_some_and(|name| {
                    !name.is_empty()
                        && name.len() <= 200
                        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                }))
        {
            return Err(InferenceError::Configuration);
        }
        self.approved(endpoint)?;
        Ok(())
    }
    pub(super) fn approved(
        &self,
        endpoint: &str,
    ) -> Result<(&reqwest::Client, &str), InferenceError> {
        let (base, _) = canonical(endpoint)?;
        let entry = self
            .endpoints
            .get(&base)
            .ok_or(InferenceError::Configuration)?;
        Ok((&entry.client, &entry.base))
    }
    /// A server load signal (Prometheus text, batch scheduling): only
    /// `{approved base without the final /v1}/metrics` of an approved local
    /// endpoint, fetched with that approval's pinned client (no redirects,
    /// proxy or retries; no credentials are ever sent). Returns the client and
    /// the exact URL.
    pub fn metrics_endpoint(
        &self,
        url: &str,
    ) -> Result<(&reqwest::Client, String), InferenceError> {
        let invalid = InferenceError::Configuration;
        if url.len() > 2048 {
            return Err(invalid);
        }
        let parsed = reqwest::Url::parse(url).map_err(|_| invalid)?;
        let prefix = parsed.path().strip_suffix("/metrics").ok_or(invalid)?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || parsed.path().contains('%')
            || url != parsed.as_str()
        {
            return Err(invalid);
        }
        let mut base = parsed.clone();
        base.set_path(&format!("{prefix}/v1"));
        let (base, _) = canonical(base.as_str())?;
        let entry = self.endpoints.get(&base).ok_or(invalid)?;
        Ok((&entry.client, parsed.as_str().to_owned()))
    }
    #[cfg(test)]
    pub(crate) fn for_test(endpoint: &str) -> Self {
        Self::parse(
            &serde_json::json!([{"endpoint":endpoint,"addresses":["127.0.0.1"]}]).to_string(),
            "development",
        )
        .unwrap()
    }
}
fn canonical(endpoint: &str) -> Result<(String, reqwest::Url), InferenceError> {
    let invalid = InferenceError::Configuration;
    if endpoint.len() > 2048 {
        return Err(invalid);
    }
    let url = reqwest::Url::parse(endpoint).map_err(|_| invalid)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().trim_end_matches('/').ends_with("/v1")
        || url.path().contains('%')
        || endpoint.trim_end_matches('/') != url.as_str().trim_end_matches('/')
    {
        return Err(invalid);
    }
    Ok((url.as_str().trim_end_matches('/').to_owned(), url))
}
/// This file's own mapping from the shared special-use category (#203) to
/// an accept/refuse decision for an IPv4 address: carrier-grade NAT
/// (`Category::Cgnat`, 100.64.0.0/10) is refused unconditionally, as it
/// always was here (never even the exception `is_private()` would have
/// given it, since Rust's `is_private()` does not cover that range).
fn allowed_v4(category: crate::special_use::Category, development: bool, http: bool) -> bool {
    use crate::special_use::Category;
    match category {
        Category::Loopback => development,
        Category::Private => true,
        Category::Public
        | Category::Benchmarking
        | Category::Documentation
        | Category::IetfProtocolAssignments
        | Category::Deprecated6to4Relay => !http,
        Category::LinkLocal
        | Category::Multicast
        | Category::Broadcast
        | Category::Reserved
        | Category::Metadata
        | Category::Cgnat
        | Category::AwsMetadataRange => false,
    }
}
/// This file's own mapping for an IPv6 address that is not loopback and does
/// not carry a mapped/compatible IPv4 address (those recurse into
/// `allowed_v4` before this is reached). Unlike `egress.rs`, only the exact
/// AWS metadata address is refused; the rest of `fd00:ec2::/32` is accepted
/// like unique-local, pending refusal in the next minor release (#203).
fn allowed_v6(category: crate::special_use::Category, development: bool, http: bool) -> bool {
    use crate::special_use::Category;
    match category {
        Category::Loopback => development,
        Category::Private | Category::AwsMetadataRange => true,
        Category::Public | Category::Benchmarking | Category::Documentation => !http,
        Category::LinkLocal | Category::Multicast | Category::Reserved | Category::Metadata => {
            false
        }
        // Never produced by `special_use::classify_v6` (IPv4-only categories).
        Category::Broadcast | Category::Cgnat | Category::IetfProtocolAssignments => false,
        Category::Deprecated6to4Relay => false,
    }
}
/// The IPv4 address `allowed` ultimately judges a configured approval's
/// literal as (mapped, compatible, 6to4 or Teredo), or the literal address
/// itself when it carries none. Loopback is never translated, matching
/// `allowed`'s own order (it checks loopback before any translation).
/// Exists only for #203's warning (`category_of`, `is_pending_refusal_v4`/
/// `is_pending_refusal_v6` below): it never changes any accept/refuse
/// decision, which `allowed` alone makes.
fn effective_address(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) if v6.is_loopback() => ip,
        IpAddr::V6(v6) => v6
            .to_ipv4()
            .or_else(|| crate::egress::translated_v4(v6))
            .map_or(ip, IpAddr::V4),
    }
}
/// The category #203's warning names for a configured approval address: see
/// [`effective_address`]. Fixed (#203 review finding): this used to unwrap
/// only mapped/compatible IPv4 forms, so a 6to4 or Teredo literal embedding
/// a pending-refusal IPv4 address (for example `2002:c612:1::1`, 6to4 of
/// `198.18.0.1`) was accepted by `allowed` (#196) but never warned about.
fn category_of(ip: IpAddr) -> crate::special_use::Category {
    match effective_address(ip) {
        IpAddr::V4(v4) => crate::special_use::classify_v4(v4),
        IpAddr::V6(v6) => crate::special_use::classify_v6(v6),
    }
}
fn allowed(ip: IpAddr, development: bool, http: bool) -> bool {
    match ip {
        IpAddr::V4(ip) => allowed_v4(crate::special_use::classify_v4(ip), development, http),
        IpAddr::V6(ip) => {
            if ip.is_loopback() {
                return development;
            }
            // Include deprecated IPv4-compatible encodings, not only mapped ones.
            if let Some(v4) = ip.to_ipv4() {
                return allowed_v4(crate::special_use::classify_v4(v4), development, http);
            }
            // Deny translation/special-use ranges that can obscure a forbidden
            // IPv4 destination (including NAT64); ordinary global v6 is 2000::/3.
            let global = !http && (ip.segments()[0] & 0xe000) == 0x2000;
            // 6to4 (2002::/16) and Teredo (2001::/32) are inside 2000::/3 but
            // carry an IPv4 destination: judge it as a plain IPv4 address, so
            // metadata, link-local and loopback (outside development) cannot
            // hide behind them (#196). NAT64 is not in 2000::/3 and stays refused.
            if global
                && let Some(v4) = crate::egress::translated_v4(ip)
                && !allowed_v4(crate::special_use::classify_v4(v4), development, http)
            {
                return false;
            }
            allowed_v6(crate::special_use::classify_v6(ip), development, http)
        }
    }
}
static PENDING_REFUSAL_WARNING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// Startup only (`main.rs`, once per process, after loading the approvals a
/// replica will actually use): log each #203 pending-refusal warning (host
/// and category, never a secret) and record that readiness should flag it.
/// Never called by tests, which read `ApprovedEndpoints::warnings()` or
/// `allowed()` directly instead of this process-wide flag.
pub fn log_pending_refusal_warnings(approvals: &ApprovedEndpoints) {
    for warning in approvals.warnings() {
        tracing::warn!(
            endpoint = %warning.endpoint,
            category = warning.category,
            "approved local upstream is in an address range the egress policy already refuses for every other client; the next minor release will refuse it at startup too (see docs/releases/unreleased.md)"
        );
    }
    if !approvals.warnings().is_empty() {
        PENDING_REFUSAL_WARNING.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
/// Readiness detail only (`/health/ready`, unauthenticated: no more detail
/// than it already exposes, never an endpoint name): whether any approved
/// local upstream is in a range a future release will refuse. Never changes
/// the ready/not-ready decision.
pub fn has_pending_refusal_warning() -> bool {
    PENDING_REFUSAL_WARNING.load(std::sync::atomic::Ordering::Relaxed)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_exact_approval_and_dangerous_destinations() {
        for endpoint in [
            "http://user:secret@host/v1",
            "http://host/v1?x=1",
            "http://host/v1#x",
            "file:///v1",
            "http://host/../v1",
            "http://HOST/v1",
            "http://host/v1%2f",
        ] {
            assert!(canonical(endpoint).is_err(), "{endpoint}");
        }
        for address in [
            "169.254.169.254",
            "169.254.1.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "fe80::1",
            "ff02::1",
            "::ffff:169.254.169.254",
            "::169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "100.100.100.200",
            "fd00:ec2::254",
        ] {
            assert!(!allowed(address.parse().unwrap(), true, false), "{address}");
        }
        assert!(!allowed("127.0.0.1".parse().unwrap(), false, true));
        assert!(allowed("127.0.0.1".parse().unwrap(), true, true));
        let p = ApprovedEndpoints::parse(
            r#"[{"endpoint":"http://local.example:8000/v1","addresses":["10.10.1.20"]}]"#,
            "production",
        )
        .unwrap();
        assert!(p.approved("http://local.example:8000/v1/").is_ok());
        assert!(p.approved("http://other.example:8000/v1").is_err());
        assert!(
            ApprovedEndpoints::parse(
                r#"[{"endpoint":"http://127.0.0.1:8000/v1","addresses":["10.10.1.20"]}]"#,
                "development"
            )
            .is_err()
        );
    }
    /// The approval check as it was before #196 and #203, frozen here as a
    /// self-contained independent oracle (#203 review finding): this used
    /// to call *production* `allowed` for the translated-IPv4 case, so it
    /// could not catch a drift between the two. Copied from `git show
    /// a99bb00:apps/gateway/src/providers/local/endpoints.rs` (the only
    /// difference from the pre-#196 function proper is that the recursive
    /// calls below go to `before` itself, exactly as the original function
    /// recursed on itself before it was renamed): its own independent
    /// 6to4/Teredo/NAT64 decoding (`oracle_translated_v4`), never a call
    /// into `egress.rs` or into this file's current `allowed`/`category_of`.
    fn before(ip: IpAddr, development: bool, http: bool) -> bool {
        match ip {
            IpAddr::V4(ip) => {
                let o = ip.octets();
                if ip.is_unspecified()
                    || ip.is_link_local()
                    || ip.is_multicast()
                    || ip.is_broadcast()
                    || o[0] == 0
                    || o[0] >= 240
                    || (o[0] == 100 && (64..=127).contains(&o[1]))
                {
                    return false;
                }
                if ip.is_loopback() {
                    return development;
                }
                !http || ip.is_private()
            }
            IpAddr::V6(ip) => {
                if ip.is_loopback() {
                    return development;
                }
                if let Some(v4) = ip.to_ipv4() {
                    return before(IpAddr::V4(v4), development, http);
                }
                if ip.is_unspecified()
                    || ip.is_unicast_link_local()
                    || ip.is_multicast()
                    || ip == "fd00:ec2::254".parse::<std::net::Ipv6Addr>().unwrap()
                {
                    return false;
                }
                let global = !http && (ip.segments()[0] & 0xe000) == 0x2000;
                // 6to4 (2002::/16) and Teredo (2001::/32) are inside 2000::/3 but
                // carry an IPv4 destination: judge it as a plain IPv4 address, so
                // metadata, link-local and loopback (outside development) cannot
                // hide behind them (#196). NAT64 is not in 2000::/3 and stays refused.
                if global
                    && let Some(v4) = oracle_translated_v4(ip)
                    && !before(IpAddr::V4(v4), development, http)
                {
                    return false;
                }
                ip.is_unique_local() || global
            }
        }
    }
    /// The IPv4 destination an IPv6 address embeds (NAT64, 6to4 or Teredo),
    /// decoded independently of `egress::translated_v4` for [`before`]'s
    /// sake: this oracle must not call into any current production helper.
    fn oracle_translated_v4(ip: std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
        let s = ip.segments();
        let low = (u32::from(s[6]) << 16) | u32::from(s[7]);
        if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
            return Some(std::net::Ipv4Addr::from(low));
        }
        if s[0] == 0x2002 {
            return Some(std::net::Ipv4Addr::from(
                (u32::from(s[1]) << 16) | u32::from(s[2]),
            ));
        }
        (s[0] == 0x2001 && s[1] == 0).then(|| std::net::Ipv4Addr::from(!low))
    }

    /// 6to4 and Teredo carry an IPv4 destination; over HTTPS they are judged by
    /// it (#196): metadata and loopback (outside development) are refused,
    /// private and public addresses are accepted as they are when written
    /// plainly. Nothing else changes.
    #[test]
    fn six_to_four_and_teredo_are_judged_by_their_embedded_address() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        // Metadata, link-local and other refused IPv4 destinations.
        for address in [
            "2002:a9fe:a9fe::1",                    // 6to4 of 169.254.169.254
            "2001:0:4136:e378:8000:63bf:5601:5601", // Teredo of 169.254.169.254
            "2002:6464:64c8::1",                    // 6to4 of 100.100.100.200 (CGNAT)
            "2002:0000:0001::1",                    // 6to4 of 0.0.0.1
            "2002:e000:1::1",                       // 6to4 of 224.0.0.1
        ] {
            for development in [false, true] {
                assert!(!allowed(ip(address), development, false), "{address}");
            }
        }
        // Loopback only in development, as a plain 127.0.0.1.
        assert!(!allowed(ip("2002:7f00:1::1"), false, false));
        assert!(allowed(ip("2002:7f00:1::1"), true, false));
        // Private and public destinations over HTTPS, like their plain forms.
        assert!(allowed(ip("2002:0a00:1::1"), false, false), "private");
        assert!(
            allowed(ip("2002:0808:0808::1"), false, false),
            "public 6to4"
        );
        assert!(
            allowed(ip("2001:0:4136:e378:8000:63bf:f7f7:f7f7"), false, false),
            "public Teredo"
        );
        // Plain HTTP to these forms stays refused, as before.
        for address in ["2002:0808:0808::1", "2002:0a00:1::1", "2002:7f00:1::1"] {
            assert!(!allowed(ip(address), true, true), "{address}");
        }
        // NAT64 stays refused, public destination or not.
        for address in ["64:ff9b::808:808", "64:ff9b::a9fe:a9fe", "64:ff9b::a00:1"] {
            for (development, http) in [(false, false), (true, false), (true, true)] {
                assert!(!allowed(ip(address), development, http), "{address}");
            }
        }
    }

    /// Existing installations may have approved these over HTTPS; refusing
    /// them would stop startup, so they stay accepted until a migration path
    /// exists (follow-up issue).
    #[test]
    fn special_use_ranges_approved_before_stay_accepted() {
        for address in [
            "198.18.0.1",
            "192.0.0.1",
            "192.88.99.1",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "2001:db8::1",
            "fd00:ec2::1",
        ] {
            assert!(allowed(address.parse().unwrap(), false, false), "{address}");
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(4096))]
        /// Against the behaviour before #196: nothing is newly allowed, and
        /// the only newly refused addresses are 6to4 and Teredo addresses
        /// whose embedded IPv4 the plain rules refuse.
        #[test]
        fn approvals_differ_from_before_only_by_the_embedded_address(
            bits in proptest::prelude::any::<u32>(),
            filler in proptest::prelude::any::<[u16; 8]>(),
            form in 0usize..6,
            development in proptest::prelude::any::<bool>(),
            http in proptest::prelude::any::<bool>(),
        ) {
            let (hi, lo) = ((bits >> 16) as u16, bits as u16);
            let (nhi, nlo) = ((!bits >> 16) as u16, !bits as u16);
            let f = filler;
            let (ip, embedded): (IpAddr, Option<std::net::Ipv4Addr>) = match form {
                0 => (std::net::Ipv4Addr::from(bits).into(), None),
                1 => (std::net::Ipv6Addr::from([0, 0, 0, 0, 0, 0xffff, hi, lo]).into(), None),
                2 => (std::net::Ipv6Addr::from([0x64, 0xff9b, 0, 0, 0, 0, hi, lo]).into(), None),
                3 => (
                    std::net::Ipv6Addr::from([0x2002, hi, lo, f[3], f[4], f[5], f[6], f[7]]).into(),
                    Some(std::net::Ipv4Addr::from(bits)),
                ),
                4 => (
                    std::net::Ipv6Addr::from([0x2001, 0, f[2], f[3], f[4], f[5], nhi, nlo]).into(),
                    Some(std::net::Ipv4Addr::from(bits)),
                ),
                _ => (std::net::Ipv6Addr::from(f).into(), None),
            };
            let (now, was) = (allowed(ip, development, http), before(ip, development, http));
            proptest::prop_assert!(!now || was, "newly allowed: {}", ip);
            if was && !now {
                let inner = embedded.expect("only translated forms may be newly refused");
                proptest::prop_assert!(!http && !before(IpAddr::V4(inner), development, false), "{}", ip);
            }
            // Nothing else moved for a form with no embedded address to judge.
            if embedded.is_none() {
                proptest::prop_assert_eq!(now, was, "{}", ip);
            }
        }
    }

    /// First and last address of every IPv4 range in #203's special-use
    /// table (`special_use.rs`), as the `u32` bit pattern: biases the
    /// proptest below toward exactly these boundaries instead of only
    /// sampling them by chance.
    fn v4_range_boundaries() -> Vec<u32> {
        let a = |o0: u8, o1: u8, o2: u8, o3: u8| u32::from(std::net::Ipv4Addr::new(o0, o1, o2, o3));
        vec![
            a(0, 0, 0, 0),
            a(0, 255, 255, 255), // "this network" (Reserved)
            a(10, 0, 0, 0),
            a(10, 255, 255, 255), // RFC 1918 (Private)
            a(100, 64, 0, 0),
            a(100, 127, 255, 255), // CGNAT (Cgnat)
            a(100, 100, 100, 200), // Alibaba metadata (Metadata)
            a(127, 0, 0, 0),
            a(127, 255, 255, 255), // loopback (Loopback)
            a(169, 254, 0, 0),
            a(169, 254, 255, 255), // link-local, cloud metadata (LinkLocal)
            a(172, 16, 0, 0),
            a(172, 31, 255, 255), // RFC 1918 (Private)
            a(192, 0, 0, 0),
            a(192, 0, 0, 255), // IETF protocol assignments
            a(192, 0, 2, 0),
            a(192, 0, 2, 255), // documentation
            a(192, 88, 99, 0),
            a(192, 88, 99, 255), // deprecated 6to4 relay
            a(192, 168, 0, 0),
            a(192, 168, 255, 255), // RFC 1918 (Private)
            a(198, 18, 0, 0),
            a(198, 19, 255, 255), // benchmarking
            a(198, 51, 100, 0),
            a(198, 51, 100, 255), // documentation
            a(203, 0, 113, 0),
            a(203, 0, 113, 255), // documentation
            a(224, 0, 0, 0),
            a(239, 255, 255, 255), // multicast (Multicast)
            a(240, 0, 0, 0),
            a(255, 255, 255, 254), // reserved
            a(255, 255, 255, 255), // broadcast
        ]
    }
    /// First and last address (segments) of every IPv6-only range in
    /// #203's special-use table.
    fn v6_range_boundaries() -> Vec<[u16; 8]> {
        vec![
            [0, 0, 0, 0, 0, 0, 0, 0],      // unspecified (Reserved)
            [0, 0, 0, 0, 0, 0, 0, 1],      // ::1 (Loopback)
            [0xfe80, 0, 0, 0, 0, 0, 0, 0], // link-local start
            [
                0xfebf, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff,
            ], // link-local end
            [0xfec0, 0, 0, 0, 0, 0, 0, 0], // deprecated site-local start (Reserved)
            [
                0xfeff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff,
            ], // site-local end
            [0xfc00, 0, 0, 0, 0, 0, 0, 0], // unique-local start (Private)
            [
                0xfdff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff,
            ], // unique-local end
            [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0], // AWS metadata range start
            [
                0xfd00, 0x0ec2, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff,
            ], // AWS metadata range end
            [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254], // exact AWS metadata address
            [0xff00, 0, 0, 0, 0, 0, 0, 0], // multicast start
            [
                0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff,
            ], // multicast end
            [0x2001, 0xdb8, 0, 0, 0, 0, 0, 0], // documentation start
            [
                0x2001, 0xdb8, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff,
            ], // documentation end
            [0x3fff, 0, 0, 0, 0, 0, 0, 0], // 3fff::/20 start (documentation)
            [
                0x3fff, 0x0fff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff,
            ], // 3fff::/20 end
            [0x3ff0, 0, 0, 0, 0, 0, 0, 0], // just outside the issue's /20 (legacy 3ff0::/12 start)
            [
                0x3ffe, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff,
            ], // just below 3fff::
            [0x2001, 2, 0, 0, 0, 0, 0, 0], // benchmarking start
            [0x2001, 2, 0, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff], // benchmarking end
            [0x2000, 0, 0, 0, 0, 0, 0, 0], // public (2000::/3) start
            [
                0x3fff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff,
            ], // 2000::/3 end
        ]
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(8192))]
        /// #203 review finding: the frozen, self-contained a99bb00 oracle
        /// (`before`) must decide *exactly* as the current `allowed` for
        /// every encoding and setting, not merely "no newly allowed address,
        /// newly refused only by its embedded address": #196 and #203 changed
        /// how classification is computed, never any accept/refuse outcome.
        /// Covers both schemes (`http`), both `development` settings, every
        /// encoding form (plain v4, mapped, compatible, 6to4, Teredo, NAT64,
        /// random v6), and biased sampling at the first/last address of every
        /// range in `special_use.rs`'s table.
        #[test]
        fn allowed_matches_the_frozen_a99bb00_oracle_exactly(
            random_bits in proptest::prelude::any::<u32>(),
            v4_boundary_pick in 0usize..v4_range_boundaries().len(),
            use_v4_boundary in proptest::prelude::any::<bool>(),
            random_segments in proptest::prelude::any::<[u16; 8]>(),
            v6_boundary_pick in 0usize..v6_range_boundaries().len(),
            use_v6_boundary in proptest::prelude::any::<bool>(),
            filler in proptest::prelude::any::<[u16; 8]>(),
            form in 0usize..7,
            development in proptest::prelude::any::<bool>(),
            http in proptest::prelude::any::<bool>(),
        ) {
            let v4_boundaries = v4_range_boundaries();
            let v6_boundaries = v6_range_boundaries();
            let bits = if use_v4_boundary { v4_boundaries[v4_boundary_pick] } else { random_bits };
            let (hi, lo) = ((bits >> 16) as u16, bits as u16);
            let (nhi, nlo) = ((!bits >> 16) as u16, !bits as u16);
            let f = filler;
            let ip: IpAddr = match form {
                0 => std::net::Ipv4Addr::from(bits).into(),
                1 => std::net::Ipv6Addr::from([0, 0, 0, 0, 0, 0xffff, hi, lo]).into(), // mapped
                2 => std::net::Ipv6Addr::from([0, 0, 0, 0, 0, 0, hi, lo]).into(), // deprecated compatible
                3 => std::net::Ipv6Addr::from([0x2002, hi, lo, f[3], f[4], f[5], f[6], f[7]]).into(), // 6to4
                4 => std::net::Ipv6Addr::from([0x2001, 0, f[2], f[3], f[4], f[5], nhi, nlo]).into(), // Teredo
                5 => std::net::Ipv6Addr::from([0x64, 0xff9b, 0, 0, 0, 0, hi, lo]).into(), // NAT64
                _ => {
                    let s = if use_v6_boundary { v6_boundaries[v6_boundary_pick] } else { random_segments };
                    std::net::Ipv6Addr::from(s).into()
                }
            };
            proptest::prop_assert_eq!(
                allowed(ip, development, http),
                before(ip, development, http),
                "{} development={} http={}", ip, development, http
            );
        }
    }

    #[test]
    fn metrics_urls_must_be_an_approved_origin() {
        let p = ApprovedEndpoints::parse(
            r#"[{"endpoint":"http://gpu.example:8000/v1","addresses":["10.10.1.20"]},{"endpoint":"https://proxy.example/vllm/v1","addresses":["10.10.1.21"]}]"#,
            "production",
        )
        .unwrap();
        assert_eq!(
            p.metrics_endpoint("http://gpu.example:8000/metrics")
                .unwrap()
                .1,
            "http://gpu.example:8000/metrics"
        );
        assert!(
            p.metrics_endpoint("https://proxy.example/vllm/metrics")
                .is_ok()
        );
        for url in [
            "http://gpu.example:9000/metrics",
            "http://other.example:8000/metrics",
            "http://gpu.example:8000/v1/metrics",
            "http://gpu.example:8000/metrics?x=1",
            "http://gpu.example:8000/metrics#x",
            "http://u:p@gpu.example:8000/metrics",
            "http://gpu.example:8000/stats",
            "https://gpu.example:8000/metrics",
            "https://proxy.example/metrics",
            "http://GPU.example:8000/metrics",
        ] {
            assert!(p.metrics_endpoint(url).is_err(), "{url}");
        }
    }
}

/// #203, release N: a test per listed range proving it is still accepted
/// locally, still produces a startup warning naming it, and is still
/// refused by the egress policy for every other client; plus a control
/// proving ordinary public/RFC 1918 approvals produce no warning.
#[cfg(test)]
mod pending_refusal_tests {
    use super::*;

    fn ranges() -> Vec<(&'static str, &'static str)> {
        vec![
            ("198.18.0.1", "benchmarking"),
            ("192.0.0.1", "IETF protocol assignments"),
            ("192.88.99.1", "deprecated 6to4 relay"),
            ("192.0.2.1", "documentation"),
            ("198.51.100.1", "documentation"),
            ("203.0.113.1", "documentation"),
            ("2001:db8::1", "documentation"),
            ("3fff::1", "documentation"),
            ("2001:2::1", "benchmarking"),
            ("fd00:ec2::1", "AWS metadata range (fd00:ec2::/32)"),
        ]
    }

    #[test]
    fn each_pending_refusal_range_is_accepted_warned_and_egress_refused() {
        for (address, category) in ranges() {
            let scheme = if address.contains(':') {
                format!("https://[{address}]/v1")
            } else {
                format!("https://{address}/v1")
            };
            let ip: IpAddr = address.parse().unwrap();

            // (a) local approval still accepts it this release.
            assert!(allowed(ip, false, false), "{address}");
            let approvals = ApprovedEndpoints::parse(
                &serde_json::json!([{"endpoint": scheme, "addresses": [address]}]).to_string(),
                "production",
            )
            .unwrap_or_else(|_| panic!("{address} should still be approved this release"));

            // (b) a startup warning names the approval and its category.
            let base = scheme.trim_end_matches('/').to_owned();
            assert!(
                approvals
                    .warnings()
                    .iter()
                    .any(|w| w.endpoint == base && w.category == category),
                "{address}: warnings were {:?}",
                approvals.warnings()
            );

            // (c) egress still refuses it, even for the most permissive policy.
            let everything = crate::egress::Policy::new(
                true,
                crate::egress::Networks::parse(crate::egress::PRIVATE_ALLOW_ENV, "0.0.0.0/0,::/0")
                    .unwrap(),
            );
            assert!(!everything.allows(ip), "{address}");
        }
    }

    /// NAT64 approvals stay refused outright: no warning, never accepted.
    #[test]
    fn nat64_approvals_stay_refused_with_no_warning() {
        for address in ["64:ff9b::808:808", "64:ff9b::a9fe:a9fe"] {
            let ip: IpAddr = address.parse().unwrap();
            assert!(!allowed(ip, true, false), "{address}");
            assert!(
                ApprovedEndpoints::parse(
                    &serde_json::json!([{"endpoint": format!("https://[{address}]/v1"), "addresses": [address]}])
                        .to_string(),
                    "production",
                )
                .is_err(),
                "{address}"
            );
        }
    }

    /// Control: an installation with only public and RFC 1918 approvals
    /// produces no pending-refusal warning. `warnings()` is the source of
    /// truth tests use (not the process-wide readiness flag, which other
    /// tests in this binary may already have set: it is monotonic and
    /// shared, so only ever asserted true, never false, below).
    #[test]
    fn only_public_or_rfc1918_approvals_produce_no_warning() {
        let approvals = ApprovedEndpoints::parse(
            r#"[
                {"endpoint":"https://public.example/v1","addresses":["8.8.8.8"]},
                {"endpoint":"http://private.example/v1","addresses":["10.1.2.3"]},
                {"endpoint":"https://private6.example/v1","addresses":["fd12::1"]}
            ]"#,
            "production",
        )
        .unwrap();
        assert!(
            approvals.warnings().is_empty(),
            "{:?}",
            approvals.warnings()
        );
    }

    /// A control in the other direction: an installation with one pending
    /// range mixed into otherwise-ordinary approvals is still flagged.
    /// `log_pending_refusal_warnings`/`has_pending_refusal_warning` are not
    /// exercised here: the flag they share is process-wide (one process runs
    /// every test in this binary), so a unit test that set it would make
    /// every other test observing it order-dependent. They are proved at
    /// the real startup path instead.
    #[test]
    fn a_single_pending_range_among_ordinary_approvals_is_flagged() {
        let approvals = ApprovedEndpoints::parse(
            r#"[
                {"endpoint":"https://public.example/v1","addresses":["8.8.8.8"]},
                {"endpoint":"https://benchmarking.example/v1","addresses":["198.18.0.1"]}
            ]"#,
            "production",
        )
        .unwrap();
        assert_eq!(approvals.warnings().len(), 1);
        assert_eq!(approvals.warnings()[0].category, "benchmarking");
    }

    /// A 6to4 or Teredo literal for each pending-refusal IPv4 range, built
    /// from the embedded address rather than a hand-computed literal.
    fn six_to_four_and_teredo(embedded: &str) -> [IpAddr; 2] {
        let v4: std::net::Ipv4Addr = embedded.parse().unwrap();
        let o = v4.octets();
        let six_to_four = std::net::Ipv6Addr::new(
            0x2002,
            u16::from_be_bytes([o[0], o[1]]),
            u16::from_be_bytes([o[2], o[3]]),
            0,
            0,
            0,
            0,
            1,
        );
        let inverted = !u32::from(v4);
        let teredo = std::net::Ipv6Addr::new(
            0x2001,
            0,
            0,
            0,
            0x8000,
            0x63bf,
            (inverted >> 16) as u16,
            inverted as u16,
        );
        [IpAddr::V6(six_to_four), IpAddr::V6(teredo)]
    }

    /// #203 review finding: `category_of` used to unwrap only
    /// mapped/compatible IPv4 forms, so a 6to4 or Teredo literal embedding a
    /// pending-refusal address (`allowed` already judges it by that embedded
    /// address, #196) produced no warning at all. It must warn exactly like
    /// the plain IPv4 form does.
    #[test]
    fn six_to_four_and_teredo_embedding_a_pending_range_are_warned() {
        for (embedded, category) in [
            ("198.18.0.1", "benchmarking"),
            ("192.0.0.1", "IETF protocol assignments"),
            ("192.88.99.1", "deprecated 6to4 relay"),
            ("192.0.2.1", "documentation"),
            ("198.51.100.1", "documentation"),
            ("203.0.113.1", "documentation"),
            ("2001:2::1", "benchmarking"),
            ("2001:db8::1", "documentation"),
        ] {
            // The last two ranges are IPv6-only (no embeddable IPv4 form);
            // every other range has a 6to4/Teredo literal to check too.
            let forms: Vec<IpAddr> = if embedded.contains(':') {
                vec![embedded.parse().unwrap()]
            } else {
                six_to_four_and_teredo(embedded).to_vec()
            };
            for ip in forms {
                assert!(
                    allowed(ip, false, false),
                    "{ip}: should still be accepted (#196)"
                );
                let scheme = format!("https://[{ip}]/v1");
                let approvals = ApprovedEndpoints::parse(
                    &serde_json::json!([{"endpoint": scheme, "addresses": [ip.to_string()]}])
                        .to_string(),
                    "production",
                )
                .unwrap_or_else(|_| panic!("{ip} should still be approved this release"));
                assert!(
                    approvals.warnings().iter().any(|w| w.category == category),
                    "{ip}: expected a {category} warning, got {:?}",
                    approvals.warnings()
                );
            }
        }
    }

    /// Control: 6to4 and Teredo embedding an ordinary public or RFC 1918
    /// address produce no warning (the embedded address's category, not the
    /// literal's own apparent `Category::Public`, decides this).
    #[test]
    fn six_to_four_and_teredo_embedding_public_or_private_are_not_warned() {
        for embedded in ["8.8.8.8", "10.1.2.3"] {
            for ip in six_to_four_and_teredo(embedded) {
                assert!(allowed(ip, false, false), "{ip}");
                let scheme = format!("https://[{ip}]/v1");
                let approvals = ApprovedEndpoints::parse(
                    &serde_json::json!([{"endpoint": scheme, "addresses": [ip.to_string()]}])
                        .to_string(),
                    "production",
                )
                .unwrap();
                assert!(
                    approvals.warnings().is_empty(),
                    "{ip}: {:?}",
                    approvals.warnings()
                );
            }
        }
    }

    /// #203 review finding, updated for #244: the warning for `3fff::/20`
    /// must not also fire for `3ff0::/12` addresses outside it.
    /// `providers/local/endpoints.rs`'s own accept/refuse decision for these
    /// addresses (`allowed`, via `allowed_v6`) is unchanged by #244: before
    /// #244 they were `Category::Documentation`, after they are
    /// `Category::Public`, and `allowed_v6` maps both to `!http`, so the
    /// local-upstream approval is still accepted over HTTPS either way.
    /// `egress.rs`'s decision for them is *not* unchanged: #244 fixes the
    /// over-wide `3ff0::/12` documentation mask there, so these addresses
    /// move from `Category::Documentation` (refused) to `Category::Public`
    /// (allowed) for every other client. That egress outcome change is the
    /// point of #244; this test only proves it does not also change
    /// `endpoints.rs`'s own decision or the exact-`/20` warning.
    #[test]
    fn warning_fires_for_3fff_20_only_not_the_wider_3ff0_12() {
        let warned = ApprovedEndpoints::parse(
            &serde_json::json!([{"endpoint": "https://[3fff::1]/v1", "addresses": ["3fff::1"]}])
                .to_string(),
            "production",
        )
        .unwrap();
        assert_eq!(warned.warnings().len(), 1);
        assert_eq!(warned.warnings()[0].category, "documentation");

        for address in ["3ff0::1", "3ffe:ffff::1"] {
            let ip: IpAddr = address.parse().unwrap();
            // endpoints.rs's own decision is unchanged: still accepted over
            // HTTPS (`allowed_v6` maps both `Public` and `Documentation` to
            // `!http`).
            assert!(allowed(ip, false, false), "{address}");
            // egress's decision *does* change (#244, intentional): these
            // addresses are `Category::Public` now, not `Documentation`, so
            // the strict policy allows them.
            let strict = crate::egress::Policy::default();
            assert!(strict.allows(ip), "{address}");
            // No warning either before or after #244: these addresses were
            // never in `is_pending_refusal_v6`'s exact ranges.
            let not_warned = ApprovedEndpoints::parse(
                &serde_json::json!([{"endpoint": format!("https://[{address}]/v1"), "addresses": [address]}])
                    .to_string(),
                "production",
            )
            .unwrap();
            assert!(
                not_warned.warnings().is_empty(),
                "{address}: {:?}",
                not_warned.warnings()
            );
        }
    }
}
