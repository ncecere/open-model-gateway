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
/// Pinned local-upstream approvals, loaded from operator configuration.
/// Raw approval JSON is not a public production constructor:
///
/// ```compile_fail
/// use open_model_gateway::providers::local::endpoints::ApprovedEndpoints;
/// let _ = ApprovedEndpoints::parse("[]", "development");
/// ```
pub struct ApprovedEndpoints {
    endpoints: BTreeMap<String, Endpoint>,
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
    /// Test-only constructor for fixtures that own their approval JSON.
    /// Runs the production validator without mutating process environment.
    #[cfg(feature = "integration-tests")]
    #[doc(hidden)]
    pub fn parse_for_integration_tests(
        value: &str,
        environment: &str,
    ) -> Result<Self, InferenceError> {
        Self::parse(value, environment)
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
        Ok(Self { endpoints })
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
/// This file's own mapping from the shared special-use disposition (#203) to
/// an accept/refuse decision, the same for IPv4 and IPv6. Everything the
/// shared table refuses outright (`Disposition::Refused`, decided once for
/// this file and `egress.rs`) is refused here too, in every environment and
/// over both schemes; carrier-grade NAT (`Disposition::Cgnat`) is refused
/// unconditionally, as it always was here (egress alone lets a private
/// allowlist reach it); loopback only in development; private addresses over
/// either scheme; public addresses over HTTPS only.
fn decide(category: crate::special_use::Category, development: bool, http: bool) -> bool {
    use crate::special_use::Disposition;
    match category.disposition() {
        Disposition::Loopback => development,
        Disposition::Private => true,
        Disposition::Public => !http,
        Disposition::Refused | Disposition::Cgnat => false,
    }
}

fn allowed(ip: IpAddr, development: bool, http: bool) -> bool {
    match ip {
        IpAddr::V4(ip) => decide(crate::special_use::classify_v4(ip), development, http),
        IpAddr::V6(ip) => {
            if ip.is_loopback() {
                return development;
            }
            // Include deprecated IPv4-compatible encodings, not only mapped ones.
            if let Some(v4) = ip.to_ipv4() {
                return decide(crate::special_use::classify_v4(v4), development, http);
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
                && !decide(crate::special_use::classify_v4(v4), development, http)
            {
                return false;
            }
            decide(crate::special_use::classify_v6(ip), development, http)
        }
    }
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

    /// The ranges #203 release N+1 refuses for approvals (the issue's list),
    /// written as CIDR masks from the issue text, independently of
    /// `special_use.rs`'s octet checks and of `allowed`: a drift in either is
    /// caught by comparing against this.
    fn oracle_in_v4(ip: std::net::Ipv4Addr, base: [u8; 4], prefix: u32) -> bool {
        let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
        u32::from(ip) & mask == u32::from(std::net::Ipv4Addr::from(base)) & mask
    }
    fn oracle_in_v6(ip: std::net::Ipv6Addr, base: [u16; 8], prefix: u32) -> bool {
        let mask = u128::MAX.checked_shl(128 - prefix).unwrap_or(0);
        u128::from(ip) & mask == u128::from(std::net::Ipv6Addr::from(base)) & mask
    }
    fn oracle_refused_range_v4(ip: std::net::Ipv4Addr) -> bool {
        [
            ([198, 18, 0, 0], 15),   // benchmarking
            ([192, 0, 0, 0], 24),    // IETF protocol assignments
            ([192, 88, 99, 0], 24),  // deprecated 6to4 relays
            ([192, 0, 2, 0], 24),    // documentation
            ([198, 51, 100, 0], 24), // documentation
            ([203, 0, 113, 0], 24),  // documentation
        ]
        .iter()
        .any(|(base, prefix)| oracle_in_v4(ip, *base, *prefix))
    }
    /// Whether an approval address is in one of the ranges #203 refuses,
    /// after decoding any IPv4 address it carries (mapped, compatible, 6to4
    /// or Teredo, per the RFCs). NAT64 is refused outright elsewhere and
    /// loopback is never in these ranges.
    fn oracle_refused_range(ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(ip) => oracle_refused_range_v4(ip),
            IpAddr::V6(ip) => {
                let s = ip.segments();
                let low = std::net::Ipv4Addr::from((u32::from(s[6]) << 16) | u32::from(s[7]));
                if s[..6] == [0; 6] || (s[..5] == [0; 5] && s[5] == 0xffff) {
                    return oracle_refused_range_v4(low);
                }
                if (s[0] == 0x2002 || (s[0] == 0x2001 && s[1] == 0))
                    && oracle_translated_v4(ip).is_some_and(oracle_refused_range_v4)
                {
                    return true;
                }
                [
                    ([0x2001, 0xdb8, 0, 0, 0, 0, 0, 0], 32),  // documentation
                    ([0x3fff, 0, 0, 0, 0, 0, 0, 0], 20),      // documentation (RFC 9637)
                    ([0x2001, 2, 0, 0, 0, 0, 0, 0], 48),      // benchmarking
                    ([0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0], 32), // AWS metadata range
                ]
                .iter()
                .any(|(base, prefix)| oracle_in_v6(ip, *base, *prefix))
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

    /// #203 release N+1: the special-use ranges v0.6.0 through v0.8.0 still
    /// accepted over HTTPS (v0.8.0 warned) are refused, in every environment.
    /// The exhaustive per-range boundary tests are in `refused_range_tests`.
    #[test]
    fn special_use_ranges_approved_before_are_now_refused() {
        for address in [
            "198.18.0.1",
            "192.0.0.1",
            "192.88.99.1",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "2001:db8::1",
            "3fff::1",
            "2001:2::1",
            "fd00:ec2::1",
        ] {
            for development in [false, true] {
                assert!(
                    !allowed(address.parse().unwrap(), development, false),
                    "{address}"
                );
            }
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(4096))]
        /// Against the behaviour before #196: nothing is newly allowed, and
        /// the only newly refused addresses are 6to4 and Teredo addresses
        /// whose embedded IPv4 the plain rules refuse (#196), or an address
        /// in one of the special-use ranges #203 refuses (release N+1).
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
                let by_embedded = embedded
                    .is_some_and(|inner| !http && !before(IpAddr::V4(inner), development, false));
                proptest::prop_assert!(by_embedded || oracle_refused_range(ip), "{}", ip);
            }
            // Nothing else moved for a form with no embedded address to judge,
            // outside the refused ranges.
            if embedded.is_none() && !oracle_refused_range(ip) {
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
            a(191, 255, 255, 255),
            a(192, 0, 1, 0),
            a(192, 0, 1, 255),
            a(192, 0, 3, 0), // just outside the IETF and documentation /24s
            a(192, 88, 98, 255),
            a(192, 88, 100, 0), // just outside the deprecated 6to4 relay /24
            a(198, 17, 255, 255),
            a(198, 20, 0, 0), // just outside benchmarking
            a(198, 51, 99, 255),
            a(198, 51, 101, 0),
            a(203, 0, 112, 255),
            a(203, 0, 114, 0), // just outside the documentation /24s
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
            [
                0x2001, 0xdb7, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff,
            ], // just below documentation
            [0x2001, 0xdb9, 0, 0, 0, 0, 0, 0], // just above documentation
            [0x3fff, 0x1000, 0, 0, 0, 0, 0, 0], // just above 3fff::/20
            [0x2001, 1, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff], // just below benchmarking
            [0x2001, 2, 1, 0, 0, 0, 0, 0], // just above benchmarking
            [
                0xfd00, 0x0ec1, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff,
            ], // just below AWS range
            [0xfd00, 0x0ec3, 0, 0, 0, 0, 0, 0], // just above AWS range
            [0x2000, 0, 0, 0, 0, 0, 0, 0], // public (2000::/3) start
            [
                0x3fff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff,
            ], // 2000::/3 end
        ]
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(8192))]
        /// #203 review finding, updated for release N+1: `allowed` decides
        /// *exactly* as the frozen, self-contained a99bb00 oracle (`before`)
        /// for every encoding and setting, except that an address in one of
        /// the special-use ranges #203 refuses (decided by the independent
        /// `oracle_refused_range`, after decoding any embedded IPv4 address)
        /// is now refused too. Everything else keeps its exact decision:
        /// public and RFC 1918 approvals, loopback in development, NAT64
        /// refused, 6to4/Teredo judged by their embedded address (#196).
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
                before(ip, development, http) && !oracle_refused_range(ip),
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

/// #203, release N+1: every range the issue lists is refused for approvals,
/// at its first and last address with its neighbours untouched, through every
/// encoding that carries the same IPv4 address, and loading any such approval
/// fails (startup refuses the whole configuration). Controls prove ordinary
/// approvals are unaffected.
#[cfg(test)]
mod refused_range_tests {
    use super::*;

    /// (first, last, just below, just above, category) of each listed range.
    type Row = (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    );

    fn ranges() -> Vec<Row> {
        vec![
            (
                "198.18.0.0",
                "198.19.255.255",
                "198.17.255.255",
                "198.20.0.0",
                "benchmarking",
            ),
            (
                "192.0.0.0",
                "192.0.0.255",
                "191.255.255.255",
                "192.0.1.0",
                "IETF protocol assignments",
            ),
            (
                "192.88.99.0",
                "192.88.99.255",
                "192.88.98.255",
                "192.88.100.0",
                "deprecated 6to4 relay",
            ),
            (
                "192.0.2.0",
                "192.0.2.255",
                "192.0.1.255",
                "192.0.3.0",
                "documentation",
            ),
            (
                "198.51.100.0",
                "198.51.100.255",
                "198.51.99.255",
                "198.51.101.0",
                "documentation",
            ),
            (
                "203.0.113.0",
                "203.0.113.255",
                "203.0.112.255",
                "203.0.114.0",
                "documentation",
            ),
            (
                "2001:db8::",
                "2001:db8:ffff:ffff:ffff:ffff:ffff:ffff",
                "2001:db7:ffff:ffff:ffff:ffff:ffff:ffff",
                "2001:db9::",
                "documentation",
            ),
            (
                "3fff::",
                "3fff:0fff:ffff:ffff:ffff:ffff:ffff:ffff",
                "3ffe:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
                "3fff:1000::",
                "documentation",
            ),
            (
                "2001:2::",
                "2001:2:0:ffff:ffff:ffff:ffff:ffff",
                "2001:1:ffff:ffff:ffff:ffff:ffff:ffff",
                "2001:2:1::",
                "benchmarking",
            ),
            (
                "fd00:ec2::",
                "fd00:ec2:ffff:ffff:ffff:ffff:ffff:ffff",
                "fd00:ec1:ffff:ffff:ffff:ffff:ffff:ffff",
                "fd00:ec3::",
                "AWS metadata range (fd00:ec2::/32)",
            ),
        ]
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// The JSON for one approval of `address` at `scheme` (a literal host,
    /// or a DNS name when `named`).
    fn approval(scheme: &str, address: &str, named: bool) -> String {
        let host = if named {
            "models.internal".to_owned()
        } else if address.contains(':') {
            format!("[{address}]")
        } else {
            address.to_owned()
        };
        serde_json::json!([{"endpoint": format!("{scheme}://{host}/v1"), "addresses": [address]}])
            .to_string()
    }

    #[test]
    fn each_listed_range_is_refused_at_both_ends_in_every_setting() {
        for (first, last, _, _, _) in ranges() {
            for address in [first, last] {
                for development in [false, true] {
                    for http in [false, true] {
                        assert!(
                            !allowed(ip(address), development, http),
                            "{address} development={development} http={http}"
                        );
                    }
                }
            }
        }
    }

    /// The addresses just outside each range keep their exact decision:
    /// accepted over HTTPS (public, or unique-local), and over plain HTTP
    /// only when private (unique-local).
    #[test]
    fn the_neighbours_of_each_range_keep_their_decision() {
        for (_, _, below, above, _) in ranges() {
            for address in [below, above] {
                let private = matches!(ip(address), IpAddr::V6(v6) if v6.is_unique_local());
                for development in [false, true] {
                    assert!(allowed(ip(address), development, false), "{address}");
                    assert_eq!(
                        allowed(ip(address), development, true),
                        private,
                        "{address} over http"
                    );
                }
            }
        }
    }

    /// The AWS IPv6 metadata `/32` was accepted over plain HTTP too, like
    /// an ordinary private address: it is refused there now as well.
    #[test]
    fn the_aws_metadata_range_is_refused_over_plain_http_too() {
        for address in [
            "fd00:ec2::",
            "fd00:ec2::1",
            "fd00:ec2::253",
            "fd00:ec2::255",
        ] {
            for development in [false, true] {
                assert!(!allowed(ip(address), development, true), "{address}");
            }
        }
        // An unrelated unique-local address is still a private address.
        assert!(allowed(ip("fd12::1"), false, true));
        assert!(allowed(ip("fd00:ec1::1"), false, true));
    }

    #[test]
    fn loading_an_approval_in_a_listed_range_fails() {
        for (first, last, below, above, _) in ranges() {
            for address in [first, last] {
                for named in [false, true] {
                    for scheme in ["https", "http"] {
                        for environment in ["production", "development"] {
                            assert!(
                                ApprovedEndpoints::parse(
                                    &approval(scheme, address, named),
                                    environment
                                )
                                .is_err(),
                                "{scheme} {address} named={named} {environment}"
                            );
                        }
                    }
                }
                // One refused approval refuses the whole configuration.
                let mixed = serde_json::json!([
                    {"endpoint": "https://public.example/v1", "addresses": ["8.8.8.8"]},
                    {"endpoint": "https://models.internal/v1", "addresses": [address]},
                ])
                .to_string();
                assert!(
                    ApprovedEndpoints::parse(&mixed, "production").is_err(),
                    "{address}"
                );
            }
            // The neighbours load, over HTTPS.
            for address in [below, above] {
                for named in [false, true] {
                    assert!(
                        ApprovedEndpoints::parse(&approval("https", address, named), "production")
                            .is_ok(),
                        "{address}"
                    );
                }
            }
        }
    }

    /// A list of several addresses is refused when any one is in a range.
    #[test]
    fn one_refused_address_among_several_refuses_the_approval() {
        let value = serde_json::json!([{
            "endpoint": "https://models.internal/v1",
            "addresses": ["8.8.8.8", "10.1.2.3", "198.18.0.1"],
        }])
        .to_string();
        assert!(ApprovedEndpoints::parse(&value, "production").is_err());
        let ordinary = serde_json::json!([{
            "endpoint": "https://models.internal/v1",
            "addresses": ["8.8.8.8", "10.1.2.3"],
        }])
        .to_string();
        assert!(ApprovedEndpoints::parse(&ordinary, "production").is_ok());
    }

    /// Every encoding of an IPv4 address in an IPv4 range is judged by that
    /// address: refused in a listed range, accepted just outside it.
    #[test]
    fn the_ipv4_ranges_are_refused_through_every_encoding() {
        for (first, last, below, above, _) in ranges().into_iter().take(6) {
            for (address, refused) in [(first, true), (last, true), (below, false), (above, false)]
            {
                let v4: std::net::Ipv4Addr = address.parse().unwrap();
                let o = v4.octets();
                let mapped = IpAddr::V6(v4.to_ipv6_mapped());
                let compatible = IpAddr::V6(std::net::Ipv6Addr::new(
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    u16::from_be_bytes([o[0], o[1]]),
                    u16::from_be_bytes([o[2], o[3]]),
                ));
                let [six_to_four, teredo] = six_to_four_and_teredo(address);
                for form in [IpAddr::V4(v4), mapped, compatible, six_to_four, teredo] {
                    for development in [false, true] {
                        assert_eq!(
                            allowed(form, development, false),
                            !refused,
                            "{form} (carrying {address}) development={development}"
                        );
                    }
                }
            }
        }
    }

    /// Egress refuses every listed range for every other client, even under
    /// the most permissive policy: the two checks agree.
    #[test]
    fn egress_refuses_every_listed_range_too() {
        let everything = crate::egress::Policy::new(
            true,
            crate::egress::Networks::parse(crate::egress::PRIVATE_ALLOW_ENV, "0.0.0.0/0,::/0")
                .unwrap(),
        );
        for (first, last, below, above, _) in ranges() {
            for address in [first, last] {
                assert!(!everything.allows(ip(address)), "{address}");
            }
            for address in [below, above] {
                assert!(everything.allows(ip(address)), "{address}");
            }
        }
    }

    /// NAT64 approvals stay refused outright.
    #[test]
    fn nat64_approvals_stay_refused() {
        for address in ["64:ff9b::808:808", "64:ff9b::a9fe:a9fe", "64:ff9b::c612:1"] {
            for (development, http) in [(false, false), (true, false), (true, true)] {
                assert!(!allowed(ip(address), development, http), "{address}");
            }
            assert!(
                ApprovedEndpoints::parse(&approval("https", address, false), "production").is_err(),
                "{address}"
            );
        }
    }

    /// Control: an installation with only public and RFC 1918 approvals,
    /// loopback in development, and 6to4/Teredo literals carrying such
    /// addresses loads exactly as before.
    #[test]
    fn ordinary_approvals_are_unaffected() {
        let ordinary = r#"[
            {"endpoint":"https://public.example/v1","addresses":["8.8.8.8"]},
            {"endpoint":"http://private.example/v1","addresses":["10.1.2.3"]},
            {"endpoint":"http://private172.example/v1","addresses":["172.16.0.1"]},
            {"endpoint":"http://private192.example/v1","addresses":["192.168.1.1"]},
            {"endpoint":"https://private6.example/v1","addresses":["fd12::1"]},
            {"endpoint":"http://private6http.example/v1","addresses":["fd12::2"]},
            {"endpoint":"https://public6.example/v1","addresses":["2606:4700::1111"]},
            {"endpoint":"https://near.example/v1","addresses":["198.17.255.255","198.20.0.0"]}
        ]"#;
        assert!(ApprovedEndpoints::parse(ordinary, "production").is_ok());
        assert!(
            ApprovedEndpoints::parse(
                r#"[{"endpoint":"http://127.0.0.1:8000/v1","addresses":["127.0.0.1"]}]"#,
                "development"
            )
            .is_ok()
        );
        for embedded in ["8.8.8.8", "10.1.2.3"] {
            for form in six_to_four_and_teredo(embedded) {
                assert!(allowed(form, false, false), "{form}");
                assert!(
                    ApprovedEndpoints::parse(
                        &approval("https", &form.to_string(), false),
                        "production"
                    )
                    .is_ok(),
                    "{form}"
                );
            }
        }
    }

    /// Carrier-grade NAT, link-local, metadata and the rest stay refused.
    #[test]
    fn previously_refused_approvals_stay_refused() {
        for address in [
            "100.64.0.1",
            "100.100.100.200",
            "169.254.169.254",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "::",
            "fd00:ec2::254",
        ] {
            for (development, http) in [(false, false), (true, false), (true, true)] {
                assert!(!allowed(ip(address), development, http), "{address}");
            }
        }
    }

    /// A 6to4 or Teredo literal for each IPv4 range, built from the embedded
    /// address rather than a hand-computed literal.
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

    /// #244: only `3fff::/20` is refused. The addresses just outside it
    /// (inside the old, over-wide `3ff0::/12` documentation mask) are
    /// ordinary global unicast and stay accepted over HTTPS.
    #[test]
    fn only_3fff_20_is_refused_not_the_wider_3ff0_12() {
        assert!(!allowed(ip("3fff::1"), false, false));
        let strict = crate::egress::Policy::default();
        for address in ["3ff0::1", "3ffe:ffff::1", "3ff1:dead::1", "3fff:1000::1"] {
            assert!(allowed(ip(address), false, false), "{address}");
            assert!(strict.allows(ip(address)), "{address}");
            assert!(
                ApprovedEndpoints::parse(&approval("https", address, false), "production").is_ok(),
                "{address}"
            );
        }
    }
}
