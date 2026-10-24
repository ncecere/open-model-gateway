//! Shared OTLP validation and bounded HTTP delivery, not a worker, queue or
//! retry policy. Signal-specific config, deadlines and wire bodies stay with
//! the caller. Never expose destinations, header material or transport errors.
use std::time::Duration;

macro_rules! fixed_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum $name { $($variant),+ }
        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            pub const COUNT: usize = Self::ALL.len();
            pub fn as_str(self) -> &'static str { match self { $(Self::$variant => $text),+ } }
        }
    };
}
pub(crate) use fixed_enum;
fixed_enum!(Delivery { Accepted => "accepted", HttpStatus => "http_status", Transport => "transport", Timeout => "timeout", Acknowledgement => "acknowledgement" });

const OTLP_MAX_HEADERS: usize = 16;
const OTLP_RESERVED_HEADERS: [&str; 9] = [
    "content-type",
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "upgrade",
    "te",
    "keep-alive",
    "expect",
];
const ACK_LIMIT: usize = 64 * 1024;

pub(crate) fn endpoint(
    setting: &str,
    value: &str,
    path: &str,
    environment: crate::config::Environment,
    exchange: Duration,
) -> anyhow::Result<(reqwest::Url, reqwest::Client)> {
    let invalid = || {
        anyhow::anyhow!(
            "{setting} is invalid (full signal URL required; HTTPS except development loopback)"
        )
    };
    let url = reqwest::Url::parse(value).map_err(|_| invalid())?;
    let development = environment == crate::config::Environment::Development;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    anyhow::ensure!(
        url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == path
            && !value.chars().any(|c| c.is_whitespace() || c.is_control())
            && (url.scheme() == "https" || (development && loopback && url.scheme() == "http")),
        "{setting} is invalid (full signal URL required; HTTPS except development loopback)"
    );
    let client = export_http_client(
        &url,
        crate::egress::Purpose::Telemetry,
        development,
        Duration::from_secs(1),
        exchange,
    )
    .map_err(|_| invalid())?;
    Ok((url, client))
}

/// Header secret references must be dedicated even when another pointer's
/// consumer is disabled. Exclude only our own pointer from the reservation
/// check; no secret value (or OTEL setting) is read during collision checks.
pub(crate) fn headers(
    setting: &str,
    name: &str,
    get: &impl Fn(&str) -> Option<String>,
) -> anyhow::Result<reqwest::header::HeaderMap> {
    let invalid = || {
        anyhow::anyhow!(
            "{setting} requires a dedicated, valid headers secret variable (at most 16 headers and 4 KiB)"
        )
    };
    check_secret_env_name(setting, name, get).map_err(|_| invalid())?;
    anyhow::ensure!(
        !crate::providers::secrets::SECRET_POINTERS
            .iter()
            .filter(|key| **key != setting)
            .any(|key| get(key).is_some_and(|v| v.trim() == name))
            && !crate::providers::secrets::reserved_env_name(name, &|key| if key == setting {
                None
            } else {
                get(key)
            }),
        "{setting} requires a dedicated, non-reserved secret variable"
    );
    let value = zeroize::Zeroizing::new(get(name).ok_or_else(invalid)?);
    anyhow::ensure!(value.len() <= 4096, "{setting} exceeds the 4 KiB limit");
    parse_otlp_headers(setting, &value).map_err(|_| invalid())
}

/// Audit-export's historical name rules. Telemetry adds full reserved-name
/// and pointer-disjointness checks in `headers` above.
pub(crate) fn check_secret_env_name(
    setting: &str,
    name: &str,
    get: &impl Fn(&str) -> Option<String>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
        "{setting} must name an upper-case environment variable"
    );
    let listed =
        |var: &str| get(var).is_some_and(|list| list.split(',').map(str::trim).any(|n| n == name));
    anyhow::ensure!(
        !listed("GATEWAY_SECRET_ENV_ALLOWLIST") && !listed(crate::email::SMTP_ALLOWLIST_VAR),
        "{setting} names {name}, which a provider or SMTP allowlist can also read; use a variable only the audit export reads"
    );
    anyhow::ensure!(
        !crate::startup::SECRET_NAMES.contains(&name)
            && !name.starts_with("AWS_")
            && !name.starts_with("GATEWAY_"),
        "{setting} names a reserved variable ({name})"
    );
    Ok(())
}

/// Audit-style `Key=Value,Key2=Value2`. Errors never include a header name or
/// value: a misplaced `=` could put a token in the name.
pub(crate) fn parse_otlp_headers(
    setting: &str,
    value: &str,
) -> anyhow::Result<reqwest::header::HeaderMap> {
    let mut headers = reqwest::header::HeaderMap::new();
    let items: Vec<&str> = value.split(',').collect();
    anyhow::ensure!(
        items.len() <= OTLP_MAX_HEADERS,
        "{setting}: at most {OTLP_MAX_HEADERS} headers"
    );
    for (i, item) in items.into_iter().enumerate() {
        let n = i + 1;
        let (name, value) = item
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("{setting}: header {n} is not Key=Value"))?;
        let (name, value) = (name.trim_matches(' '), value.trim_matches(' '));
        anyhow::ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)),
            "{setting}: header {n} has an invalid name"
        );
        anyhow::ensure!(
            !OTLP_RESERVED_HEADERS
                .iter()
                .any(|reserved| name.eq_ignore_ascii_case(reserved)),
            "{setting}: header {n} is one the gateway sets itself (Content-Type, Host or Content-Length)"
        );
        anyhow::ensure!(
            !value.is_empty() && value.bytes().all(|b| (b' '..=b'~').contains(&b)),
            "{setting}: header {n} has an empty or invalid value (visible ASCII and spaces only)"
        );
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| anyhow::anyhow!("{setting}: header {n} has an invalid name"))?;
        let mut value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|_| anyhow::anyhow!("{setting}: header {n} has an invalid value"))?;
        value.set_sensitive(true);
        headers.append(name, value);
    }
    Ok(headers)
}

/// Shared export transport, never a durable queue or retry policy.
pub(crate) fn export_http_client(
    url: &reqwest::Url,
    purpose: crate::egress::Purpose,
    development: bool,
    connect_timeout: Duration,
    exchange_timeout: Duration,
) -> anyhow::Result<reqwest::Client> {
    let policy = crate::egress::policy().permit_loopback(development);
    crate::egress::check_url(&policy, purpose, url)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .dns_resolver(crate::egress::resolver_with(purpose, policy))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(exchange_timeout)
        .connect_timeout(connect_timeout)
        .build()
        .map_err(|_| anyhow::anyhow!("cannot build the export client"))?;
    Ok(client)
}

/// One exchange, including a bounded acknowledgement. Never retry, log a
/// collector response, or return a reqwest error containing the destination.
pub(crate) async fn post(
    client: &reqwest::Client,
    url: &reqwest::Url,
    headers: &reqwest::header::HeaderMap,
    body: Vec<u8>,
    count: usize,
    rejected_key: &str,
    exchange: Duration,
) -> Result<u64, Delivery> {
    tokio::time::timeout(exchange, async {
        let mut response = client
            .post(url.clone())
            .headers(headers.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    Delivery::Timeout
                } else {
                    Delivery::Transport
                }
            })?;
        if !response.status().is_success() {
            return Err(Delivery::HttpStatus);
        }
        let mut ack = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            if error.is_timeout() {
                Delivery::Timeout
            } else {
                Delivery::Acknowledgement
            }
        })? {
            if ack.len() + chunk.len() > ACK_LIMIT {
                return Err(Delivery::Acknowledgement);
            }
            ack.extend_from_slice(&chunk);
        }
        acknowledgement(&ack, count, rejected_key)
    })
    .await
    .unwrap_or(Err(Delivery::Timeout))
}

fn acknowledgement(ack: &[u8], count: usize, rejected_key: &str) -> Result<u64, Delivery> {
    if ack.is_empty() {
        return Ok(0);
    }
    let value: serde_json::Value =
        serde_json::from_slice(ack).map_err(|_| Delivery::Acknowledgement)?;
    if !value.is_object()
        || (!value["partialSuccess"].is_null() && !value["partialSuccess"].is_object())
    {
        return Err(Delivery::Acknowledgement);
    }
    let rejected = &value["partialSuccess"][rejected_key];
    if rejected.is_null() {
        return Ok(0);
    }
    let n = rejected
        .as_u64()
        .or_else(|| rejected.as_str()?.parse().ok())
        .ok_or(Delivery::Acknowledgement)?;
    Ok(n.min(count as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn acknowledgement_uses_only_the_selected_signals_count_key() {
        let ack = br#"{"partialSuccess":{"rejectedSpans":"1","rejectedDataPoints":"999","errorMessage":"never returned"}}"#;
        assert_eq!(acknowledgement(ack, 3, "rejectedSpans"), Ok(1));
        assert_eq!(acknowledgement(ack, 3, "rejectedDataPoints"), Ok(3));
        assert_eq!(acknowledgement(ack, 3, "rejectedLogRecords"), Ok(0));
        for ack in [
            br#"{"partialSuccess":{"rejectedDataPoints":-1}}"#.as_slice(),
            br#"{"partialSuccess":{"rejectedDataPoints":"18446744073709551616"}}"#,
            br#"{"partialSuccess":{"rejectedDataPoints":1.5}}"#,
        ] {
            assert_eq!(
                acknowledgement(ack, 3, "rejectedDataPoints"),
                Err(Delivery::Acknowledgement)
            );
        }
    }
}
