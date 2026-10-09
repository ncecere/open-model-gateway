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
fn allowed(ip: IpAddr, development: bool, http: bool) -> bool {
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
            // Include deprecated IPv4-compatible encodings, not only mapped ones.
            if let Some(v4) = ip.to_ipv4() {
                return allowed(IpAddr::V4(v4), development, http);
            }
            if ip.is_unspecified()
                || ip.is_unicast_link_local()
                || ip.is_multicast()
                || ip
                    == "fd00:ec2::254"
                        .parse::<std::net::Ipv6Addr>()
                        .expect("constant metadata IP")
            {
                return false;
            }
            // Deny translation/special-use ranges that can obscure a forbidden
            // IPv4 destination (including NAT64); ordinary global v6 is 2000::/3.
            ip.is_unique_local() || (!http && (ip.segments()[0] & 0xe000) == 0x2000)
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
