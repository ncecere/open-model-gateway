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
                let category = category_of(ip);
                if category.is_pending_refusal() {
                    warnings.push(Warning {
                        endpoint: base.clone(),
                        category: category.label(),
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
/// The category #203's warning logic reports for a configured approval
/// address: the same dispatch `allowed` uses for mapped/compatible IPv4
/// forms, but it does not re-check a 6to4 or Teredo literal's embedded
/// address (approvals are never written in those forms in practice, and
/// `allowed` has already judged the literal address itself safe).
fn category_of(ip: IpAddr) -> crate::special_use::Category {
    match ip {
        IpAddr::V4(ip) => crate::special_use::classify_v4(ip),
        IpAddr::V6(ip) => match ip.to_ipv4() {
            Some(v4) => crate::special_use::classify_v4(v4),
            None => crate::special_use::classify_v6(ip),
        },
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
    /// The approval check as it was before #196, kept as the reference: the
    /// only difference now is that a 6to4 or Teredo address is also judged by
    /// the IPv4 address it carries.
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
                    && let Some(v4) = crate::egress::translated_v4(ip)
                    && !allowed(IpAddr::V4(v4), development, http)
                {
                    return false;
                }
                ip.is_unique_local() || global
            }
        }
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
}
