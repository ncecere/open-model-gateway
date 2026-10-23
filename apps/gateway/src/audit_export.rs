//! Audit export to a SIEM through a signed HTTPS webhook (#106), syslog over
//! TLS (#107) or OTLP/HTTP JSON logs (#108).
//!
//! - **Operator-controlled destination.** `GATEWAY_AUDIT_WEBHOOK_URL` and
//!   `GATEWAY_AUDIT_WEBHOOK_SECRET_ENV` (the name of the variable holding the
//!   signing secret) come from the environment; Platform Admins can see the
//!   status and send a test, never change or switch off the export.
//! - **What is exported.** The platform audit view's events (no workspace, or
//!   a team or project), queued by a trigger in the transaction that wrote
//!   them (migration 0044). Personal-workspace events never are.
//! - **Delivery.** One leased exporter posts the queue in order, up to
//!   [`BATCH`] events per request, and deletes them only after a 2xx, in a
//!   transaction fenced by its lease term. Failures back off exponentially
//!   (at most [`MAX_BACKOFF`]) and are retried forever. At-least-once: a
//!   receiver dedups on each event's `id`.
//! - **Signature.** `X-OMG-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256 of
//!   "<t>.<body>">`; `X-OMG-Delivery` is a fresh id per request.
//! - **Gap detection (#174).** Each event carries `seq`, numbered by the
//!   exporter before its first attempt (gap-free, never reused; a database
//!   trigger refuses rewinding or reassigning), and an idle export sends a
//!   signed heartbeat with `next_seq` every [`HEARTBEAT`]. A receiver alerts on
//!   a gap or on silence.
//! - **Syslog over TLS (#107).** `GATEWAY_AUDIT_SYSLOG_URL=syslog+tls://host[:port]`
//!   instead of the webhook (one destination at a time): one RFC 5424 message
//!   per event, RFC 5425 octet-counted, over TLS verified against the public
//!   roots or `GATEWAY_AUDIT_SYSLOG_CA_FILE`, optionally with a client
//!   certificate. Same queue, lease, numbering and backoff. One connection per
//!   batch; it counts as delivered once every frame is written, close_notify
//!   sent and the receiver closed the connection without a reset. Syslog has
//!   no application-level acknowledgement: a receiver that fails after reading
//!   can lose events, which shows as a `seq` gap.
//! - **OTLP logs (#108).** `GATEWAY_AUDIT_OTLP_URL` (the collector's full
//!   logs endpoint) instead: one OTLP/HTTP JSON request per batch, one
//!   LogRecord per event whose body is the webhook's event object, with
//!   optional headers from the variable `GATEWAY_AUDIT_OTLP_HEADERS_ENV`
//!   names. A separate destination from any operational telemetry: `OTEL_*`
//!   is never read. Records the collector refuses in a 2xx `partialSuccess`
//!   are not retried (the OTLP rule): they are counted, recorded as
//!   `otlp_rejected`, and show as a `seq` gap.

use std::{net::Ipv6Addr, sync::Arc, time::Duration};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use crate::store::Store;

pub const URL_ENV: &str = "GATEWAY_AUDIT_WEBHOOK_URL";
pub const SECRET_ENV_ENV: &str = "GATEWAY_AUDIT_WEBHOOK_SECRET_ENV";
pub const SYSLOG_URL_ENV: &str = "GATEWAY_AUDIT_SYSLOG_URL";
pub const SYSLOG_CA_FILE_ENV: &str = "GATEWAY_AUDIT_SYSLOG_CA_FILE";
pub const SYSLOG_CLIENT_CERT_FILE_ENV: &str = "GATEWAY_AUDIT_SYSLOG_CLIENT_CERT_FILE";
pub const SYSLOG_CLIENT_KEY_FILE_ENV: &str = "GATEWAY_AUDIT_SYSLOG_CLIENT_KEY_FILE";
pub const OTLP_URL_ENV: &str = "GATEWAY_AUDIT_OTLP_URL";
pub const OTLP_HEADERS_ENV_ENV: &str = "GATEWAY_AUDIT_OTLP_HEADERS_ENV";
/// Headers the OTLP variable may set.
const OTLP_MAX_HEADERS: usize = 16;
/// Headers the request sets itself, so the variable can't.
/// Set by the client, or hop-by-hop headers that break HTTP/1 framing or are
/// forbidden in HTTP/2: never configurable.
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
/// The OTLP response is read up to this size; a larger one rejects nothing.
const OTLP_ACK_LIMIT: usize = 64 * 1024;
/// The OTLP instrumentation scope.
const OTLP_SCOPE: &str = "omg.audit";
/// OTLP SeverityNumber INFO.
const OTLP_SEVERITY_INFO: u8 = 9;
/// The syslog-over-TLS port (RFC 5425).
pub const SYSLOG_PORT: u16 = 6514;
const SYSLOG_SCHEME: &str = "syslog+tls://";
const APP_NAME: &str = "open-model-gateway";
/// Facility 13 (log audit) times 8, plus severity 5 (notice).
const PRI: u8 = 109;
const MSGID_MAX: usize = 32;
/// PEM files are read whole at startup, up to this size.
const PEM_FILE_LIMIT: u64 = 1024 * 1024;
/// The versioned event format.
pub const SCHEMA: &str = "omg.audit.v1";
/// Events per request.
pub const BATCH: i64 = 100;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-address TCP connect limit for syslog (inside [`REQUEST_TIMEOUT`]).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
pub const MAX_BACKOFF: Duration = Duration::from_secs(300);
const MIN_SECRET_BYTES: usize = 32;

/// A configured destination. The secret, the TLS files and key material are
/// never logged or returned.
pub struct Exporter {
    transport: Transport,
}

enum Transport {
    Webhook(Webhook),
    Syslog(Syslog),
    Otlp(Otlp),
}

struct Webhook {
    url: reqwest::Url,
    secret: zeroize::Zeroizing<Vec<u8>>,
    client: reqwest::Client,
}

struct Syslog {
    /// Without brackets for IPv6; resolved through the egress policy per run.
    host: String,
    port: u16,
    server_name: ServerName<'static>,
    tls: Arc<rustls::ClientConfig>,
    policy: crate::egress::Policy,
}

struct Otlp {
    url: reqwest::Url,
    /// From the operator's variable; every value is marked sensitive.
    headers: reqwest::header::HeaderMap,
    client: reqwest::Client,
}

/// What the receiver did with a delivered payload.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Delivery {
    /// Records an OTLP collector refused for good (`partialSuccess`); they
    /// are not retried.
    rejected: u64,
}

/// What one send carries; each transport encodes it in its own format.
enum Payload {
    Events {
        installation: Uuid,
        events: Vec<Value>,
    },
    Heartbeat {
        installation: Uuid,
        next_seq: i64,
    },
    Test {
        installation: Uuid,
    },
}

impl std::fmt::Debug for Exporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Exporter")
            .field("transport", &self.transport())
            .field("host", &self.host())
            .finish_non_exhaustive()
    }
}

/// This replica's export configuration. Only an explicit `off` turns export
/// off for the installation; a replica without any setting (a rolling deploy,
/// a debug pod) leaves it as it is and doesn't deliver.
pub enum Setting {
    Unset,
    Off,
    On(Exporter),
}

impl Setting {
    pub fn from_env() -> anyhow::Result<Self> {
        let development = std::env::var("GATEWAY_ENV").as_deref() == Ok("development");
        Self::from_lookup(|name| std::env::var(name).ok(), development)
    }

    pub fn from_lookup(
        get: impl Fn(&str) -> Option<String>,
        development: bool,
    ) -> anyhow::Result<Self> {
        let set = |name: &str| get(name).filter(|v| !v.trim().is_empty());
        let url = set(URL_ENV);
        let secret_name = set(SECRET_ENV_ENV);
        let syslog_file = [
            SYSLOG_CA_FILE_ENV,
            SYSLOG_CLIENT_CERT_FILE_ENV,
            SYSLOG_CLIENT_KEY_FILE_ENV,
        ]
        .into_iter()
        .find(|name| set(name).is_some());
        let headers_name = set(OTLP_HEADERS_ENV_ENV);
        // One destination at a time; only an explicit `off` turns export off.
        // `OTEL_*` is operational telemetry, never the audit destination.
        let urls = [
            (URL_ENV, url),
            (SYSLOG_URL_ENV, set(SYSLOG_URL_ENV)),
            (OTLP_URL_ENV, set(OTLP_URL_ENV)),
        ];
        let urls: Vec<(&str, String)> = urls
            .into_iter()
            .filter_map(|(name, value)| Some((name, value?.trim().to_owned())))
            .collect();
        let (off, on): (Vec<_>, Vec<_>) = urls.iter().partition(|(_, value)| value == "off");
        // `off` still checks for leftover settings below (they'd do nothing).
        let destination = match (off.first(), on.as_slice()) {
            (Some(_), []) => None,
            (Some((off, _)), [(on, _), ..]) => {
                anyhow::bail!("{off} is off and {on} a destination; set only one")
            }
            (None, [_, _, ..]) => anyhow::bail!(
                "set only one of {URL_ENV}, {SYSLOG_URL_ENV} and {OTLP_URL_ENV}: audit export has one destination"
            ),
            (None, []) => None,
            (None, [(name, value)]) => Some((*name, value.as_str())),
        };
        // Settings that belong to another transport (or to none) are errors.
        let transport = destination.map(|(name, _)| name);
        if transport != Some(URL_ENV) {
            anyhow::ensure!(
                secret_name.is_none(),
                "{SECRET_ENV_ENV} is set without {URL_ENV}{}",
                if transport.is_some() {
                    " (only the webhook uses it)"
                } else {
                    ""
                }
            );
        }
        if transport != Some(SYSLOG_URL_ENV)
            && let Some(name) = syslog_file
        {
            anyhow::bail!("{name} is set without {SYSLOG_URL_ENV}");
        }
        if transport != Some(OTLP_URL_ENV) {
            anyhow::ensure!(
                headers_name.is_none(),
                "{OTLP_HEADERS_ENV_ENV} is set without {OTLP_URL_ENV}"
            );
        }
        let url = match destination {
            None if !off.is_empty() => return Ok(Self::Off),
            None => return Ok(Self::Unset),
            Some((SYSLOG_URL_ENV, syslog)) => return Self::syslog_from(syslog, &set, development),
            Some((OTLP_URL_ENV, otlp)) => {
                let headers = match headers_name {
                    None => None,
                    Some(name) => {
                        check_secret_env_name(OTLP_HEADERS_ENV_ENV, &name, &get)?;
                        Some(zeroize::Zeroizing::new(get(&name).ok_or_else(|| {
                            anyhow::anyhow!(
                                "{OTLP_HEADERS_ENV_ENV} names a variable that isn't set"
                            )
                        })?))
                    }
                };
                return Ok(Self::On(Exporter::otlp(
                    otlp,
                    headers.as_deref().map(String::as_str),
                    development,
                )?));
            }
            Some((_, webhook)) => webhook.to_owned(),
        };
        let name =
            secret_name.ok_or_else(|| anyhow::anyhow!("{URL_ENV} needs {SECRET_ENV_ENV}"))?;
        check_secret_env_name(SECRET_ENV_ENV, &name, &get)?;
        let secret = get(&name)
            .ok_or_else(|| anyhow::anyhow!("{SECRET_ENV_ENV} names a variable that isn't set"))?;
        Ok(
            match Exporter::new(&url, secret.into_bytes(), development)? {
                Some(exporter) => Self::On(exporter),
                None => Self::Unset,
            },
        )
    }

    /// The syslog destination with its CA and client identity, read and
    /// parsed now so a bad file stops startup.
    fn syslog_from(
        url: &str,
        set: &impl Fn(&str) -> Option<String>,
        development: bool,
    ) -> anyhow::Result<Self> {
        let read = |name: &str| {
            set(name)
                .map(|path| read_pem_file(name, path.trim()))
                .transpose()
        };
        let ca = read(SYSLOG_CA_FILE_ENV)?;
        let (cert, key) = (
            read(SYSLOG_CLIENT_CERT_FILE_ENV)?,
            read(SYSLOG_CLIENT_KEY_FILE_ENV)?,
        );
        let identity = match (&cert, &key) {
            (Some(cert), Some(key)) => Some((cert.as_slice(), key.as_slice())),
            (None, None) => None,
            _ => anyhow::bail!(
                "{SYSLOG_CLIENT_CERT_FILE_ENV} and {SYSLOG_CLIENT_KEY_FILE_ENV} must be set together"
            ),
        };
        Ok(Self::On(Exporter::syslog(
            url,
            ca.as_ref().map(|ca| ca.as_slice()),
            identity,
            development,
        )?))
    }

    pub fn exporter(self) -> Option<Exporter> {
        match self {
            Self::On(exporter) => Some(exporter),
            _ => None,
        }
    }
}

impl Exporter {
    /// HTTPS only; plain HTTP only to loopback in development (and tests).
    pub fn new(url: &str, secret: Vec<u8>, development: bool) -> anyhow::Result<Option<Self>> {
        let secret = zeroize::Zeroizing::new(secret);
        anyhow::ensure!(
            secret.len() >= MIN_SECRET_BYTES,
            "the audit webhook secret must be at least {MIN_SECRET_BYTES} bytes"
        );
        let (url, client) = https_client(URL_ENV, url, development)?;
        Ok(Some(Self {
            transport: Transport::Webhook(Webhook {
                url,
                secret,
                client,
            }),
        }))
    }

    /// OTLP/HTTP JSON logs (#108) to the collector's full logs endpoint, with
    /// the URL checks and client of the webhook. `headers` is the
    /// `Key=Value,...` text of the operator's variable; errors never repeat it.
    pub fn otlp(url: &str, headers: Option<&str>, development: bool) -> anyhow::Result<Self> {
        let (url, client) = https_client(OTLP_URL_ENV, url.trim(), development)?;
        let headers = match headers {
            None => reqwest::header::HeaderMap::new(),
            Some(text) => parse_otlp_headers(OTLP_HEADERS_ENV_ENV, text)?,
        };
        Ok(Self {
            transport: Transport::Otlp(Otlp {
                url,
                headers,
                client,
            }),
        })
    }

    /// Syslog over TLS (#107) to `syslog+tls://host[:port]`. The server is
    /// verified for `host` against the public roots, or only `ca_pem` when
    /// given; `identity` is an optional PEM client certificate chain and key.
    /// Errors name the setting, never the file contents.
    pub fn syslog(
        url: &str,
        ca_pem: Option<&[u8]>,
        identity: Option<(&[u8], &[u8])>,
        development: bool,
    ) -> anyhow::Result<Self> {
        let (host, port) = parse_syslog_url(url.trim())?;
        // An address literal is checked now, like the webhook URL, so a
        // refused destination stops startup instead of retrying forever.
        let policy = crate::egress::policy().permit_loopback(development);
        if let Ok(ip) = host.trim_matches(['[', ']']).parse::<std::net::IpAddr>() {
            anyhow::ensure!(
                policy.allows(ip),
                "{SYSLOG_URL_ENV} points to an address the outbound address policy refuses"
            );
        }
        let server_name = ServerName::try_from(host.clone())
            .map_err(|_| anyhow::anyhow!("{SYSLOG_URL_ENV} has an invalid host"))?;
        let roots = match ca_pem {
            None => rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            },
            Some(pem) => {
                let mut roots = rustls::RootCertStore::empty();
                for cert in CertificateDer::pem_slice_iter(pem) {
                    let cert = cert.map_err(|_| {
                        anyhow::anyhow!("{SYSLOG_CA_FILE_ENV} is not a valid PEM file")
                    })?;
                    roots.add(cert).map_err(|_| {
                        anyhow::anyhow!(
                            "{SYSLOG_CA_FILE_ENV} holds a certificate that can't be parsed"
                        )
                    })?;
                }
                anyhow::ensure!(
                    !roots.is_empty(),
                    "{SYSLOG_CA_FILE_ENV} holds no PEM certificate"
                );
                roots
            }
        };
        let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| anyhow::anyhow!("cannot build the audit syslog TLS configuration"))?
        .with_root_certificates(roots);
        let config = match identity {
            None => builder.with_no_client_auth(),
            Some((cert, key)) => {
                let chain = CertificateDer::pem_slice_iter(cert)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| {
                        anyhow::anyhow!("{SYSLOG_CLIENT_CERT_FILE_ENV} is not a valid PEM file")
                    })?;
                anyhow::ensure!(
                    !chain.is_empty(),
                    "{SYSLOG_CLIENT_CERT_FILE_ENV} holds no PEM certificate"
                );
                let key = PrivateKeyDer::from_pem_slice(key).map_err(|_| {
                    anyhow::anyhow!("{SYSLOG_CLIENT_KEY_FILE_ENV} holds no PEM private key")
                })?;
                builder.with_client_auth_cert(chain, key).map_err(|_| {
                    anyhow::anyhow!(
                        "{SYSLOG_CLIENT_CERT_FILE_ENV} and {SYSLOG_CLIENT_KEY_FILE_ENV} are not a usable certificate and key"
                    )
                })?
            }
        };
        Ok(Self {
            transport: Transport::Syslog(Syslog {
                host,
                port,
                server_name,
                tls: Arc::new(config),
                policy,
            }),
        })
    }

    /// The destination host, for status (never the path or query).
    pub fn host(&self) -> &str {
        match &self.transport {
            Transport::Webhook(webhook) => webhook.url.host_str().unwrap_or_default(),
            Transport::Syslog(syslog) => &syslog.host,
            Transport::Otlp(otlp) => otlp.url.host_str().unwrap_or_default(),
        }
    }

    /// `webhook`, `syslog` or `otlp`, for status.
    pub fn transport(&self) -> &'static str {
        match &self.transport {
            Transport::Webhook(_) => "webhook",
            Transport::Syslog(_) => "syslog",
            Transport::Otlp(_) => "otlp",
        }
    }

    /// Deliver one payload. `Err(category)` unless the receiver took it: a
    /// webhook or OTLP 2xx, or a syslog stream written and closed cleanly.
    /// A collector's 2xx can still refuse some records ([`Delivery`]).
    async fn send(&self, payload: Payload) -> Result<Delivery, &'static str> {
        match &self.transport {
            Transport::Webhook(webhook) => {
                webhook.send(&payload).await.map(|()| Delivery::default())
            }
            Transport::Syslog(syslog) => syslog
                .send(&frames(payload))
                .await
                .map(|()| Delivery::default()),
            Transport::Otlp(otlp) => otlp.send(&payload).await,
        }
    }
}

/// The rules for a variable an audit-export setting names (the webhook
/// secret #106, the OTLP headers #108): an upper-case name, not on the
/// provider or SMTP allowlist, and not reserved. A Platform Admin chooses
/// where provider credentials and the SMTP password go: if either allowlist
/// could read it, an admin could learn it (#140's rule, here too).
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

/// `Key=Value,Key2=Value2` from the OTLP headers variable (#108). Errors say
/// which item is wrong, never its name or value (a misplaced `=` could put a
/// token in the name).
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
        // Spaces around `=` and `,` are ignored; a CR or LF never is.
        let (name, value) = (name.trim_matches(' '), value.trim_matches(' '));
        // An HTTP token (RFC 9110 5.6.2).
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
        // Visible ASCII and space: no CR, LF or other control characters.
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

/// The URL checks and client shared by the webhook and the OTLP collector:
/// HTTPS only (plain HTTP only to loopback in development), no credentials
/// or fragment, the outbound address policy, no proxy, redirects or retries.
fn https_client(
    setting: &str,
    url: &str,
    development: bool,
) -> anyhow::Result<(reqwest::Url, reqwest::Client)> {
    let url = reqwest::Url::parse(url).map_err(|_| anyhow::anyhow!("{setting} is not a URL"))?;
    let loopback = url.host_str().is_some_and(|h| {
        h == "localhost"
            || h.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    anyhow::ensure!(
        url.scheme() == "https" || (development && loopback && url.scheme() == "http"),
        "{setting} must be https:// (http only to loopback in development)"
    );
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
        "{setting} must not contain credentials or a fragment"
    );
    let client = export_http_client(
        &url,
        crate::egress::Purpose::Audit,
        development,
        Duration::from_secs(5),
        REQUEST_TIMEOUT,
    )?;
    Ok((url, client))
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

/// Read a PEM file at startup. Errors name the setting, never the contents.
fn read_pem_file(name: &str, path: &str) -> anyhow::Result<zeroize::Zeroizing<Vec<u8>>> {
    use std::io::Read;
    let unreadable = || anyhow::anyhow!("{name} names a file that can't be read");
    let file = std::fs::File::open(path).map_err(|_| unreadable())?;
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    file.take(PEM_FILE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unreadable())?;
    anyhow::ensure!(
        u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= PEM_FILE_LIMIT,
        "{name} names a file larger than 1 MiB"
    );
    Ok(bytes)
}

/// `syslog+tls://host[:port]`, nothing else: no path, query, credentials or
/// plaintext scheme. IPv6 literals in brackets.
fn parse_syslog_url(url: &str) -> anyhow::Result<(String, u16)> {
    let invalid =
        || anyhow::anyhow!("{SYSLOG_URL_ENV} must be syslog+tls://host[:port] (TLS only)");
    let rest = url
        .get(..SYSLOG_SCHEME.len())
        .filter(|scheme| scheme.eq_ignore_ascii_case(SYSLOG_SCHEME))
        .and_then(|_| url.get(SYSLOG_SCHEME.len()..))
        .ok_or_else(invalid)?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let (host, port) = match rest.strip_prefix('[') {
        Some(bracketed) => {
            let (host, after) = bracketed.split_once(']').ok_or_else(invalid)?;
            host.parse::<Ipv6Addr>().map_err(|_| invalid())?;
            match after {
                "" => (host, None),
                _ => (host, Some(after.strip_prefix(':').ok_or_else(invalid)?)),
            }
        }
        None => match rest.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        },
    };
    let port = match port {
        None => SYSLOG_PORT,
        Some(port) => port
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| port.parse::<u16>().ok())
            .flatten()
            .filter(|port| *port != 0)
            .ok_or_else(invalid)?,
    };
    anyhow::ensure!(crate::email::valid_host(host), invalid());
    Ok((host.to_ascii_lowercase(), port))
}

impl Webhook {
    /// The same signed JSON bodies as before #107.
    async fn send(&self, payload: &Payload) -> Result<(), &'static str> {
        let sent_at = chrono::Utc::now();
        let body = match payload {
            Payload::Events {
                installation,
                events,
            } => json!({
                "schema": SCHEMA,
                "installation_id": installation,
                "sent_at": sent_at,
                "events": events,
            }),
            Payload::Heartbeat {
                installation,
                next_seq,
            } => json!({
                "schema": SCHEMA,
                "installation_id": installation,
                "sent_at": sent_at,
                "heartbeat": true,
                "next_seq": next_seq,
                "events": [],
            }),
            Payload::Test { installation } => json!({
                "schema": SCHEMA,
                "installation_id": installation,
                "sent_at": sent_at,
                "test": true,
                "events": [],
            }),
        };
        let body = serde_json::to_vec(&body).map_err(|_| "body")?;
        match self.post(body).await? {
            200..=299 => Ok(()),
            status => Err(status_category(status)),
        }
    }

    /// POST one signed body. `Ok(status)` for any HTTP answer; `Err(category)`
    /// when there was none.
    async fn post(&self, body: Vec<u8>) -> Result<u16, &'static str> {
        let t = chrono::Utc::now().timestamp();
        let response = self
            .client
            .post(self.url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::USER_AGENT,
                concat!("open-model-gateway/", env!("CARGO_PKG_VERSION")),
            )
            .header("x-omg-delivery", Uuid::new_v4().to_string())
            .header("x-omg-signature", sign(&self.secret, t, &body))
            .body(body)
            .send()
            .await
            .map_err(|e| request_category(&e))?;
        Ok(response.status().as_u16())
    }
}

/// A request that got no HTTP answer.
fn request_category(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else {
        "connection"
    }
}

impl Otlp {
    /// One OTLP/HTTP JSON request. A 2xx is delivered; its body may say the
    /// collector refused some records for good (`partialSuccess`).
    async fn send(&self, payload: &Payload) -> Result<Delivery, &'static str> {
        let (body, records) = otlp_body(payload, chrono::Utc::now());
        let body = serde_json::to_vec(&body).map_err(|_| "body")?;
        let response = self
            .client
            .post(self.url.clone())
            .header(
                reqwest::header::USER_AGENT,
                concat!("open-model-gateway/", env!("CARGO_PKG_VERSION")),
            )
            .headers(self.headers.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| request_category(&e))?;
        match response.status().as_u16() {
            200..=299 => {}
            status => return Err(status_category(status)),
        }
        let rejected = rejected_records(&read_bounded(response, OTLP_ACK_LIMIT).await);
        Ok(Delivery {
            rejected: rejected.min(u64::try_from(records).unwrap_or(u64::MAX)),
        })
    }
}

/// At most `limit` bytes of a response body; empty when it is larger (so it
/// doesn't parse) or can't be read.
async fn read_bounded(mut response: reqwest::Response, limit: usize) -> Vec<u8> {
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if body.len() + chunk.len() <= limit => {
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return body,
            Ok(Some(_)) | Err(_) => return Vec::new(),
        }
    }
}

/// `partialSuccess.rejectedLogRecords` of an OTLP response (an int64: a
/// string or a number in JSON). Missing, zero, negative or unparsable is 0.
fn rejected_records(body: &[u8]) -> u64 {
    let Ok(ack) = serde_json::from_slice::<Value>(body) else {
        return 0;
    };
    let rejected = &ack["partialSuccess"]["rejectedLogRecords"];
    rejected
        .as_u64()
        .or_else(|| rejected.as_str()?.parse().ok())
        .unwrap_or(0)
}

/// Nanoseconds since the Unix epoch as a decimal string (the protobuf JSON
/// mapping of a 64-bit integer); `0` (unknown) outside its range.
fn unix_nanos(at: chrono::DateTime<chrono::Utc>) -> String {
    at.timestamp_nanos_opt().unwrap_or(0).to_string()
}

fn string_attribute(key: &str, value: &str) -> Value {
    json!({"key": key, "value": {"stringValue": value}})
}

fn int_attribute(key: &str, value: i64) -> Value {
    json!({"key": key, "value": {"intValue": value.to_string()}})
}

/// One INFO LogRecord.
fn log_record(time: String, observed: &str, body: String, attributes: Vec<Value>) -> Value {
    json!({
        "timeUnixNano": time,
        "observedTimeUnixNano": observed,
        "severityNumber": OTLP_SEVERITY_INFO,
        "severityText": "INFO",
        "body": {"stringValue": body},
        "attributes": attributes,
    })
}

/// The OTLP/HTTP JSON request body for a payload and how many records it
/// holds: one LogRecord per event, its body the event object the webhook
/// sends; a heartbeat or test is one record.
fn otlp_body(payload: &Payload, now: chrono::DateTime<chrono::Utc>) -> (Value, usize) {
    let observed = unix_nanos(now);
    let (installation, records) = match payload {
        Payload::Events {
            installation,
            events,
        } => (
            installation,
            events
                .iter()
                .map(|event| {
                    let time = event["occurred_at"]
                        .as_str()
                        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                        .map_or_else(
                            || "0".to_owned(),
                            |t| unix_nanos(t.with_timezone(&chrono::Utc)),
                        );
                    log_record(
                        time,
                        &observed,
                        event.to_string(),
                        vec![
                            string_attribute(
                                "event.name",
                                event["action"].as_str().unwrap_or_default(),
                            ),
                            string_attribute(
                                "omg.audit.id",
                                event["id"].as_str().unwrap_or_default(),
                            ),
                            int_attribute("omg.audit.seq", event["seq"].as_i64().unwrap_or(0)),
                        ],
                    )
                })
                .collect::<Vec<_>>(),
        ),
        Payload::Heartbeat {
            installation,
            next_seq,
        } => (
            installation,
            vec![log_record(
                observed.clone(),
                &observed,
                format!(r#"{{"schema":"{SCHEMA}","heartbeat":true,"next_seq":{next_seq}}}"#),
                vec![
                    string_attribute("event.name", "omg.audit.heartbeat"),
                    int_attribute("omg.audit.next_seq", *next_seq),
                ],
            )],
        ),
        Payload::Test { installation } => (
            installation,
            vec![log_record(
                observed.clone(),
                &observed,
                format!(r#"{{"schema":"{SCHEMA}","test":true}}"#),
                vec![string_attribute("event.name", "omg.audit.test")],
            )],
        ),
    };
    let count = records.len();
    let body = json!({"resourceLogs": [{
        "resource": {"attributes": [
            string_attribute("service.name", APP_NAME),
            string_attribute("omg.installation_id", &installation.to_string()),
        ]},
        "scopeLogs": [{
            "scope": {"name": OTLP_SCOPE, "version": SCHEMA},
            "logRecords": records,
        }],
    }]});
    (body, count)
}

impl Syslog {
    /// One connection for the whole batch, all within [`REQUEST_TIMEOUT`].
    async fn send(&self, frames: &[u8]) -> Result<(), &'static str> {
        tokio::time::timeout(REQUEST_TIMEOUT, self.session(frames))
            .await
            .unwrap_or(Err("timeout"))
    }

    async fn session(&self, frames: &[u8]) -> Result<(), &'static str> {
        // The egress policy picks the addresses (#33); TLS still verifies `host`.
        let addresses = crate::egress::resolve(
            &self.policy,
            crate::egress::Purpose::Audit,
            &self.host,
            self.port,
        )
        .await
        .map_err(|_| "connection")?;
        let mut tcp = None;
        // Each address gets its own short connect limit, so one that never
        // answers (a dead IPv6 route) doesn't use up the whole run.
        for address in addresses {
            if let Ok(Ok(stream)) =
                tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(address)).await
            {
                tcp = Some(stream);
                break;
            }
        }
        let tcp = tcp.ok_or("connection")?;
        let mut tls = tokio_rustls::TlsConnector::from(self.tls.clone())
            .connect(self.server_name.clone(), tcp)
            .await
            .map_err(|_| "tls")?;
        tls.write_all(frames).await.map_err(io_category)?;
        tls.flush().await.map_err(io_category)?;
        // close_notify, then the end of our side of the stream.
        tls.shutdown().await.map_err(io_category)?;
        // RFC 5425 4.4: the receiver answers close_notify with its own or
        // closes. Waiting for that turns a receiver that failed mid-batch (a
        // reset) or refused our client certificate (an alert, TLS 1.3) into a
        // failure. Anything it sends is discarded.
        let mut sink = [0u8; 512];
        loop {
            match tls.read(&mut sink).await {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(error) => return Err(io_category(error)),
            }
        }
    }
}

/// A rustls error (an alert, a bad record) is `tls`; anything else on the
/// socket is `connection`.
fn io_category(error: std::io::Error) -> &'static str {
    if error
        .get_ref()
        .is_some_and(|inner| inner.is::<rustls::Error>())
    {
        "tls"
    } else {
        "connection"
    }
}

/// The RFC 5425 frames for a payload: one RFC 5424 message per event.
fn frames(payload: Payload) -> Vec<u8> {
    let at = chrono::Utc::now();
    match payload {
        Payload::Events {
            installation,
            events,
        } => events
            .into_iter()
            .flat_map(|event| {
                let id = msgid(event["action"].as_str().unwrap_or_default());
                frame(
                    at,
                    installation,
                    &id,
                    &json!({"schema": SCHEMA, "installation_id": installation, "event": event}),
                )
            })
            .collect(),
        Payload::Heartbeat {
            installation,
            next_seq,
        } => frame(
            at,
            installation,
            "heartbeat",
            &json!({"schema": SCHEMA, "installation_id": installation, "heartbeat": true, "next_seq": next_seq}),
        ),
        Payload::Test { installation } => frame(
            at,
            installation,
            "test",
            &json!({"schema": SCHEMA, "installation_id": installation, "test": true}),
        ),
    }
}

/// `<len> <109>1 <time> <installation> open-model-gateway - <MSGID> - <JSON>`:
/// RFC 5425 octet counting around an RFC 5424 message without structured
/// data or BOM.
fn frame(
    at: chrono::DateTime<chrono::Utc>,
    installation: Uuid,
    msgid: &str,
    json: &Value,
) -> Vec<u8> {
    let message = format!(
        "<{PRI}>1 {} {installation} {APP_NAME} - {msgid} - {json}",
        at.format("%Y-%m-%dT%H:%M:%S%.3fZ")
    );
    format!("{} {message}", message.len()).into_bytes()
}

/// RFC 5424 MSGID: the action, at most 32 characters, printable ASCII (33 to
/// 126) only.
fn msgid(action: &str) -> String {
    let id: String = action
        .chars()
        .take(MSGID_MAX)
        .map(|c| if ('!'..='~').contains(&c) { c } else { '_' })
        .collect();
    if id.is_empty() { "-".to_owned() } else { id }
}

/// HMAC-SHA256 (RFC 2104) over SHA-256, so no new dependency is needed.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// The `X-OMG-Signature` value: `t=<unix>,v1=<hex HMAC of "<t>.<body>">`.
pub fn sign(secret: &[u8], t: i64, body: &[u8]) -> String {
    let mut message = format!("{t}.").into_bytes();
    message.extend_from_slice(body);
    format!("t={t},v1={}", hex::encode(hmac_sha256(secret, &message)))
}

/// Apply this replica's setting at startup. `On` turns export on (and
/// records the destination host); `Off` turns it off; `Unset` changes
/// nothing. Each real transition is itself an audit event: `disabled` is
/// queued while export is still on and `enabled` once it is, so the SIEM sees
/// where a gap begins and ends. Turning it on starts the queue from now.
pub async fn sync_state(store: &Store, setting: &Setting) -> Result<(), sqlx::Error> {
    // The `audit_export.enabled`/`disabled` markers are written by triggers on
    // `audit_export_state` (0048), for any role that changes it.
    let mut tx = crate::db::begin(&store.pool).await?;
    match setting {
        Setting::Unset => {}
        Setting::On(exporter) => {
            let turned_on = sqlx::query("UPDATE audit_export_state SET enabled=true,enabled_at=now(),destination_host=$1 WHERE NOT enabled")
                .bind(exporter.host())
                .execute(&mut *tx)
                .await?
                .rows_affected()
                > 0;
            if !turned_on {
                sqlx::query("UPDATE audit_export_state SET destination_host=$1")
                    .bind(exporter.host())
                    .execute(&mut *tx)
                    .await?;
            }
        }
        Setting::Off => {
            let on: bool = sqlx::query_scalar("SELECT enabled FROM audit_export_state FOR UPDATE")
                .fetch_one(&mut *tx)
                .await?;
            if on {
                sqlx::query("UPDATE audit_export_state SET enabled=false,enabled_at=NULL,destination_host=NULL")
                    .execute(&mut *tx)
                    .await?;
            }
        }
    }
    tx.commit().await
}

/// What one delivery run did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing queued, or backing off.
    Idle,
    /// Accepted by the receiver (this many events).
    Delivered(usize),
    /// Not accepted; retried after the backoff.
    Failed(&'static str),
    /// Nothing to deliver for [`HEARTBEAT`]: a heartbeat was accepted.
    Heartbeat,
}

/// An idle export sends a heartbeat this long after its last accepted request.
pub const HEARTBEAT: Duration = Duration::from_secs(60);
/// Queue rows numbered per run.
const NUMBER_BATCH: i64 = 1000;

/// Number queued events in order, before their first delivery attempt (#174).
/// The counter row is locked only here, by the exporter; enqueueing never
/// touches it. The rows are locked before numbering (a concurrent delete
/// can't make the count and the numbers differ), and numbering starts above
/// any number already in the queue, so a number planted with the runtime
/// credential becomes a visible gap, never a collision that stalls export.
async fn number_queue(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>) -> Result<(), sqlx::Error> {
    let next: i64 = sqlx::query_scalar("SELECT greatest(s.next_seq,(SELECT coalesce(max(seq),0)+1 FROM audit_export_queue)) FROM audit_export_state s FOR UPDATE OF s")
        .fetch_one(&mut **tx)
        .await?;
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT event_id FROM audit_export_queue WHERE seq IS NULL ORDER BY event_created_at,event_id LIMIT $1 FOR UPDATE")
        .bind(NUMBER_BATCH)
        .fetch_all(&mut **tx)
        .await?;
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query("UPDATE audit_export_queue q SET seq=$1+o.k-1 FROM unnest($2::uuid[]) WITH ORDINALITY AS o(event_id,k) WHERE q.event_id=o.event_id")
        .bind(next)
        .bind(&ids)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE audit_export_state SET next_seq=$1")
        .bind(next + i64::try_from(ids.len()).unwrap_or(i64::MAX))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

// #37 PR2: additive `admin_token_id` (nullable; NULL for every browser-
// session event, exactly as before). Only the token's own id -- never a
// digest, plaintext or anything else from `admin_api_tokens` -- so this
// stays schema `omg.audit.v1`, additive (docs/operations.md).
const EVENT: &str = "jsonb_build_object('id',a.id,'occurred_at',a.created_at,'action',a.action,'resource',jsonb_build_object('type',a.resource_type,'id',a.resource_id),'actor',CASE WHEN a.actor_user_id IS NULL THEN NULL ELSE jsonb_build_object('id',a.actor_user_id,'label',(SELECT coalesce(nullif(btrim(u.display_name),''),u.email,'Retained identity') FROM users u WHERE u.id=a.actor_user_id)) END,'workspace',CASE WHEN w.id IS NULL THEN NULL ELSE jsonb_build_object('id',w.id,'name',w.name,'kind',w.kind) END,'admin_token_id',a.admin_token_id,'metadata',a.metadata)";

fn backoff(attempts: i32) -> f64 {
    2f64.powi(attempts.clamp(0, 16))
        .min(MAX_BACKOFF.as_secs_f64())
}

/// Deliver the oldest queued events once (the lease holder's loop calls it).
pub async fn deliver_once(
    store: &Store,
    exporter: &Exporter,
    fence: Option<&crate::leases::Fence>,
) -> Result<Outcome, sqlx::Error> {
    // 1. Number new events, then read the batch (in sequence order) unless
    //    backing off.
    let mut tx = crate::db::begin(&store.pool).await?;
    crate::leases::fence(&mut tx, fence).await?;
    number_queue(&mut tx).await?;
    let (due, attempts, installation, quiet, next_seq): (bool, i32, Uuid, bool, i64) = sqlx::query_as("SELECT s.next_attempt_at<=now(),s.attempts,(SELECT id FROM installation),s.enabled AND (s.last_success_at IS NULL OR s.last_success_at<=now()-make_interval(secs=>$1)),s.next_seq FROM audit_export_state s")
        .bind(HEARTBEAT.as_secs_f64())
        .fetch_one(&mut *tx)
        .await?;
    if !due {
        tx.commit().await?;
        return Ok(Outcome::Idle);
    }
    let rows: Vec<(Uuid, i64, Option<Value>)> = sqlx::query_as(&format!("SELECT q.event_id,q.seq,CASE WHEN a.id IS NULL OR NOT (a.workspace_id IS NULL OR w.kind IN ('team','project')) THEN NULL ELSE {EVENT} END FROM audit_export_queue q LEFT JOIN audit_events a ON a.id=q.event_id AND a.created_at=q.event_created_at LEFT JOIN workspaces w ON w.id=a.workspace_id WHERE q.seq IS NOT NULL ORDER BY q.seq LIMIT $1"))
        .bind(BATCH)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    let heartbeat = rows.is_empty();
    if heartbeat && !quiet {
        return Ok(Outcome::Idle);
    }
    let ids: Vec<Uuid> = rows.iter().map(|(id, _, _)| *id).collect();
    let events: Vec<Value> = rows
        .into_iter()
        .filter_map(|(_, seq, event)| {
            event.map(|mut event| {
                event["seq"] = json!(seq);
                event
            })
        })
        .collect();
    let missing = ids.len() - events.len();
    if missing > 0 {
        // A partition archived before its events were exported, or a row that
        // isn't platform-visible (re-checked above): counted, logged, skipped;
        // its number stays a gap the receiver sees.
        crate::metrics::METRICS.audit_export_skipped(missing as u64);
        tracing::warn!(
            missing,
            "queued audit events no longer in the database; skipped"
        );
    }
    // 2. Deliver, outside any transaction. An idle export sends a heartbeat
    //    instead: every number below `next_seq` has been delivered or skipped.
    let sent = events.len();
    let result = if heartbeat {
        exporter
            .send(Payload::Heartbeat {
                installation,
                next_seq,
            })
            .await
    } else if events.is_empty() {
        Ok(Delivery::default())
    } else {
        exporter
            .send(Payload::Events {
                installation,
                events,
            })
            .await
    };
    // 3. Record the outcome, fenced: a holder whose term ended commits nothing.
    let mut tx = crate::db::begin(&store.pool).await?;
    crate::leases::fence(&mut tx, fence).await?;
    // Records an OTLP collector refused for good (#108) are delivered as far
    // as the queue goes (never retried), but recorded as the last error.
    let rejected = result.map_or(0, |delivery| delivery.rejected);
    let error = (rejected > 0).then_some("otlp_rejected");
    // A refused heartbeat didn't reach the receiver: a failure, backed off.
    let result = match result {
        Ok(_) if heartbeat && rejected > 0 => Err("otlp_rejected"),
        other => other,
    };
    let all_rejected = !heartbeat && rejected > 0 && rejected >= u64::try_from(sent).unwrap_or(0);
    let outcome = match result {
        Ok(_) if heartbeat => {
            sqlx::query("UPDATE audit_export_state SET attempts=0,next_attempt_at='-infinity',last_success_at=now(),last_heartbeat_at=now(),last_error=$1,last_error_at=CASE WHEN $1::text IS NULL THEN last_error_at ELSE now() END")
                .bind(error)
                .execute(&mut *tx)
                .await?;
            Outcome::Heartbeat
        }
        Ok(_) => {
            sqlx::query("DELETE FROM audit_export_queue WHERE event_id=ANY($1)")
                .bind(&ids)
                .execute(&mut *tx)
                .await?;
            let accepted = u64::try_from(sent)
                .unwrap_or(u64::MAX)
                .saturating_sub(rejected);
            // A batch the collector refused entirely isn't a success: the
            // last delivery time (and so the backlog and silence alerts) stays.
            sqlx::query("UPDATE audit_export_state SET attempts=0,next_attempt_at='-infinity',last_success_at=CASE WHEN $3 THEN last_success_at ELSE now() END,last_error=$2,last_error_at=CASE WHEN $2::text IS NULL THEN last_error_at ELSE now() END,delivered_total=delivered_total+$1")
                .bind(i64::try_from(accepted).unwrap_or(i64::MAX))
                .bind(error)
                .bind(all_rejected)
                .execute(&mut *tx)
                .await?;
            Outcome::Delivered(sent)
        }
        Err(category) => {
            sqlx::query("UPDATE audit_export_state SET attempts=attempts+1,next_attempt_at=now()+make_interval(secs=>$1),last_error=$2,last_error_at=now()")
                .bind(backoff(attempts))
                .bind(category)
                .execute(&mut *tx)
                .await?;
            Outcome::Failed(category)
        }
    };
    tx.commit().await?;
    if rejected > 0 && !heartbeat {
        crate::metrics::METRICS.audit_export_rejected(rejected);
        tracing::warn!(
            rejected,
            "the OTLP collector refused audit records; they are not retried"
        );
    }
    // Heartbeats aren't deliveries (a failed one counts as a failure).
    if outcome != Outcome::Heartbeat {
        crate::metrics::METRICS.audit_export_delivery(match outcome {
            Outcome::Delivered(_) => "delivered",
            _ => "failed",
        });
    }
    Ok(outcome)
}

fn status_category(status: u16) -> &'static str {
    match status {
        400..=499 => "http_4xx",
        500..=599 => "http_5xx",
        _ => "http_other",
    }
}

/// A test delivery: `"test": true` and no events (a signed webhook body, one
/// syslog message with MSGID `test`, or one OTLP record `omg.audit.test`).
/// Doesn't touch the queue or the status. A collector that refuses the test
/// record fails the test (`otlp_rejected`).
pub async fn send_test(store: &Store, exporter: &Exporter) -> Result<(), &'static str> {
    let installation: Uuid = sqlx::query_scalar("SELECT id FROM installation")
        .fetch_one(&store.pool)
        .await
        .map_err(|_| "database")?;
    match exporter.send(Payload::Test { installation }).await? {
        Delivery { rejected: 0 } => Ok(()),
        Delivery { .. } => Err("otlp_rejected"),
    }
}

/// Status for Admin › Settings › Audit export. Never the URL or secret.
pub async fn status(store: &Store) -> Result<Value, sqlx::Error> {
    let mut v: Value = sqlx::query_scalar("SELECT jsonb_build_object('enabled',s.enabled,'enabled_at',s.enabled_at,'destination_host',s.destination_host,'queued',(SELECT count(*) FROM audit_export_queue),'oldest_queued_at',(SELECT min(enqueued_at) FROM audit_export_queue),'last_success_at',s.last_success_at,'last_error',s.last_error,'last_error_at',s.last_error_at,'retrying',s.attempts>0,'next_attempt_at',CASE WHEN s.attempts>0 THEN s.next_attempt_at END,'delivered_total',s.delivered_total::text,'next_seq',s.next_seq::text,'last_heartbeat_at',s.last_heartbeat_at) FROM audit_export_state s")
        .fetch_one(&store.pool)
        .await?;
    v["schema"] = json!(SCHEMA);
    Ok(v)
}

/// The exporter loop for `serve`: only the `audit_export` lease holder
/// delivers; a full batch goes again at once, otherwise every 2 seconds.
pub fn start(store: Store, exporter: std::sync::Arc<Exporter>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            // The backlog gauges come from the delivering replica only.
            if store
                .leases()
                .is_none_or(|leases| leases.held(crate::leases::Lease::AuditExport).is_some())
            {
                record_backlog(&store).await;
            }
            for _ in 0..20 {
                let fence = match store.leases() {
                    Some(leases) => match leases.held(crate::leases::Lease::AuditExport) {
                        Some(fence) => Some(fence),
                        None => break,
                    },
                    None => None,
                };
                match tokio::time::timeout(
                    REQUEST_TIMEOUT * 3,
                    deliver_once(&store, &exporter, fence.as_ref()),
                )
                .await
                {
                    Ok(Ok(Outcome::Delivered(n))) if i64::try_from(n).unwrap_or(0) == BATCH => {
                        continue;
                    }
                    Ok(Ok(Outcome::Failed(category))) => {
                        tracing::warn!(
                            category,
                            host = exporter.host(),
                            transport = exporter.transport(),
                            "audit export delivery failed; retrying with backoff"
                        );
                        break;
                    }
                    Ok(Err(error)) if crate::leases::is_fenced(&error) => break,
                    Ok(Err(_)) | Err(_) => {
                        tracing::warn!("audit export run incomplete; retrying");
                        break;
                    }
                    Ok(Ok(_)) => break,
                }
            }
        }
    })
}

async fn record_backlog(store: &Store) {
    if let Ok((queued, oldest)) = sqlx::query_as::<_, (i64, Option<f64>)>("SELECT count(*),extract(epoch FROM now()-min(enqueued_at))::float8 FROM audit_export_queue")
        .fetch_one(&store.pool)
        .await
    {
        crate::metrics::METRICS.audit_export_backlog(queued, oldest.unwrap_or(0.0));
    }
}

/// Test PKI for the syslog transport (#107), generated with the `openssl` CLI
/// (P-256, valid 2026 to 2126): a CA, a receiver certificate for 127.0.0.1 and
/// localhost and one for another name (same key), a client certificate (all
/// signed by the CA), and an unrelated CA.
#[cfg(test)]
pub(crate) mod pki {
    pub(crate) const CA: &str = "-----BEGIN CERTIFICATE-----
MIIBkjCCATmgAwIBAgIUKahYf7QPSYFha6+yWuXhelwBAQUwCgYIKoZIzj0EAwIw
FjEUMBIGA1UEAwwLT01HIFRlc3QgQ0EwIBcNMjYwMTAxMDAwMDAwWhgPMjEyNjAx
MDEwMDAwMDBaMBYxFDASBgNVBAMMC09NRyBUZXN0IENBMFkwEwYHKoZIzj0CAQYI
KoZIzj0DAQcDQgAEG91hAc1RCG5918cbTDiFUzVoZdJgviLca8E0pqf6r5R9Dp1T
kz4ZVF2PBb2vRhQbOqtGIGGVQun2985wQGDw0KNjMGEwHwYDVR0jBBgwFoAUwVtb
Hi8i6H6JK8MwFXQqzVrspI8wDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMC
AQYwHQYDVR0OBBYEFMFbWx4vIuh+iSvDMBV0Ks1a7KSPMAoGCCqGSM49BAMCA0cA
MEQCIGAcCPbBblVzsp1NwOYvXCOq+9Dd4Wm31YmIQd53Bww8AiA+fZ/bRchz1t8Q
2uH+ATSczUV2iUeqx+wDpYWusexEDg==
-----END CERTIFICATE-----
";
    pub(crate) const OTHER_CA: &str = "-----BEGIN CERTIFICATE-----
MIIBpjCCAU2gAwIBAgIUYpUDpfHgyL62mSSc6ydAac++jfwwCgYIKoZIzj0EAwIw
IDEeMBwGA1UEAwwVT01HIFVucmVsYXRlZCBUZXN0IENBMCAXDTI2MDEwMTAwMDAw
MFoYDzIxMjYwMTAxMDAwMDAwWjAgMR4wHAYDVQQDDBVPTUcgVW5yZWxhdGVkIFRl
c3QgQ0EwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAARBEMpFFwk+kjwdvG7mUc5r
hVRKbmwqQlm9uFAeSjG6jMhzJzUKQJG47JtfYkRGHYVwzdXz5d7PF5451B8tptuG
o2MwYTAfBgNVHSMEGDAWgBSgb9BJqkH01pXp1We9UIEmvj53QTAPBgNVHRMBAf8E
BTADAQH/MA4GA1UdDwEB/wQEAwIBBjAdBgNVHQ4EFgQUoG/QSapB9NaV6dVnvVCB
Jr4+d0EwCgYIKoZIzj0EAwIDRwAwRAIgXe664gHxHXPaw4UkLKt5EUY9j1eQw0c6
HKhjYeg7+SACIHwuA9H9xeQKzPdkExqAonuKAeFT7He4iJ/+UzFRG3Tf
-----END CERTIFICATE-----
";
    pub(crate) const SERVER_CERT: &str = "-----BEGIN CERTIFICATE-----
MIIByTCCAW+gAwIBAgIURx9n10MhilUEloMJ/O4DKBWnQQkwCgYIKoZIzj0EAwIw
FjEUMBIGA1UEAwwLT01HIFRlc3QgQ0EwIBcNMjYwMTAxMDAwMDAwWhgPMjEyNjAx
MDEwMDAwMDBaMBwxGjAYBgNVBAMMEW9tZy10ZXN0LXJlY2VpdmVyMFkwEwYHKoZI
zj0CAQYIKoZIzj0DAQcDQgAEolRPOpACXzYb+/YEDArshWuygf8G7RmxToatT037
/5tNv5Se3hHcP2ujRwLiIzoAisPZXURyY3OojJIeWbrPtqOBkjCBjzAMBgNVHRMB
Af8EAjAAMA4GA1UdDwEB/wQEAwIHgDATBgNVHSUEDDAKBggrBgEFBQcDATAfBgNV
HSMEGDAWgBTBW1seLyLofokrwzAVdCrNWuykjzAaBgNVHREEEzARhwR/AAABggls
b2NhbGhvc3QwHQYDVR0OBBYEFHB8pgvbZdfrLdHF7CsxAZY9ZRUOMAoGCCqGSM49
BAMCA0gAMEUCIG1g+Tngl4j9YNByir09OdQ1i+A8o5bTIsWFfQp8Bn1WAiEAxvNQ
ISAhBc4rf7HIuml6NoGmuZ/wA8y+5rAFbcR61nM=
-----END CERTIFICATE-----
";
    /// Signed by [`CA`] for `siem.example.test` only: a name mismatch on 127.0.0.1.
    pub(crate) const WRONG_NAME_CERT: &str = "-----BEGIN CERTIFICATE-----
MIIByzCCAXGgAwIBAgIURx9n10MhilUEloMJ/O4DKBWnQQowCgYIKoZIzj0EAwIw
FjEUMBIGA1UEAwwLT01HIFRlc3QgQ0EwIBcNMjYwMTAxMDAwMDAwWhgPMjEyNjAx
MDEwMDAwMDBaMBwxGjAYBgNVBAMMEXNpZW0uZXhhbXBsZS50ZXN0MFkwEwYHKoZI
zj0CAQYIKoZIzj0DAQcDQgAEolRPOpACXzYb+/YEDArshWuygf8G7RmxToatT037
/5tNv5Se3hHcP2ujRwLiIzoAisPZXURyY3OojJIeWbrPtqOBlDCBkTAMBgNVHRMB
Af8EAjAAMA4GA1UdDwEB/wQEAwIHgDATBgNVHSUEDDAKBggrBgEFBQcDATAfBgNV
HSMEGDAWgBTBW1seLyLofokrwzAVdCrNWuykjzAcBgNVHREEFTATghFzaWVtLmV4
YW1wbGUudGVzdDAdBgNVHQ4EFgQUcHymC9tl1+st0cXsKzEBlj1lFQ4wCgYIKoZI
zj0EAwIDSAAwRQIhAPRFQ2NvkN0cLpP+XgwqVAB2te6Pyf1TjVkYxjEmo5sFAiAi
rSyeJxZiykjKEO8laqUBIIrRvrssu5Ma/6h9HU/DSw==
-----END CERTIFICATE-----
";
    pub(crate) const SERVER_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgNM/I9lDtGbEloYt9
3ysQIi9VLLr5aUIHS3WiNMGDC/GhRANCAASiVE86kAJfNhv79gQMCuyFa7KB/wbt
GbFOhq1PTfv/m02/lJ7eEdw/a6NHAuIjOgCKw9ldRHJjc6iMkh5Zus+2
-----END PRIVATE KEY-----
";
    pub(crate) const CLIENT_CERT: &str = "-----BEGIN CERTIFICATE-----
MIIBqzCCAVCgAwIBAgIURx9n10MhilUEloMJ/O4DKBWnQQswCgYIKoZIzj0EAwIw
FjEUMBIGA1UEAwwLT01HIFRlc3QgQ0EwIBcNMjYwMTAxMDAwMDAwWhgPMjEyNjAx
MDEwMDAwMDBaMBsxGTAXBgNVBAMMEG9tZy10ZXN0LWdhdGV3YXkwWTATBgcqhkjO
PQIBBggqhkjOPQMBBwNCAAQaZK7/hkSw3WAqlb2FAsjOSmj64iKaPQRhI+zyaFt0
RDFdpYeRxeNUjnyCkqAfcm7oPDyqLNrA5LrQoU1mak4Vo3UwczAMBgNVHRMBAf8E
AjAAMA4GA1UdDwEB/wQEAwIHgDATBgNVHSUEDDAKBggrBgEFBQcDAjAfBgNVHSME
GDAWgBTBW1seLyLofokrwzAVdCrNWuykjzAdBgNVHQ4EFgQUg0QvXAn0C7kAy4tX
Kb5qA4EVU7gwCgYIKoZIzj0EAwIDSQAwRgIhAMexI2/Y6lc+K07HHYPRlB4hj6TD
yi0x58TJ4NJ0BF69AiEA2Q7WZmmUR28o9oeAzAbaTL+MSCsy2Oh+gMiOBglncX4=
-----END CERTIFICATE-----
";
    pub(crate) const CLIENT_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgAxk2rqE1tw7NMsZd
8dO9d4Hs6FWuIakUar1baq6OH0OhRANCAAQaZK7/hkSw3WAqlb2FAsjOSmj64iKa
PQRhI+zyaFt0RDFdpYeRxeNUjnyCkqAfcm7oPDyqLNrA5LrQoU1mak4V
-----END PRIVATE KEY-----
";
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(pairs: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
        }
    }

    /// #107: syslog over TLS is the one alternative destination, TLS only,
    /// with its trust and client identity read from files at startup.
    #[test]
    fn syslog_configuration_is_one_destination_tls_only_with_files_read_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str, contents: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, contents).unwrap();
            path.to_string_lossy().into_owned()
        };
        let ca = file("ca.pem", pki::CA);
        let cert = file("client.pem", pki::CLIENT_CERT);
        let key = file("client.key", pki::CLIENT_KEY);
        // Contents that must never appear in an error message.
        const MARK: &str = "Zm9vYmFyU0VDUkVUTUFSSw";
        let bad = file(
            "bad.pem",
            &format!("-----BEGIN CERTIFICATE-----\n{MARK}\n-----END CERTIFICATE-----\n"),
        );
        let junk = file("junk.pem", &format!("{MARK} not a PEM file"));
        let bad_key = file(
            "bad.key",
            &format!("-----BEGIN PRIVATE KEY-----\n{MARK}\n-----END PRIVATE KEY-----\n"),
        );
        let missing = dir
            .path()
            .join("missing.pem")
            .to_string_lossy()
            .into_owned();
        let syslog = |url: &str| (SYSLOG_URL_ENV, url.to_owned());

        // A destination, default port 6514, the public roots.
        let exp =
            Setting::from_lookup(lookup(vec![syslog("syslog+tls://SIEM.example.org")]), false)
                .unwrap()
                .exporter()
                .unwrap();
        assert_eq!(
            (exp.transport(), exp.host()),
            ("syslog", "siem.example.org")
        );
        let Transport::Syslog(s) = &exp.transport else {
            panic!("syslog")
        };
        assert_eq!(s.port, 6514);
        // A private CA and a client identity, read from files.
        let exp = Setting::from_lookup(
            lookup(vec![
                syslog("syslog+tls://127.0.0.1:1514"),
                (SYSLOG_CA_FILE_ENV, ca.clone()),
                (SYSLOG_CLIENT_CERT_FILE_ENV, cert.clone()),
                (SYSLOG_CLIENT_KEY_FILE_ENV, key.clone()),
            ]),
            true,
        )
        .unwrap()
        .exporter()
        .unwrap();
        let Transport::Syslog(s) = &exp.transport else {
            panic!("syslog")
        };
        assert_eq!((exp.host(), s.port), ("127.0.0.1", 1514));
        let debug = format!("{exp:?}");
        assert!(debug.contains("syslog") && debug.contains("127.0.0.1"));
        for secret in [
            &pki::CLIENT_KEY[28..60],
            &pki::CLIENT_CERT[28..60],
            &pki::CA[28..60],
            "PRIVATE",
        ] {
            assert!(!debug.contains(secret), "no key material in Debug output");
        }
        // The webhook's Debug says which transport it is, still without the secret.
        let webhook = Exporter::new("https://siem.example.org/x", vec![b's'; 32], false)
            .unwrap()
            .unwrap();
        assert_eq!(webhook.transport(), "webhook");
        assert!(format!("{webhook:?}").contains("webhook"));

        // Only an explicit `off`, in either variable, turns export off.
        for pairs in [
            vec![syslog("off")],
            vec![syslog(" off "), (URL_ENV, "off".into())],
        ] {
            assert!(matches!(
                Setting::from_lookup(lookup(pairs), false).unwrap(),
                Setting::Off
            ));
        }
        let webhook_dest = || {
            vec![
                (URL_ENV, "https://siem.example.org".to_owned()),
                (SECRET_ENV_ENV, "AUDIT_SECRET".to_owned()),
                ("AUDIT_SECRET", "s".repeat(32)),
            ]
        };
        let with = |mut pairs: Vec<(&'static str, String)>, extra: Vec<(&'static str, String)>| {
            pairs.extend(extra);
            pairs
        };
        for (pairs, why) in [
            (
                with(
                    webhook_dest(),
                    vec![syslog("syslog+tls://siem.example.org")],
                ),
                "both destinations",
            ),
            (
                vec![
                    (URL_ENV, "off".into()),
                    syslog("syslog+tls://siem.example.org"),
                ],
                "off plus syslog",
            ),
            (
                with(webhook_dest(), vec![syslog("off")]),
                "webhook plus off",
            ),
            (
                vec![
                    syslog("syslog+tls://siem.example.org"),
                    (SECRET_ENV_ENV, "AUDIT_SECRET".into()),
                    ("AUDIT_SECRET", "s".repeat(32)),
                ],
                "the webhook secret with only syslog",
            ),
            (
                with(webhook_dest(), vec![(SYSLOG_CA_FILE_ENV, ca.clone())]),
                "syslog files with the webhook",
            ),
            (
                vec![(SYSLOG_CA_FILE_ENV, ca.clone())],
                "syslog files without a URL",
            ),
            (
                vec![syslog("syslog://siem.example.org")],
                "plaintext syslog",
            ),
            (vec![syslog("tcp://siem.example.org:514")], "tcp"),
            (vec![syslog("udp://siem.example.org:514")], "udp"),
            (vec![syslog("syslog+tls://siem.example.org/path")], "a path"),
            (
                vec![syslog("syslog+tls://user:pw@siem.example.org")],
                "credentials",
            ),
            (vec![syslog("syslog+tls://siem.example.org:0")], "port 0"),
            (
                vec![syslog("syslog+tls://siem.example.org:99999")],
                "port too large",
            ),
            (vec![syslog("syslog+tls://")], "no host"),
            (
                vec![
                    syslog("syslog+tls://siem.example.org"),
                    (SYSLOG_CA_FILE_ENV, missing.clone()),
                ],
                "missing CA file",
            ),
            (
                vec![
                    syslog("syslog+tls://siem.example.org"),
                    (SYSLOG_CA_FILE_ENV, bad.clone()),
                ],
                "bad CA file",
            ),
            (
                vec![
                    syslog("syslog+tls://siem.example.org"),
                    (SYSLOG_CA_FILE_ENV, junk.clone()),
                ],
                "no certificate in the CA file",
            ),
            (
                vec![
                    syslog("syslog+tls://siem.example.org"),
                    (SYSLOG_CLIENT_CERT_FILE_ENV, cert.clone()),
                ],
                "certificate without key",
            ),
            (
                vec![
                    syslog("syslog+tls://siem.example.org"),
                    (SYSLOG_CLIENT_KEY_FILE_ENV, key.clone()),
                ],
                "key without certificate",
            ),
            (
                vec![
                    syslog("syslog+tls://siem.example.org"),
                    (SYSLOG_CLIENT_CERT_FILE_ENV, cert.clone()),
                    (SYSLOG_CLIENT_KEY_FILE_ENV, bad_key.clone()),
                ],
                "bad key file",
            ),
            (
                vec![
                    syslog("syslog+tls://siem.example.org"),
                    (SYSLOG_CLIENT_CERT_FILE_ENV, junk.clone()),
                    (SYSLOG_CLIENT_KEY_FILE_ENV, key.clone()),
                ],
                "bad certificate file",
            ),
            (
                vec![
                    syslog("syslog+tls://siem.example.org"),
                    (SYSLOG_CLIENT_CERT_FILE_ENV, cert.clone()),
                    (SYSLOG_CLIENT_KEY_FILE_ENV, cert.clone()),
                ],
                "a certificate as the key",
            ),
        ] {
            let error = match Setting::from_lookup(lookup(pairs), false) {
                Ok(_) => panic!("{why}: accepted"),
                Err(error) => format!("{error:#}"),
            };
            assert!(!error.contains(MARK), "{why}: file contents in the error");
            assert!(
                !error.contains(&pki::CLIENT_KEY[28..60]),
                "{why}: key in the error"
            );
            assert!(
                !error.contains(&pki::CLIENT_CERT[28..60]),
                "{why}: certificate in the error"
            );
        }
    }

    /// RFC 5424 MSGID: at most 32 printable ASCII characters (33 to 126).
    #[test]
    fn syslog_message_ids_and_frames_follow_rfc_5424_and_5425() {
        assert_eq!(msgid("key.revoked"), "key.revoked");
        assert_eq!(msgid("a b\tc\u{e9}d"), "a_b_c_d");
        assert_eq!(msgid(&"x".repeat(40)).len(), 32);
        assert_eq!(msgid(""), "-");
        let at = chrono::DateTime::parse_from_rfc3339("2026-10-14T01:02:03.456789Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let installation = Uuid::nil();
        let frame = frame(at, installation, "test", &json!({"a":"\u{e9}"}));
        let text = String::from_utf8(frame).unwrap();
        let message = format!(
            "<109>1 2026-10-14T01:02:03.456Z {installation} open-model-gateway - test - {{\"a\":\"\u{e9}\"}}"
        );
        // The octet count is bytes, not characters.
        assert_eq!(text, format!("{} {message}", message.len()));
        assert_ne!(message.len(), message.chars().count());
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        // Test cases 1, 2 and 6 (a key longer than the block).
        assert_eq!(
            hex::encode(hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex::encode(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex::encode(hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
        assert_eq!(
            sign(b"secret", 1_700_000_000, b"{}"),
            format!(
                "t=1700000000,v1={}",
                hex::encode(hmac_sha256(b"secret", b"1700000000.{}"))
            )
        );
    }

    #[test]
    fn configuration_is_operator_only_and_strict() {
        let secret = "s".repeat(32);
        let env = |pairs: Vec<(&'static str, String)>| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.clone())
            }
        };
        assert!(
            matches!(
                Setting::from_lookup(env(vec![]), false).unwrap(),
                Setting::Unset
            ),
            "unset: no export"
        );
        let ok = Setting::from_lookup(
            env(vec![
                (URL_ENV, "https://siem.example.org/ingest?token=x".into()),
                (SECRET_ENV_ENV, "AUDIT_SECRET".into()),
                ("AUDIT_SECRET", secret.clone()),
            ]),
            false,
        )
        .unwrap()
        .exporter()
        .unwrap();
        assert_eq!(ok.host(), "siem.example.org");
        assert!(
            !format!("{ok:?}").contains(&secret),
            "the secret is never printed"
        );
        for (pairs, why) in [
            (
                vec![(URL_ENV, "https://siem.example.org".into())],
                "no secret",
            ),
            (
                vec![(SECRET_ENV_ENV, "AUDIT_SECRET".into())],
                "secret without URL",
            ),
            (
                vec![
                    (URL_ENV, "https://siem.example.org".into()),
                    (SECRET_ENV_ENV, "AUDIT_SECRET".into()),
                ],
                "secret variable unset",
            ),
            (
                vec![
                    (URL_ENV, "https://siem.example.org".into()),
                    (SECRET_ENV_ENV, "AUDIT_SECRET".into()),
                    ("AUDIT_SECRET", "short".into()),
                ],
                "short secret",
            ),
            (
                vec![
                    (URL_ENV, "http://siem.example.org".into()),
                    (SECRET_ENV_ENV, "AUDIT_SECRET".into()),
                    ("AUDIT_SECRET", secret.clone()),
                ],
                "plain http",
            ),
            (
                vec![
                    (URL_ENV, "https://user:pw@siem.example.org".into()),
                    (SECRET_ENV_ENV, "AUDIT_SECRET".into()),
                    ("AUDIT_SECRET", secret.clone()),
                ],
                "credentials",
            ),
            (
                vec![
                    (URL_ENV, "https://169.254.169.254/".into()),
                    (SECRET_ENV_ENV, "AUDIT_SECRET".into()),
                    ("AUDIT_SECRET", secret.clone()),
                ],
                "metadata address",
            ),
            (
                vec![
                    (URL_ENV, "https://siem.example.org".into()),
                    (SECRET_ENV_ENV, "lower".into()),
                    ("lower", secret.clone()),
                ],
                "bad variable name",
            ),
        ] {
            assert!(Setting::from_lookup(env(pairs), false).is_err(), "{why}");
        }
        assert!(
            Exporter::new("http://127.0.0.1:9/", secret.into_bytes(), true)
                .unwrap()
                .is_some(),
            "development loopback"
        );

        // #106 review: the signing secret's variable can't be on an allowlist
        // a Platform Admin can point somewhere, nor a reserved name.
        let base = |name: &'static str, extra: Vec<(&'static str, String)>| {
            let mut pairs = vec![
                (URL_ENV, "https://siem.example.org".to_owned()),
                (SECRET_ENV_ENV, name.to_owned()),
                (name, "s".repeat(32)),
            ];
            pairs.extend(extra);
            pairs
        };
        for (pairs, why) in [
            (
                base(
                    "AUDIT_SECRET",
                    vec![(
                        "GATEWAY_SECRET_ENV_ALLOWLIST",
                        "OPENAI_API_KEY, AUDIT_SECRET".into(),
                    )],
                ),
                "provider allowlist",
            ),
            (
                base(
                    "AUDIT_SECRET",
                    vec![(crate::email::SMTP_ALLOWLIST_VAR, "AUDIT_SECRET".into())],
                ),
                "SMTP allowlist",
            ),
            (base("OPENAI_API_KEY", vec![]), "reserved key"),
            (base("GATEWAY_SCIM_TOKEN", vec![]), "GATEWAY_ prefix"),
            (base("AWS_SECRET_ACCESS_KEY", vec![]), "AWS_ prefix"),
        ] {
            let error = Setting::from_lookup(env(pairs), false)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(error.contains(SECRET_ENV_ENV), "{why}: {error}");
        }
        assert!(
            matches!(
                Setting::from_lookup(env(vec![(URL_ENV, " off ".into())]), false).unwrap(),
                Setting::Off
            ),
            "explicit off"
        );
    }

    #[test]
    fn backoff_doubles_up_to_five_minutes() {
        assert_eq!(backoff(0), 1.0);
        assert_eq!(backoff(3), 8.0);
        assert_eq!(backoff(8), 256.0);
        assert_eq!(backoff(9), 300.0);
        assert_eq!(backoff(1000), 300.0);
    }

    /// #108: OTLP/HTTP is the third destination, one at a time. Its headers
    /// come from a variable named under the webhook secret's rules, are
    /// checked at startup and never printed; `OTEL_*` is never read.
    #[test]
    fn otlp_configuration_is_one_destination_with_checked_headers_and_no_otel_variables() {
        // A header value that must never appear in Debug output or an error.
        const TOKEN: &str = "c2VjcmV0LWNvbGxlY3Rvci10b2tlbg";
        let otlp = |url: &str| (OTLP_URL_ENV, url.to_owned());
        let headers = |value: &str| {
            vec![
                (OTLP_HEADERS_ENV_ENV, "AUDIT_OTLP_HEADERS".to_owned()),
                ("AUDIT_OTLP_HEADERS", value.to_owned()),
            ]
        };
        let with = |mut pairs: Vec<(&'static str, String)>, extra: Vec<(&'static str, String)>| {
            pairs.extend(extra);
            pairs
        };
        let otel = || {
            vec![
                (
                    "OTEL_EXPORTER_OTLP_ENDPOINT",
                    "https://telemetry.example.org".to_owned(),
                ),
                (
                    "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
                    "https://telemetry.example.org/v1/logs".to_owned(),
                ),
                ("OTEL_EXPORTER_OTLP_HEADERS", "x-telemetry=1".to_owned()),
            ]
        };

        // A destination with headers; the operational OTEL_* settings are
        // never even looked up.
        let read = std::cell::RefCell::new(Vec::<String>::new());
        let get = lookup(with(
            with(
                vec![otlp("https://Collector.example.org:4318/v1/logs")],
                headers(&format!(
                    " Authorization = Bearer {TOKEN} ,X-Scope-OrgID=audit"
                )),
            ),
            otel(),
        ));
        let exp = Setting::from_lookup(
            |name| {
                read.borrow_mut().push(name.to_owned());
                get(name)
            },
            false,
        )
        .unwrap()
        .exporter()
        .unwrap();
        assert_eq!(
            (exp.transport(), exp.host()),
            ("otlp", "collector.example.org")
        );
        let Transport::Otlp(o) = &exp.transport else {
            panic!("otlp")
        };
        assert_eq!(o.url.as_str(), "https://collector.example.org:4318/v1/logs");
        assert_eq!(o.headers.len(), 2);
        assert_eq!(
            o.headers["authorization"],
            format!("Bearer {TOKEN}").as_str()
        );
        assert_eq!(o.headers["x-scope-orgid"], "audit");
        assert!(o.headers.values().all(|v| v.is_sensitive()));
        let debug = format!("{exp:?}");
        assert!(debug.contains("otlp"));
        assert!(
            !debug.contains(TOKEN) && !debug.contains("audit\"") && !debug.contains("Bearer"),
            "no header value in Debug output"
        );
        assert!(
            read.borrow().iter().all(|name| !name.starts_with("OTEL_")),
            "{:?}",
            read.borrow()
        );
        // OTEL_* alone is no audit destination.
        assert!(matches!(
            Setting::from_lookup(lookup(otel()), false).unwrap(),
            Setting::Unset
        ));
        // Headers are optional; plain HTTP only to loopback in development.
        let exp = Setting::from_lookup(lookup(vec![otlp("http://127.0.0.1:4318/v1/logs")]), true)
            .unwrap()
            .exporter()
            .unwrap();
        let Transport::Otlp(o) = &exp.transport else {
            panic!("otlp")
        };
        assert!(o.headers.is_empty());

        // Only an explicit `off` turns export off, in any of the three.
        for pairs in [
            vec![otlp("off")],
            vec![
                otlp(" off "),
                (URL_ENV, "off".into()),
                (SYSLOG_URL_ENV, "off".into()),
            ],
        ] {
            assert!(matches!(
                Setting::from_lookup(lookup(pairs), false).unwrap(),
                Setting::Off
            ));
        }
        // `off` with a leftover transport setting is refused: it would do nothing.
        for extra in [
            headers("X-Token=1"),
            vec![(SECRET_ENV_ENV, "AUDIT_SECRET".to_owned())],
            vec![(SYSLOG_CA_FILE_ENV, "/etc/ca.pem".to_owned())],
        ] {
            assert!(Setting::from_lookup(lookup(with(vec![otlp("off")], extra)), false).is_err());
        }
        let collector = || otlp("https://collector.example.org/v1/logs");
        let webhook = || {
            vec![
                (URL_ENV, "https://siem.example.org".to_owned()),
                (SECRET_ENV_ENV, "AUDIT_SECRET".to_owned()),
                ("AUDIT_SECRET", "s".repeat(32)),
            ]
        };
        let syslog = || (SYSLOG_URL_ENV, "syslog+tls://siem.example.org".to_owned());
        let dir = tempfile::tempdir().unwrap();
        let ca = dir.path().join("ca.pem");
        std::fs::write(&ca, pki::CA).unwrap();
        let ca = (SYSLOG_CA_FILE_ENV, ca.to_string_lossy().into_owned());
        let good = || headers(&format!("Authorization=Bearer {TOKEN}"));
        let too_many = (0..17)
            .map(|i| format!("X-H{i}={TOKEN}"))
            .collect::<Vec<_>>()
            .join(",");
        for (pairs, why, header_error) in [
            (
                with(webhook(), vec![collector()]),
                "webhook and OTLP",
                false,
            ),
            (vec![syslog(), collector()], "syslog and OTLP", false),
            (
                with(webhook(), vec![syslog(), collector()]),
                "all three",
                false,
            ),
            (
                vec![(URL_ENV, "off".into()), collector()],
                "webhook off plus OTLP",
                false,
            ),
            (
                vec![collector(), (SYSLOG_URL_ENV, "off".into())],
                "OTLP plus syslog off",
                false,
            ),
            (
                with(webhook(), vec![otlp("off")]),
                "webhook plus OTLP off",
                false,
            ),
            (
                with(
                    vec![collector()],
                    vec![
                        (SECRET_ENV_ENV, "AUDIT_SECRET".into()),
                        ("AUDIT_SECRET", "s".repeat(32)),
                    ],
                ),
                "the webhook secret with OTLP",
                false,
            ),
            (
                vec![collector(), ca.clone()],
                "syslog files with OTLP",
                false,
            ),
            (good(), "headers without the OTLP URL", true),
            (with(webhook(), good()), "headers with the webhook", true),
            (with(vec![syslog()], good()), "headers with syslog", true),
            (
                vec![otlp("http://collector.example.org/v1/logs")],
                "plain http",
                false,
            ),
            (
                vec![otlp("http://127.0.0.1:4318/v1/logs")],
                "plain http to loopback outside development",
                false,
            ),
            (
                vec![otlp("https://user:pw@collector.example.org/v1/logs")],
                "credentials",
                false,
            ),
            (
                vec![otlp("https://collector.example.org/v1/logs#x")],
                "a fragment",
                false,
            ),
            (
                vec![otlp("https://169.254.169.254/v1/logs")],
                "metadata address",
                false,
            ),
            (vec![otlp("collector:4318")], "not a URL", false),
            (
                vec![
                    collector(),
                    (OTLP_HEADERS_ENV_ENV, "AUDIT_OTLP_HEADERS".into()),
                ],
                "the headers variable unset",
                true,
            ),
            (
                with(vec![collector()], headers("")),
                "no headers in the variable",
                true,
            ),
            (
                with(
                    vec![collector()],
                    headers(&format!("Authorization {TOKEN}")),
                ),
                "not Key=Value",
                true,
            ),
            (
                with(vec![collector()], headers(&format!("X Token={TOKEN}"))),
                "a space in the name",
                true,
            ),
            (
                with(vec![collector()], headers(&format!("X-Token:={TOKEN}"))),
                "a colon in the name",
                true,
            ),
            (
                with(vec![collector()], headers(&format!("={TOKEN}"))),
                "an empty name",
                true,
            ),
            (
                with(vec![collector()], headers("X-Token=")),
                "an empty value",
                true,
            ),
            (
                with(
                    vec![collector()],
                    headers(&format!("X-Token={TOKEN}\r\nX-Injected: 1")),
                ),
                "CR/LF in a value",
                true,
            ),
            (
                with(vec![collector()], headers(&format!("X-Token={TOKEN}\n"))),
                "LF in a value",
                true,
            ),
            (
                with(
                    vec![collector()],
                    headers(&format!("X-Token={TOKEN}\u{e9}")),
                ),
                "a non-ASCII value",
                true,
            ),
            (
                with(vec![collector()], headers(&too_many)),
                "more than 16 headers",
                true,
            ),
            (
                with(
                    vec![collector()],
                    headers(&format!("Content-Type=text/plain,X-Token={TOKEN}")),
                ),
                "content-type",
                true,
            ),
            (
                with(vec![collector()], headers(&format!("HOST={TOKEN}"))),
                "host",
                true,
            ),
            (
                with(
                    vec![collector()],
                    headers(&format!("Transfer-Encoding=chunked,X-Token={TOKEN}")),
                ),
                "transfer-encoding",
                true,
            ),
            (
                with(
                    vec![collector()],
                    headers(&format!("Connection=close,X-Token={TOKEN}")),
                ),
                "connection",
                true,
            ),
            (
                with(
                    vec![collector()],
                    headers(&format!("content-length=1,X-Token={TOKEN}")),
                ),
                "content-length",
                true,
            ),
        ] {
            let error = match Setting::from_lookup(lookup(pairs), false) {
                Ok(_) => panic!("{why}: accepted"),
                Err(error) => format!("{error:#} {error:?}"),
            };
            assert!(!error.contains(TOKEN), "{why}: header value in the error");
            if header_error {
                assert!(
                    error.contains(OTLP_HEADERS_ENV_ENV),
                    "{why}: names the setting"
                );
            }
        }
        // Exactly 16 headers are fine.
        let sixteen = (0..16)
            .map(|i| format!("X-H{i}={TOKEN}"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(
            Setting::from_lookup(lookup(with(vec![collector()], headers(&sixteen))), false).is_ok()
        );

        // The headers variable follows the webhook secret's naming rules
        // (one shared check): a Platform Admin must not be able to read it
        // through a provider connection or the SMTP relay.
        let named = |name: &'static str, extra: Vec<(&'static str, String)>| {
            with(
                vec![
                    collector(),
                    (OTLP_HEADERS_ENV_ENV, name.to_owned()),
                    (name, format!("Authorization=Bearer {TOKEN}")),
                ],
                extra,
            )
        };
        for (pairs, why) in [
            (
                named(
                    "AUDIT_OTLP_HEADERS",
                    vec![(
                        "GATEWAY_SECRET_ENV_ALLOWLIST",
                        "OPENAI_API_KEY, AUDIT_OTLP_HEADERS".into(),
                    )],
                ),
                "provider allowlist",
            ),
            (
                named(
                    "AUDIT_OTLP_HEADERS",
                    vec![(
                        crate::email::SMTP_ALLOWLIST_VAR,
                        "AUDIT_OTLP_HEADERS".into(),
                    )],
                ),
                "SMTP allowlist",
            ),
            (named("OPENAI_API_KEY", vec![]), "reserved key"),
            (named("GATEWAY_SCIM_TOKEN", vec![]), "GATEWAY_ prefix"),
            (named("AWS_SECRET_ACCESS_KEY", vec![]), "AWS_ prefix"),
            (named("lower_case", vec![]), "lower case"),
        ] {
            let error = Setting::from_lookup(lookup(pairs), false)
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_default();
            assert!(
                error.contains(OTLP_HEADERS_ENV_ENV),
                "{why}: names the setting"
            );
            assert!(!error.contains(TOKEN), "{why}: header value in the error");
        }
    }

    /// #108: the collector's acknowledgement. Anything but a positive
    /// `rejectedLogRecords` (or a body that isn't JSON) rejects nothing.
    #[test]
    fn otlp_rejected_records_are_read_from_the_partial_success() {
        for (body, rejected) in [
            ("", 0),
            ("{}", 0),
            ("not json", 0),
            (r#"{"partialSuccess":{}}"#, 0),
            (r#"{"partialSuccess":{"rejectedLogRecords":"0"}}"#, 0),
            (r#"{"partialSuccess":{"rejectedLogRecords":-3}}"#, 0),
            (
                r#"{"partialSuccess":{"rejectedLogRecords":"2","errorMessage":"x"}}"#,
                2,
            ),
            (r#"{"partialSuccess":{"rejectedLogRecords":3}}"#, 3),
        ] {
            assert_eq!(rejected_records(body.as_bytes()), rejected, "{body}");
        }
    }
}

#[cfg(all(test, feature = "integration-tests"))]
mod database {
    use super::*;
    use axum::{Router, http::HeaderMap, routing::post};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU16, Ordering},
    };

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

    type Requests = Arc<Mutex<Vec<(HeaderMap, Vec<u8>)>>>;

    /// A receiver that records every request and answers with `status`.
    struct Receiver {
        url: String,
        seen: Requests,
        status: Arc<AtomicU16>,
    }
    async fn receiver() -> Receiver {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let status = Arc::new(AtomicU16::new(200));
        let (log, answer) = (seen.clone(), status.clone());
        let app = Router::new().route(
            "/ingest",
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let (log, answer) = (log.clone(), answer.clone());
                async move {
                    log.lock().unwrap().push((headers, body.to_vec()));
                    axum::http::StatusCode::from_u16(answer.load(Ordering::SeqCst)).unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/ingest", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Receiver { url, seen, status }
    }
    fn exporter(url: &str) -> Exporter {
        Exporter::new(url, SECRET.to_vec(), true).unwrap().unwrap()
    }
    async fn workspace(pool: &sqlx::PgPool, kind: &str) -> Uuid {
        let id = Uuid::new_v4();
        let owner = if kind == "personal" {
            let user = Uuid::new_v4();
            sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
                .bind(user)
                .bind(format!("{user}@example.test"))
                .execute(pool)
                .await
                .unwrap();
            Some(user)
        } else {
            None
        };
        sqlx::query("INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES($1,$2,$3,$4)")
            .bind(id)
            .bind(format!("{kind} space"))
            .bind(kind)
            .bind(owner)
            .execute(pool)
            .await
            .unwrap();
        id
    }
    async fn event(pool: &sqlx::PgPool, workspace: Option<Uuid>, action: &str) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO audit_events(id,workspace_id,action,resource_type,metadata) VALUES($1,$2,$3,'test','{\"count\":1}')")
            .bind(id).bind(workspace).bind(action).execute(pool).await.unwrap();
        id
    }
    async fn queued(pool: &sqlx::PgPool) -> Vec<Uuid> {
        sqlx::query_scalar(
            "SELECT event_id FROM audit_export_queue ORDER BY event_created_at,event_id",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }
    /// Turn export on and drop its own `audit_export.enabled` marker, so a
    /// test sees only the events it writes.
    async fn enable(pool: &sqlx::PgPool, store: &Store, receiver: &Receiver) {
        sync_state(store, &Setting::On(exporter(&receiver.url)))
            .await
            .unwrap();
        sqlx::query("DELETE FROM audit_export_queue")
            .execute(pool)
            .await
            .unwrap();
    }
    fn delivered(receiver: &Receiver) -> Vec<Value> {
        receiver
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, body)| serde_json::from_slice::<Value>(body).unwrap())
            .collect()
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn only_shared_events_are_queued_and_only_while_enabled(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let (team, project, personal) = (
            workspace(&pool, "team").await,
            workspace(&pool, "project").await,
            workspace(&pool, "personal").await,
        );
        event(&pool, None, "before.enabled").await;
        assert!(
            queued(&pool).await.is_empty(),
            "off by default: no backfill later either"
        );
        let receiver = receiver().await;
        sync_state(&store, &Setting::On(exporter(&receiver.url)))
            .await
            .unwrap();
        let marker: Uuid =
            sqlx::query_scalar("SELECT id FROM audit_events WHERE action='audit_export.enabled'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let global = event(&pool, None, "installation.updated").await;
        let t = event(&pool, Some(team), "workspace.updated").await;
        let p = event(&pool, Some(project), "workspace.updated").await;
        event(&pool, Some(personal), "key.created").await;
        assert_eq!(
            queued(&pool).await,
            vec![marker, global, t, p],
            "the enabled marker, then never personal-workspace events"
        );
        // A replica without any setting changes nothing.
        sync_state(&store, &Setting::Unset).await.unwrap();
        event(&pool, None, "unset.replica").await;
        assert_eq!(queued(&pool).await.len(), 5, "still exporting");
        // Only an explicit `off` stops it, after queueing its own marker.
        sync_state(&store, &Setting::Off).await.unwrap();
        event(&pool, None, "after.disabled").await;
        let actions: Vec<String> = sqlx::query_scalar("SELECT a.action FROM audit_export_queue q JOIN audit_events a ON a.id=q.event_id ORDER BY q.event_created_at,q.event_id").fetch_all(&pool).await.unwrap();
        assert_eq!(
            actions.last().map(String::as_str),
            Some("audit_export.disabled")
        );
        assert_eq!(
            actions.len(),
            6,
            "disabling stops queueing; queued events wait"
        );
        // Repeating a setting is not another transition.
        sync_state(&store, &Setting::Off).await.unwrap();
        let markers: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_events WHERE action LIKE 'audit_export.%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(markers, 2);
        let state: (bool, Option<String>) =
            sqlx::query_as("SELECT enabled,destination_host FROM audit_export_state")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, (false, None));
    }

    /// A currently effective Platform Admin, able to own an admin API token
    /// (migration 0051 requires it live at creation, under the owner's own
    /// user-scope authority lock).
    async fn admin_owner(pool: &sqlx::PgPool, email: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
            .bind(id)
            .bind(email)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO platform_role_grants(id,user_id,role,source) VALUES($1,$2,'admin','manual')",
        )
        .bind(Uuid::new_v4())
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    /// #37 PR2 item 1: the exported `EVENT` projection carries
    /// `admin_token_id` (additive, nullable; `omg.audit.v1` stays additive
    /// per docs/operations.md). A real admin token (migration 0051: owner
    /// must be a currently effective Platform Admin, created under the
    /// owner's own exclusive user-scope lock, exactly as PR2's
    /// `admin_tokens.rs` handler does), one event attributed to it, one
    /// plain browser-session event (`admin_token_id` stays NULL, as
    /// before), and one event on a personal workspace attributed to the
    /// same token (still excluded: token attribution never widens what is
    /// exported). Delivered to a loopback HTTP receiver, not just read
    /// back from the database.
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn delivered_events_carry_admin_token_attribution_and_personal_events_stay_excluded(
        pool: sqlx::PgPool,
    ) {
        let store = Store::new(pool.clone());
        let owner = admin_owner(&pool, "export-owner@example.test").await;
        let mut tx = crate::db::begin(&pool).await.unwrap();
        crate::governance::locks::exclusive(
            &mut tx,
            [crate::governance::locks::Scope::User(owner)],
        )
        .await
        .unwrap();
        let token = crate::admin_tokens::create_admin_token(
            &mut tx,
            owner,
            "export probe",
            &["platform:read".to_string()],
            30,
            None,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let team = workspace(&pool, "team").await;
        let personal = workspace(&pool, "personal").await;
        let receiver = receiver().await;
        enable(&pool, &store, &receiver).await;

        let attributed = Uuid::now_v7();
        sqlx::query("INSERT INTO audit_events(id,actor_user_id,workspace_id,action,resource_type,metadata,admin_token_id) VALUES($1,$2,$3,'token.action','test','{}',$4)")
            .bind(attributed).bind(owner).bind(team).bind(token.id).execute(&pool).await.unwrap();
        let browser = Uuid::now_v7();
        sqlx::query("INSERT INTO audit_events(id,actor_user_id,workspace_id,action,resource_type,metadata) VALUES($1,$2,$3,'browser.action','test','{}')")
            .bind(browser).bind(owner).bind(team).execute(&pool).await.unwrap();
        let personal_attributed = Uuid::now_v7();
        sqlx::query("INSERT INTO audit_events(id,actor_user_id,workspace_id,action,resource_type,metadata,admin_token_id) VALUES($1,$2,$3,'token.personal','test','{}',$4)")
            .bind(personal_attributed).bind(owner).bind(personal).bind(token.id).execute(&pool).await.unwrap();

        deliver_once(&store, &exporter(&receiver.url), None)
            .await
            .unwrap();
        let events: Vec<Value> = delivered(&receiver)
            .into_iter()
            .flat_map(|body| body["events"].as_array().cloned().unwrap_or_default())
            .collect();
        assert_eq!(
            events.len(),
            2,
            "the personal event must never be exported, token attribution or not: {events:?}"
        );
        let by_id = |id: Uuid| {
            events
                .iter()
                .find(|e| e["id"] == id.to_string())
                .unwrap_or_else(|| panic!("{id} missing from {events:?}"))
        };
        let a = by_id(attributed);
        assert_eq!(a["admin_token_id"], token.id.to_string());
        assert_eq!(a["actor"]["id"], owner.to_string());
        let b = by_id(browser);
        assert_eq!(
            b["admin_token_id"],
            Value::Null,
            "a browser-session event carries no admin_token_id, exactly as before"
        );
        // Never a hash or the plaintext: the id is the only admin-token
        // field, and it is a plain UUID, not the 64-hex secret or the
        // 32-byte digest.
        let body = serde_json::to_string(&a).unwrap();
        assert!(
            !body.contains(&token.token),
            "plaintext must never be exported"
        );
        assert!(
            !body.contains(&hex::encode(token.digest)),
            "the digest must never be exported"
        );
        assert_eq!(
            token.id.to_string().len(),
            36,
            "admin_token_id is a UUID, not a hash"
        );
    }

    fn seqs(receiver: &Receiver) -> Vec<i64> {
        delivered(receiver)
            .iter()
            .flat_map(|b| {
                b["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["seq"].as_i64().unwrap())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// #174: every exported event carries a gap-free, never reused sequence
    /// number, fixed before its first delivery attempt, so a receiver can see
    /// lost events; an idle export sends a signed heartbeat.
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn sequence_numbers_reveal_lost_events_and_idle_exports_heartbeat(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let receiver = receiver().await;
        let exp = exporter(&receiver.url);
        enable(&pool, &store, &receiver).await;
        for _ in 0..3 {
            event(&pool, None, "test.event").await;
        }
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(3)
        );
        assert_eq!(seqs(&receiver), [1, 2, 3]);

        // A failed attempt numbers the events; the retry sends the same numbers.
        receiver.status.store(503, Ordering::SeqCst);
        for _ in 0..2 {
            event(&pool, None, "test.event").await;
        }
        assert!(matches!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Failed(_)
        ));
        let numbered: Vec<Option<i64>> =
            sqlx::query_scalar("SELECT seq FROM audit_export_queue ORDER BY seq")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(numbered, [Some(4), Some(5)]);
        assert_eq!(
            seqs(&receiver),
            [1, 2, 3, 4, 5],
            "the failed attempt was numbered"
        );
        receiver.seen.lock().unwrap().clear();
        // Removing a queued event (what a stolen runtime credential could do)
        // leaves a gap the receiver sees.
        sqlx::query("DELETE FROM audit_export_queue WHERE seq=4")
            .execute(&pool)
            .await
            .unwrap();
        receiver.status.store(200, Ordering::SeqCst);
        sqlx::query("UPDATE audit_export_state SET next_attempt_at='-infinity'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(1)
        );
        assert_eq!(seqs(&receiver), [5], "4 is a gap");
        // Numbers are never reused or rewritten, even by the runtime role's owner path.
        assert!(
            sqlx::query("UPDATE audit_export_state SET next_seq=1")
                .execute(&pool)
                .await
                .is_err()
        );
        // Nor can a removed event's number be given to another one.
        let spare = event(&pool, None, "test.event").await;
        assert!(
            sqlx::query("UPDATE audit_export_queue SET seq=4 WHERE event_id=$1")
                .bind(spare)
                .execute(&pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query(
                "INSERT INTO audit_export_queue(event_id,event_created_at,seq) VALUES($1,now(),99)"
            )
            .bind(Uuid::now_v7())
            .execute(&pool)
            .await
            .is_err()
        );
        sqlx::query("DELETE FROM audit_export_queue WHERE event_id=$1")
            .bind(spare)
            .execute(&pool)
            .await
            .unwrap();
        event(&pool, None, "test.event").await;
        assert!(matches!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(1)
        ));
        assert_eq!(seqs(&receiver), [5, 6]);

        // Idle: nothing for a minute sends one heartbeat with the next number.
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Idle
        );
        sqlx::query("UPDATE audit_export_state SET last_success_at=now()-interval '2 minutes'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Heartbeat
        );
        let last = delivered(&receiver).pop().unwrap();
        assert_eq!(last["heartbeat"], true);
        assert_eq!(last["next_seq"], 7);
        assert_eq!(last["events"], json!([]));
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Idle
        );
        let (headers, body) = receiver.seen.lock().unwrap().last().cloned().unwrap();
        let signature = headers["x-omg-signature"].to_str().unwrap().to_owned();
        let t: i64 = signature[2..].split(',').next().unwrap().parse().unwrap();
        assert_eq!(signature, sign(SECRET, t, &body));
    }

    /// #174 review: switching export off directly in the database (what a
    /// stolen runtime credential could do) is itself exported and stops the
    /// heartbeat; a number planted on a queued row becomes a gap, not a
    /// collision that stalls export; a backlog over one numbering batch is
    /// numbered in order across runs.
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn tampering_with_the_export_state_shows_up_at_the_receiver(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let receiver = receiver().await;
        let exp = exporter(&receiver.url);
        enable(&pool, &store, &receiver).await;
        // Planted number above the counter: the exporter numbers around it.
        let planted = event(&pool, None, "test.event").await;
        sqlx::query("UPDATE audit_export_queue SET seq=10 WHERE event_id=$1")
            .bind(planted)
            .execute(&pool)
            .await
            .unwrap();
        event(&pool, None, "test.event").await;
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(2)
        );
        assert_eq!(seqs(&receiver), [10, 11]);
        // Off and on again, directly: both markers reach the receiver.
        sqlx::query("UPDATE audit_export_state SET enabled=false")
            .execute(&pool)
            .await
            .unwrap();
        event(&pool, None, "test.unexported").await;
        sqlx::query("UPDATE audit_export_state SET last_success_at=now()-interval '2 minutes'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(1)
        );
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Idle,
            "no heartbeat while off"
        );
        sqlx::query("UPDATE audit_export_state SET enabled=true")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(1)
        );
        let actions: Vec<String> = delivered(&receiver)
            .iter()
            .flat_map(|b| {
                b["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["action"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(
            actions,
            [
                "test.event",
                "test.event",
                "audit_export.disabled",
                "audit_export.enabled"
            ]
        );
        // A backlog larger than one numbering batch keeps its order.
        receiver.seen.lock().unwrap().clear();
        sqlx::query("INSERT INTO audit_events(id,action,resource_type,created_at) SELECT gen_random_uuid(),$1,'test',now()+make_interval(secs=>g) FROM generate_series(1,1050) g").bind("test.event")
            .execute(&pool).await.unwrap();
        while let Outcome::Delivered(_) = deliver_once(&store, &exp, None).await.unwrap() {}
        let got = seqs(&receiver);
        assert_eq!(got.len(), 1050);
        assert!(got.windows(2).all(|w| w[1] == w[0] + 1), "consecutive");
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn batches_are_delivered_in_order_signed_and_then_dequeued(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let receiver = receiver().await;
        let exp = exporter(&receiver.url);
        enable(&pool, &store, &receiver).await;
        let team = workspace(&pool, "team").await;
        let mut ids = Vec::new();
        for i in 0..(BATCH + 20) {
            ids.push(
                event(
                    &pool,
                    if i % 2 == 0 { Some(team) } else { None },
                    "test.event",
                )
                .await,
            );
        }
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(100)
        );
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(20)
        );
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Idle
        );
        assert!(queued(&pool).await.is_empty());
        let bodies = delivered(&receiver);
        let sent: Vec<Uuid> = bodies
            .iter()
            .flat_map(|b| {
                b["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["id"].as_str().unwrap().parse().unwrap())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(sent, ids, "every event once, in order");
        assert_eq!(bodies[0]["schema"], SCHEMA);
        let first = &bodies[0]["events"][0];
        assert_eq!(first["workspace"]["kind"], "team");
        assert_eq!(first["metadata"], json!({"count":1}));
        assert_eq!(bodies[0]["events"][1]["workspace"], Value::Null);
        // The signature verifies over "<t>.<body>" with the shared secret.
        for (headers, body) in receiver.seen.lock().unwrap().iter() {
            let signature = headers["x-omg-signature"].to_str().unwrap();
            let t: i64 = signature
                .strip_prefix("t=")
                .unwrap()
                .split(',')
                .next()
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(signature, sign(SECRET, t, body));
            assert!(headers.contains_key("x-omg-delivery"));
        }
        let total: i64 = sqlx::query_scalar("SELECT delivered_total FROM audit_export_state")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(total, BATCH + 20);
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn an_outage_backs_off_loses_nothing_and_recovers(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let receiver = receiver().await;
        let exp = exporter(&receiver.url);
        enable(&pool, &store, &receiver).await;
        let ids = [event(&pool, None, "a").await, event(&pool, None, "b").await];
        receiver.status.store(503, Ordering::SeqCst);
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Failed("http_5xx")
        );
        assert_eq!(queued(&pool).await, ids.to_vec(), "nothing lost");
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Idle,
            "backing off"
        );
        let v = status(&store).await.unwrap();
        assert_eq!(
            (
                v["retrying"].clone(),
                v["last_error"].clone(),
                v["queued"].clone()
            ),
            (json!(true), json!("http_5xx"), json!(2))
        );
        // A refused connection is a failure too, and the backoff grows.
        let down = exporter("http://127.0.0.1:9/ingest");
        sqlx::query("UPDATE audit_export_state SET next_attempt_at=now()-interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            deliver_once(&store, &down, None).await.unwrap(),
            Outcome::Failed("connection")
        );
        let attempts: i32 = sqlx::query_scalar("SELECT attempts FROM audit_export_state")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(attempts, 2);
        // Receiver back: the same events, once more, then a clean state.
        receiver.status.store(200, Ordering::SeqCst);
        sqlx::query("UPDATE audit_export_state SET next_attempt_at=now()-interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(2)
        );
        assert!(queued(&pool).await.is_empty());
        let v = status(&store).await.unwrap();
        assert_eq!(
            (v["retrying"].clone(), v["last_error"].clone()),
            (json!(false), Value::Null)
        );
        // Both attempts that reached the receiver carried the same event ids (dedup).
        let bodies = delivered(&receiver);
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0]["events"], bodies[1]["events"]);
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn a_stale_lease_term_commits_nothing(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let receiver = receiver().await;
        let exp = exporter(&receiver.url);
        enable(&pool, &store, &receiver).await;
        let id = event(&pool, None, "a").await;
        let stale = crate::leases::Fence {
            lease: crate::leases::Lease::AuditExport,
            holder: Uuid::new_v4(),
            epoch: 99,
        };
        let error = deliver_once(&store, &exp, Some(&stale)).await.unwrap_err();
        assert!(crate::leases::is_fenced(&error), "{error}");
        assert_eq!(queued(&pool).await, vec![id]);
        assert!(
            delivered(&receiver).is_empty(),
            "nothing sent without the lease"
        );
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn a_queued_event_no_longer_in_the_database_is_skipped(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let receiver = receiver().await;
        let exp = exporter(&receiver.url);
        enable(&pool, &store, &receiver).await;
        let kept = event(&pool, None, "kept").await;
        sqlx::query("INSERT INTO audit_export_queue(event_id,event_created_at) VALUES($1,now()-interval '400 days')").bind(Uuid::new_v4()).execute(&pool).await.unwrap();
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(1)
        );
        assert!(queued(&pool).await.is_empty());
        assert_eq!(delivered(&receiver)[0]["events"][0]["id"], kept.to_string());
    }

    // ---- #107: syslog over TLS ----

    use super::pki;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A mock RFC 5425 receiver (tokio-rustls server). It records every
    /// octet-counted frame and answers the sender's close_notify with its own;
    /// after `drop_after` frames on one connection it resets the connection.
    struct SyslogReceiver {
        port: u16,
        frames: Arc<Mutex<Vec<String>>>,
        drop_after: Arc<AtomicUsize>,
        connections: Arc<AtomicUsize>,
    }
    fn certs(pem: &str) -> Vec<CertificateDer<'static>> {
        CertificateDer::pem_slice_iter(pem.as_bytes())
            .collect::<Result<_, _>>()
            .unwrap()
    }
    /// One complete `<len> <message>` frame at the start of `buf`.
    fn take_frame(buf: &mut Vec<u8>) -> Option<String> {
        let space = buf.iter().position(|b| *b == b' ')?;
        let digits = std::str::from_utf8(&buf[..space]).unwrap();
        assert!(
            !digits.starts_with('0') && digits.bytes().all(|b| b.is_ascii_digit()),
            "bad octet count {digits:?}"
        );
        let len: usize = digits.parse().unwrap();
        if buf.len() < space + 1 + len {
            return None;
        }
        let message = String::from_utf8(buf[space + 1..space + 1 + len].to_vec()).unwrap();
        buf.drain(..space + 1 + len);
        Some(message)
    }
    async fn syslog_receiver(server_cert: &str, client_ca: Option<&str>) -> SyslogReceiver {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap();
        let builder = match client_ca {
            None => builder.with_no_client_auth(),
            Some(ca) => {
                let mut roots = rustls::RootCertStore::empty();
                for cert in certs(ca) {
                    roots.add(cert).unwrap();
                }
                builder.with_client_cert_verifier(
                    rustls::server::WebPkiClientVerifier::builder_with_provider(
                        Arc::new(roots),
                        provider,
                    )
                    .build()
                    .unwrap(),
                )
            }
        };
        let config = builder
            .with_single_cert(
                certs(server_cert),
                PrivateKeyDer::from_pem_slice(pki::SERVER_KEY.as_bytes()).unwrap(),
            )
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let frames = Arc::new(Mutex::new(Vec::new()));
        let drop_after = Arc::new(AtomicUsize::new(usize::MAX));
        let connections = Arc::new(AtomicUsize::new(0));
        let receiver = SyslogReceiver {
            port,
            frames: frames.clone(),
            drop_after: drop_after.clone(),
            connections: connections.clone(),
        };
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                connections.fetch_add(1, Ordering::SeqCst);
                let (acceptor, frames, drop_after) =
                    (acceptor.clone(), frames.clone(), drop_after.clone());
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let (mut buf, mut chunk, mut count) = (Vec::new(), [0u8; 4096], 0);
                    loop {
                        while let Some(frame) = take_frame(&mut buf) {
                            frames.lock().unwrap().push(frame);
                            count += 1;
                            if count >= drop_after.load(Ordering::SeqCst) {
                                // A reset, not a clean close: the receiver failed.
                                tls.get_ref().0.set_zero_linger().unwrap();
                                return;
                            }
                        }
                        match tls.read(&mut chunk).await {
                            Ok(0) => {
                                let _ = tls.shutdown().await;
                                return;
                            }
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            Err(_) => return,
                        }
                    }
                });
            }
        });
        receiver
    }
    fn syslog_exporter(port: u16, ca: Option<&str>, identity: bool) -> Exporter {
        Exporter::syslog(
            &format!("syslog+tls://127.0.0.1:{port}"),
            ca.map(str::as_bytes),
            identity.then(|| (pki::CLIENT_CERT.as_bytes(), pki::CLIENT_KEY.as_bytes())),
            true,
        )
        .unwrap()
    }
    /// Turn export on with `exporter` and drop the `audit_export.enabled` marker.
    async fn enable_with(pool: &sqlx::PgPool, store: &Store, exporter: Exporter) {
        sync_state(store, &Setting::On(exporter)).await.unwrap();
        sqlx::query("DELETE FROM audit_export_queue")
            .execute(pool)
            .await
            .unwrap();
    }
    async fn retry_now(pool: &sqlx::PgPool) {
        sqlx::query("UPDATE audit_export_state SET next_attempt_at='-infinity'")
            .execute(pool)
            .await
            .unwrap();
    }
    async fn installation(pool: &sqlx::PgPool) -> Uuid {
        sqlx::query_scalar("SELECT id FROM installation")
            .fetch_one(pool)
            .await
            .unwrap()
    }
    /// An RFC 5424 message as (MSGID, JSON), checking every other field.
    fn rfc5424(message: &str, installation: Uuid) -> (String, Value) {
        let parts: Vec<&str> = message.splitn(8, ' ').collect();
        assert_eq!(parts.len(), 8, "{message}");
        assert_eq!(
            parts[0], "<109>1",
            "PRI 109 (facility 13, severity 5), version 1"
        );
        let time = parts[1];
        assert!(
            time.len() == 24 && time.ends_with('Z') && time.as_bytes()[19] == b'.',
            "UTC with milliseconds: {time}"
        );
        chrono::DateTime::parse_from_rfc3339(time).unwrap();
        assert_eq!(parts[2], installation.to_string(), "HOSTNAME");
        assert_eq!(parts[3], "open-model-gateway", "APP-NAME");
        assert_eq!(parts[4], "-", "PROCID");
        assert_eq!(parts[6], "-", "no structured data");
        assert!(parts[7].starts_with('{'), "no BOM");
        let json: Value = serde_json::from_str(parts[7]).unwrap();
        assert_eq!(json["schema"], SCHEMA);
        assert_eq!(json["installation_id"], installation.to_string());
        (parts[5].to_owned(), json)
    }
    fn received(receiver: &SyslogReceiver) -> Vec<String> {
        receiver.frames.lock().unwrap().clone()
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn syslog_delivers_octet_counted_rfc_5424_messages_in_sequence_order(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let receiver = syslog_receiver(pki::SERVER_CERT, None).await;
        let exp = syslog_exporter(receiver.port, Some(pki::CA), false);
        assert_eq!((exp.transport(), exp.host()), ("syslog", "127.0.0.1"));
        enable_with(
            &pool,
            &store,
            syslog_exporter(receiver.port, Some(pki::CA), false),
        )
        .await;
        let team = workspace(&pool, "team").await;
        let long = "weird action with spaces and \u{fc}nicode, well over thirty-two characters";
        let mut ids = Vec::new();
        for i in 0..(BATCH + 5) {
            let action = if i == 1 { long } else { "key.revoked" };
            ids.push(event(&pool, (i % 2 == 0).then_some(team), action).await);
        }
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(100)
        );
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(5)
        );
        assert!(queued(&pool).await.is_empty());
        assert_eq!(
            receiver.connections.load(Ordering::SeqCst),
            2,
            "one connection per batch"
        );
        let installation = installation(&pool).await;
        let messages: Vec<(String, Value)> = received(&receiver)
            .iter()
            .map(|m| rfc5424(m, installation))
            .collect();
        assert_eq!(messages.len(), ids.len(), "one message per event");
        for (i, ((msgid, json), id)) in messages.iter().zip(&ids).enumerate() {
            let event = &json["event"];
            assert_eq!(event["id"], id.to_string());
            assert_eq!(event["seq"], json!(i + 1), "in sequence order");
            assert_eq!(*msgid, super::msgid(event["action"].as_str().unwrap()));
            let mut keys: Vec<&String> = json.as_object().unwrap().keys().collect();
            keys.sort();
            assert_eq!(keys, ["event", "installation_id", "schema"]);
        }
        // MSGID is the action, truncated to 32 printable ASCII characters.
        assert_eq!(messages[0].0, "key.revoked");
        assert_eq!(messages[1].0, "weird_action_with_spaces_and__ni");
        assert_eq!(
            messages[1].1["event"]["action"], long,
            "the JSON keeps the action"
        );
        // The same event object the webhook sends.
        let first = &messages[0].1["event"];
        assert_eq!(first["workspace"]["kind"], "team");
        assert_eq!(first["metadata"], json!({"count":1}));
        assert_eq!(messages[1].1["event"]["workspace"], Value::Null);
        let v = status(&store).await.unwrap();
        assert_eq!(
            (v["destination_host"].clone(), v["delivered_total"].clone()),
            (json!("127.0.0.1"), json!("105"))
        );
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn syslog_refuses_an_untrusted_or_misnamed_certificate(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let receiver = syslog_receiver(pki::SERVER_CERT, None).await;
        enable_with(
            &pool,
            &store,
            syslog_exporter(receiver.port, Some(pki::CA), false),
        )
        .await;
        let id = event(&pool, None, "key.revoked").await;
        // The public roots (no CA file) and an unrelated CA don't trust it.
        for ca in [None, Some(pki::OTHER_CA)] {
            retry_now(&pool).await;
            assert_eq!(
                deliver_once(&store, &syslog_exporter(receiver.port, ca, false), None)
                    .await
                    .unwrap(),
                Outcome::Failed("tls")
            );
        }
        // Trusted CA, wrong name: the certificate is for siem.example.test.
        let misnamed = syslog_receiver(pki::WRONG_NAME_CERT, None).await;
        retry_now(&pool).await;
        assert_eq!(
            deliver_once(
                &store,
                &syslog_exporter(misnamed.port, Some(pki::CA), false),
                None
            )
            .await
            .unwrap(),
            Outcome::Failed("tls")
        );
        assert_eq!(queued(&pool).await, vec![id], "nothing dequeued");
        assert!(received(&receiver).is_empty() && received(&misnamed).is_empty());
        assert_eq!(status(&store).await.unwrap()["last_error"], "tls");
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn syslog_a_receiver_that_fails_mid_batch_gets_the_whole_batch_again(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let receiver = syslog_receiver(pki::SERVER_CERT, None).await;
        let exp = syslog_exporter(receiver.port, Some(pki::CA), false);
        enable_with(
            &pool,
            &store,
            syslog_exporter(receiver.port, Some(pki::CA), false),
        )
        .await;
        let mut ids = Vec::new();
        for _ in 0..10 {
            ids.push(event(&pool, None, "key.revoked").await);
        }
        receiver.drop_after.store(3, Ordering::SeqCst);
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Failed("connection")
        );
        assert_eq!(queued(&pool).await, ids, "nothing dequeued");
        assert_eq!(received(&receiver).len(), 3);
        receiver.drop_after.store(usize::MAX, Ordering::SeqCst);
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Idle,
            "backing off"
        );
        retry_now(&pool).await;
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(10)
        );
        assert!(queued(&pool).await.is_empty());
        assert_eq!(
            receiver.connections.load(Ordering::SeqCst),
            2,
            "a new connection"
        );
        let installation = installation(&pool).await;
        let seqs: Vec<i64> = received(&receiver)
            .iter()
            .map(|m| rfc5424(m, installation).1["event"]["seq"].as_i64().unwrap())
            .collect();
        assert_eq!(
            seqs,
            [1, 2, 3, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
            "the full batch, same numbers"
        );
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn syslog_mutual_tls_presents_the_configured_client_certificate(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let receiver = syslog_receiver(pki::SERVER_CERT, Some(pki::CA)).await;
        enable_with(
            &pool,
            &store,
            syslog_exporter(receiver.port, Some(pki::CA), true),
        )
        .await;
        let id = event(&pool, None, "key.revoked").await;
        assert!(matches!(
            deliver_once(
                &store,
                &syslog_exporter(receiver.port, Some(pki::CA), false),
                None
            )
            .await
            .unwrap(),
            Outcome::Failed("tls")
        ));
        assert_eq!(
            queued(&pool).await,
            vec![id],
            "refused without a client certificate"
        );
        assert!(received(&receiver).is_empty());
        retry_now(&pool).await;
        assert_eq!(
            deliver_once(
                &store,
                &syslog_exporter(receiver.port, Some(pki::CA), true),
                None
            )
            .await
            .unwrap(),
            Outcome::Delivered(1)
        );
        assert!(queued(&pool).await.is_empty());
        assert_eq!(received(&receiver).len(), 1);
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn syslog_heartbeat_and_test_delivery_framing(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let receiver = syslog_receiver(pki::SERVER_CERT, None).await;
        let exp = syslog_exporter(receiver.port, Some(pki::CA), false);
        enable_with(
            &pool,
            &store,
            syslog_exporter(receiver.port, Some(pki::CA), false),
        )
        .await;
        let installation = installation(&pool).await;
        sqlx::query("UPDATE audit_export_state SET last_success_at=now()-interval '2 minutes'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Heartbeat
        );
        let frames = received(&receiver);
        assert_eq!(frames.len(), 1);
        assert_eq!(
            rfc5424(&frames[0], installation),
            (
                "heartbeat".to_owned(),
                json!({"schema":SCHEMA,"installation_id":installation,"heartbeat":true,"next_seq":1})
            )
        );
        send_test(&store, &exp).await.unwrap();
        let frames = received(&receiver);
        assert_eq!(
            rfc5424(&frames[1], installation),
            (
                "test".to_owned(),
                json!({"schema":SCHEMA,"installation_id":installation,"test":true})
            )
        );
        // A test that can't be delivered says why.
        assert_eq!(
            send_test(&store, &syslog_exporter(receiver.port, None, false)).await,
            Err("tls")
        );
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn syslog_connects_only_where_the_egress_policy_allows(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let receiver = syslog_receiver(pki::SERVER_CERT, None).await;
        // Address literals the policy refuses stop startup.
        for refused in [
            "syslog+tls://10.0.0.1",
            "syslog+tls://127.0.0.1:6514",
            "syslog+tls://[::1]",
        ] {
            let error = Exporter::syslog(refused, None, None, false)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("outbound address policy"),
                "{refused}: {error}"
            );
        }
        // A name that resolves to loopback, outside development: refused when
        // connecting, never connected, nothing dequeued.
        let strict = Exporter::syslog(
            &format!("syslog+tls://localhost:{}", receiver.port),
            Some(pki::CA.as_bytes()),
            None,
            false,
        )
        .unwrap();
        enable_with(
            &pool,
            &store,
            Exporter::syslog(
                &format!("syslog+tls://localhost:{}", receiver.port),
                Some(pki::CA.as_bytes()),
                None,
                false,
            )
            .unwrap(),
        )
        .await;
        let id = event(&pool, None, "key.revoked").await;
        assert_eq!(
            deliver_once(&store, &strict, None).await.unwrap(),
            Outcome::Failed("connection")
        );
        assert_eq!(receiver.connections.load(Ordering::SeqCst), 0);
        assert_eq!(queued(&pool).await, vec![id]);
    }

    // ---- #108: OTLP/HTTP JSON logs ----

    /// A mock OTLP/HTTP collector: records every request to `/v1/logs` and
    /// answers with `status` and the JSON `reply`.
    struct Collector {
        url: String,
        seen: Requests,
        status: Arc<AtomicU16>,
        reply: Arc<Mutex<String>>,
    }
    async fn collector() -> Collector {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let status = Arc::new(AtomicU16::new(200));
        let reply = Arc::new(Mutex::new(String::new()));
        let (log, answer, text) = (seen.clone(), status.clone(), reply.clone());
        let app = Router::new().route(
            "/v1/logs",
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let (log, answer, text) = (log.clone(), answer.clone(), text.clone());
                async move {
                    log.lock().unwrap().push((headers, body.to_vec()));
                    (
                        axum::http::StatusCode::from_u16(answer.load(Ordering::SeqCst)).unwrap(),
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        text.lock().unwrap().clone(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/logs", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Collector {
            url,
            seen,
            status,
            reply,
        }
    }
    fn otlp_exporter(url: &str, headers: Option<&str>) -> Exporter {
        Exporter::otlp(url, headers, true).unwrap()
    }
    fn requests(collector: &Collector) -> Vec<Value> {
        collector
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, body)| serde_json::from_slice::<Value>(body).unwrap())
            .collect()
    }
    /// Every log record of every request, in order.
    fn log_records(collector: &Collector) -> Vec<Value> {
        requests(collector)
            .iter()
            .flat_map(|body| {
                body["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
                    .as_array()
                    .unwrap()
                    .clone()
            })
            .collect()
    }
    /// A record's attributes as a key → value object map.
    fn attributes(record: &Value) -> serde_json::Map<String, Value> {
        record["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| (a["key"].as_str().unwrap().to_owned(), a["value"].clone()))
            .collect()
    }
    /// The record's body: a string holding a JSON object.
    fn record_body(record: &Value) -> Value {
        let mut keys: Vec<&String> = record["body"].as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, ["stringValue"]);
        serde_json::from_str(record["body"]["stringValue"].as_str().unwrap()).unwrap()
    }
    fn record_seqs(collector: &Collector) -> Vec<String> {
        log_records(collector)
            .iter()
            .map(|r| {
                attributes(r)["omg.audit.seq"]["intValue"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }
    fn now_nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
    fn nanos(value: &Value) -> u128 {
        value
            .as_str()
            .expect("a 64-bit integer is a decimal string")
            .parse()
            .unwrap()
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn otlp_maps_each_event_to_one_log_record_with_the_webhook_event_as_body(
        pool: sqlx::PgPool,
    ) {
        let store = Store::new(pool.clone());
        let collector = collector().await;
        let exp = otlp_exporter(&collector.url, None);
        assert_eq!((exp.transport(), exp.host()), ("otlp", "127.0.0.1"));
        enable_with(&pool, &store, otlp_exporter(&collector.url, None)).await;
        let (team, personal) = (
            workspace(&pool, "team").await,
            workspace(&pool, "personal").await,
        );
        let first = event(&pool, Some(team), "key.revoked").await;
        event(&pool, Some(personal), "key.created").await;
        let second = event(&pool, None, "test.event").await;
        let before = now_nanos();
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(2)
        );
        let after = now_nanos();
        assert!(queued(&pool).await.is_empty());

        let seen = collector.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "one request per batch");
        let headers = &seen[0].0;
        assert_eq!(headers["content-type"], "application/json");
        assert!(!headers.contains_key("x-omg-signature"));
        let body = requests(&collector).remove(0);
        let installation = installation(&pool).await;
        assert_eq!(body.as_object().unwrap().len(), 1);
        let resource_logs = body["resourceLogs"].as_array().unwrap();
        assert_eq!(resource_logs.len(), 1);
        assert_eq!(
            resource_logs[0]["resource"],
            json!({"attributes":[
                {"key":"service.name","value":{"stringValue":"open-model-gateway"}},
                {"key":"omg.installation_id","value":{"stringValue":installation.to_string()}}]})
        );
        let scope_logs = resource_logs[0]["scopeLogs"].as_array().unwrap();
        assert_eq!(scope_logs.len(), 1);
        assert_eq!(
            scope_logs[0]["scope"],
            json!({"name":"omg.audit","version":"omg.audit.v1"})
        );
        let records = log_records(&collector);
        assert_eq!(
            records.len(),
            2,
            "one record per event; never the personal-workspace event"
        );
        for (i, (record, id)) in records.iter().zip([first, second]).enumerate() {
            let seq = i64::try_from(i).unwrap() + 1;
            // The body is the event object exactly as the webhook sends it.
            let mut expected: Value = sqlx::query_scalar(&format!("SELECT {EVENT} FROM audit_events a LEFT JOIN workspaces w ON w.id=a.workspace_id WHERE a.id=$1"))
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
            expected["seq"] = json!(seq);
            assert_eq!(record_body(record), expected);
            // occurred_at in nanoseconds, as a decimal string.
            let micros: i64 = sqlx::query_scalar(
                "SELECT (extract(epoch FROM created_at)*1000000)::bigint FROM audit_events WHERE id=$1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                record["timeUnixNano"],
                json!((i128::from(micros) * 1000).to_string())
            );
            let observed = nanos(&record["observedTimeUnixNano"]);
            assert!((before..=after).contains(&observed), "the send time");
            assert_eq!(record["severityNumber"], 9);
            assert_eq!(record["severityText"], "INFO");
            let attributes = attributes(record);
            assert_eq!(
                attributes,
                json!({
                    "event.name":{"stringValue":expected["action"]},
                    "omg.audit.id":{"stringValue":id.to_string()},
                    "omg.audit.seq":{"intValue":seq.to_string()},
                })
                .as_object()
                .unwrap()
                .clone()
            );
            let mut keys: Vec<&String> = record.as_object().unwrap().keys().collect();
            keys.sort();
            assert_eq!(
                keys,
                [
                    "attributes",
                    "body",
                    "observedTimeUnixNano",
                    "severityNumber",
                    "severityText",
                    "timeUnixNano"
                ]
            );
        }
        assert_eq!(record_body(&records[0])["workspace"]["kind"], "team");
        assert_eq!(record_body(&records[0])["metadata"], json!({"count":1}));
        assert_eq!(
            attributes(&records[0])["event.name"],
            json!({"stringValue":"key.revoked"})
        );
        let v = status(&store).await.unwrap();
        assert_eq!(
            (
                v["destination_host"].clone(),
                v["delivered_total"].clone(),
                v["last_error"].clone()
            ),
            (json!("127.0.0.1"), json!("2"), Value::Null)
        );
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn otlp_sends_the_configured_headers(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let collector = collector().await;
        let headers = Some("Authorization=Bearer collector-token,X-Scope-OrgID=audit");
        let exp = otlp_exporter(&collector.url, headers);
        enable_with(&pool, &store, otlp_exporter(&collector.url, headers)).await;
        event(&pool, None, "test.event").await;
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(1)
        );
        send_test(&store, &exp).await.unwrap();
        let seen = collector.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        for (headers, _) in &seen {
            assert_eq!(headers["authorization"], "Bearer collector-token");
            assert_eq!(headers["x-scope-orgid"], "audit");
            assert_eq!(headers["content-type"], "application/json");
            assert_eq!(headers.get_all("content-type").iter().count(), 1);
        }
    }

    /// A 2xx is success whatever its body, unless `partialSuccess` says the
    /// collector refused records: those are dequeued (OTLP says not to retry
    /// them), counted and shown, and receivers see the gap through `seq`.
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn otlp_a_partial_rejection_is_dequeued_recorded_counted_and_not_retried(
        pool: sqlx::PgPool,
    ) {
        let store = Store::new(pool.clone());
        let collector = collector().await;
        let exp = otlp_exporter(&collector.url, None);
        enable_with(&pool, &store, otlp_exporter(&collector.url, None)).await;
        let rejected_before = crate::metrics::METRICS.audit_export_rejected_count();
        // Acknowledgements that reject nothing.
        let oversized = format!(
            "{{\"partialSuccess\":{{\"rejectedLogRecords\":\"1\"}},\"pad\":\"{}\"}}",
            "x".repeat(70 * 1024)
        );
        for reply in [
            "",
            "{}",
            "not json",
            r#"{"partialSuccess":{}}"#,
            r#"{"partialSuccess":{"rejectedLogRecords":"0"}}"#,
            oversized.as_str(),
        ] {
            *collector.reply.lock().unwrap() = reply.to_owned();
            event(&pool, None, "test.event").await;
            assert_eq!(
                deliver_once(&store, &exp, None).await.unwrap(),
                Outcome::Delivered(1),
                "{:.40}",
                reply
            );
            assert_eq!(
                status(&store).await.unwrap()["last_error"],
                Value::Null,
                "{:.40}",
                reply
            );
        }
        assert_eq!(
            crate::metrics::METRICS.audit_export_rejected_count(),
            rejected_before
        );
        // Two of three refused.
        *collector.reply.lock().unwrap() =
            r#"{"partialSuccess":{"rejectedLogRecords":"2","errorMessage":"invalid"}}"#.into();
        for _ in 0..3 {
            event(&pool, None, "test.event").await;
        }
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(3)
        );
        assert!(queued(&pool).await.is_empty(), "dequeued");
        let v = status(&store).await.unwrap();
        assert_eq!(
            (
                v["last_error"].clone(),
                v["retrying"].clone(),
                v["last_error_at"].is_string(),
                v["delivered_total"].clone()
            ),
            (json!("otlp_rejected"), json!(false), true, json!("7")),
            "six accepted before, one of three now"
        );
        let attempts: i32 = sqlx::query_scalar("SELECT attempts FROM audit_export_state")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(attempts, 0);
        assert_eq!(
            crate::metrics::METRICS.audit_export_rejected_count(),
            rejected_before + 2
        );
        // Not retried: nothing more is sent.
        let sent = collector.seen.lock().unwrap().len();
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Idle
        );
        assert_eq!(collector.seen.lock().unwrap().len(), sent);
        assert_eq!(record_seqs(&collector).len(), 9, "every record sent once");

        // Everything refused: not a success, so the last delivery time (and
        // the silence and backlog alerts) stays where it was.
        let last = |v: &Value| v["last_success_at"].clone();
        let before = last(&status(&store).await.unwrap());
        sqlx::query("UPDATE audit_export_state SET last_success_at=now()-interval '10 minutes'")
            .execute(&pool)
            .await
            .unwrap();
        let ten_minutes_ago = last(&status(&store).await.unwrap());
        assert_ne!(before, ten_minutes_ago);
        *collector.reply.lock().unwrap() =
            r#"{"partialSuccess":{"rejectedLogRecords":"1"}}"#.into();
        event(&pool, None, "test.event").await;
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(1)
        );
        assert_eq!(last(&status(&store).await.unwrap()), ten_minutes_ago);
        // A refused heartbeat is a failure, backed off; not a heartbeat sent.
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Failed("otlp_rejected")
        );
        let v = status(&store).await.unwrap();
        assert_eq!(
            (v["retrying"].clone(), v["last_heartbeat_at"].clone()),
            (json!(true), Value::Null)
        );
        // The next clean delivery clears the error.
        *collector.reply.lock().unwrap() = "{}".into();
        retry_now(&pool).await;
        event(&pool, None, "test.event").await;
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(1)
        );
        let v = status(&store).await.unwrap();
        assert_eq!(
            (v["last_error"].clone(), v["retrying"].clone()),
            (Value::Null, json!(false))
        );
        assert_ne!(last(&v), ten_minutes_ago);
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn otlp_an_unavailable_collector_keeps_the_batch_and_the_retry_has_the_same_seq(
        pool: sqlx::PgPool,
    ) {
        let store = Store::new(pool.clone());
        let collector = collector().await;
        let exp = otlp_exporter(&collector.url, None);
        enable_with(&pool, &store, otlp_exporter(&collector.url, None)).await;
        let ids = [
            event(&pool, None, "test.event").await,
            event(&pool, None, "test.event").await,
        ];
        collector.status.store(503, Ordering::SeqCst);
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Failed("http_5xx")
        );
        assert_eq!(queued(&pool).await, ids.to_vec(), "nothing lost");
        assert_eq!(status(&store).await.unwrap()["retrying"], true);
        collector.status.store(400, Ordering::SeqCst);
        retry_now(&pool).await;
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Failed("http_4xx")
        );
        retry_now(&pool).await;
        assert_eq!(
            deliver_once(
                &store,
                &otlp_exporter("http://127.0.0.1:9/v1/logs", None),
                None
            )
            .await
            .unwrap(),
            Outcome::Failed("connection")
        );
        collector.status.store(200, Ordering::SeqCst);
        retry_now(&pool).await;
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Delivered(2)
        );
        assert!(queued(&pool).await.is_empty());
        assert_eq!(
            record_seqs(&collector),
            ["1", "2", "1", "2", "1", "2"],
            "every attempt, the same numbers"
        );
        let ids: Vec<String> = log_records(&collector)
            .iter()
            .map(|r| {
                attributes(r)["omg.audit.id"]["stringValue"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(ids[..2], ids[4..], "the same events again");
        assert_eq!(status(&store).await.unwrap()["last_error"], Value::Null);
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn otlp_heartbeat_and_test_records(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let collector = collector().await;
        let exp = otlp_exporter(&collector.url, None);
        enable_with(&pool, &store, otlp_exporter(&collector.url, None)).await;
        let installation = installation(&pool).await;
        sqlx::query("UPDATE audit_export_state SET last_success_at=now()-interval '2 minutes'")
            .execute(&pool)
            .await
            .unwrap();
        let before = now_nanos();
        assert_eq!(
            deliver_once(&store, &exp, None).await.unwrap(),
            Outcome::Heartbeat
        );
        send_test(&store, &exp).await.unwrap();
        let after = now_nanos();
        let bodies = requests(&collector);
        assert_eq!(bodies.len(), 2);
        for body in &bodies {
            assert_eq!(
                body["resourceLogs"][0]["resource"]["attributes"][1],
                json!({"key":"omg.installation_id","value":{"stringValue":installation.to_string()}})
            );
            assert_eq!(
                body["resourceLogs"][0]["scopeLogs"][0]["scope"],
                json!({"name":"omg.audit","version":"omg.audit.v1"})
            );
        }
        let records = log_records(&collector);
        assert_eq!(records.len(), 2, "one record each");
        for record in &records {
            assert!((before..=after).contains(&nanos(&record["timeUnixNano"])));
            assert!((before..=after).contains(&nanos(&record["observedTimeUnixNano"])));
            assert_eq!(
                (
                    record["severityNumber"].clone(),
                    record["severityText"].clone()
                ),
                (json!(9), json!("INFO"))
            );
        }
        assert_eq!(
            attributes(&records[0]),
            json!({
                "event.name":{"stringValue":"omg.audit.heartbeat"},
                "omg.audit.next_seq":{"intValue":"1"},
            })
            .as_object()
            .unwrap()
            .clone()
        );
        assert_eq!(
            records[0]["body"]["stringValue"],
            r#"{"schema":"omg.audit.v1","heartbeat":true,"next_seq":1}"#
        );
        assert_eq!(
            attributes(&records[1]),
            json!({"event.name":{"stringValue":"omg.audit.test"}})
                .as_object()
                .unwrap()
                .clone()
        );
        assert_eq!(
            record_body(&records[1]),
            json!({"schema":"omg.audit.v1","test":true})
        );
        // A test the collector refuses says so.
        *collector.reply.lock().unwrap() = r#"{"partialSuccess":{"rejectedLogRecords":1}}"#.into();
        assert_eq!(send_test(&store, &exp).await, Err("otlp_rejected"));
        collector.status.store(503, Ordering::SeqCst);
        assert_eq!(send_test(&store, &exp).await, Err("http_5xx"));
    }
}
