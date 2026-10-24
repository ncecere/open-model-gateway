use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use sqlx::postgres::PgPoolOptions;

/// Deliberately not Debug: DATABASE_URL may contain credentials.
pub struct Config {
    pub database_url: String,
    pub listen: SocketAddr,
    pub environment: Environment,
    pub web_directory: Option<PathBuf>,
    /// Display-only SDK base URL; never used for routing or browser API calls.
    pub inference_base_url: InferenceBaseUrl,
    pub secret_env_allowlist: Vec<String>,
    pub smtp_env_allowlist: Vec<String>,
    pub alert_webhooks: AlertWebhookPolicy,
    pub inference_limits: crate::inference::EngineLimits,
    /// Optional separate Prometheus listener (`GATEWAY_METRICS_ADDR`); `None` disables it.
    pub metrics_listen: Option<SocketAddr>,
    /// Optional independent OTLP push transport; never a readiness dependency.
    pub metrics_export: MetricsExportConfig,
    /// `GATEWAY_DATABASE_MAX_CONNECTIONS` (default 10) per replica.
    pub database_max_connections: u32,
    /// Optional `GATEWAY_REPORTING_DATABASE_URL`: a read-only reporting
    /// replica (or reporting pooler) for reports, usage and logs only. Unset
    /// means those reads use the primary. Never used for admission,
    /// settlement or authorization of writes.
    pub reporting_database_url: Option<String>,
    /// `GATEWAY_REPORTING_MAX_LAG_SECONDS` (default 30, 1-3600): a replica
    /// replaying WAL further behind than this is skipped for the primary.
    pub reporting_max_lag: Duration,
    /// `GATEWAY_ADMISSION_MODE` (`scoped`, default, or `global`): scoped
    /// admission locks, or the former installation row lock as an
    /// operational rollback for one release (see `governance::locks`).
    pub admission_mode: crate::governance::locks::AdmissionMode,
    /// `GATEWAY_LISTEN_DATABASE_URL` (default `DATABASE_URL`): the session
    /// connection that LISTENs for configuration changes (`crate::notify`).
    /// LISTEN needs a session, so behind a transaction-mode PgBouncer point it
    /// at PostgreSQL directly (or a session-mode pooler).
    pub listen_database_url: Option<String>,
    /// `GATEWAY_CONFIG_CACHE` (`on`, default, or `off`): per-replica caches of
    /// pre-admission reads (`crate::cache`). `off` reads everything live.
    pub config_cache: bool,
    /// Preview OTLP trace transport; no engine instrumentation yet.
    pub traces: TraceConfig,
}

/// Operator-only trace configuration. Never Debug: headers contain secrets.
#[derive(Clone)]
pub struct TraceConfig {
    pub(crate) endpoint: Option<(reqwest::Url, reqwest::Client)>,
    pub(crate) headers: reqwest::header::HeaderMap,
    pub(crate) sample_ratio: f64,
    pub(crate) environment: Environment,
}

impl TraceConfig {
    pub const URL: &'static str = "GATEWAY_TRACES_OTLP_URL";
    pub const RATIO: &'static str = "GATEWAY_TRACES_SAMPLE_RATIO";
    pub const HEADERS: &'static str = "GATEWAY_TRACES_OTLP_HEADERS_ENV";

    pub fn from_env(environment: Environment) -> Result<Self> {
        // Do not turn non-Unicode settings into an absent (disabled) setting.
        let mut values = std::collections::HashMap::new();
        for key in [Self::URL, Self::RATIO, Self::HEADERS] {
            match std::env::var(key) {
                Ok(value) => {
                    values.insert(key.to_owned(), value);
                }
                Err(std::env::VarError::NotPresent) => {}
                Err(_) => bail!("{key} is not valid Unicode"),
            }
        }
        Self::from_lookup(
            |key| values.get(key).cloned().or_else(|| std::env::var(key).ok()),
            environment,
        )
    }

    pub fn from_lookup(
        get: impl Fn(&str) -> Option<String>,
        environment: Environment,
    ) -> Result<Self> {
        let invalid = || {
            anyhow::anyhow!(
                "invalid trace exporter configuration (check GATEWAY_TRACES_* settings)"
            )
        };
        let sample_ratio = get(Self::RATIO)
            .map_or(Ok(0.01), |value| value.parse::<f64>())
            .map_err(|_| invalid())?;
        anyhow::ensure!(
            sample_ratio.is_finite() && (0.0..=1.0).contains(&sample_ratio),
            "invalid trace sample ratio"
        );
        let endpoint = get(Self::URL)
            .map(|value| {
                crate::otlp::endpoint(
                    Self::URL,
                    &value,
                    "/v1/traces",
                    environment,
                    Duration::from_secs(2),
                )
            })
            .transpose()?;
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(name) = get(Self::HEADERS) {
            anyhow::ensure!(endpoint.is_some(), "trace headers require a trace endpoint");
            headers = crate::otlp::headers(Self::HEADERS, &name, &get).map_err(|_| invalid())?;
        }
        Ok(Self {
            endpoint,
            headers,
            sample_ratio,
            environment,
        })
    }
}

/// Operator-only metrics export settings. Never Debug: headers contain secrets.
/// Parsing does not spawn a task or send anything.
#[derive(Clone)]
pub struct MetricsExportConfig {
    pub endpoint: Option<(reqwest::Url, reqwest::Client)>,
    pub headers: reqwest::header::HeaderMap,
    pub interval: Duration,
    /// Includes identity-free personal-workspace traffic when explicitly on.
    pub traffic: bool,
}

impl MetricsExportConfig {
    pub const URL: &'static str = "GATEWAY_METRICS_OTLP_URL";
    pub const HEADERS: &'static str = "GATEWAY_METRICS_OTLP_HEADERS_ENV";
    pub const INTERVAL: &'static str = "GATEWAY_METRICS_OTLP_INTERVAL_SECONDS";
    pub const TRAFFIC: &'static str = "GATEWAY_METRICS_OTLP_TRAFFIC";

    pub fn from_env(environment: Environment) -> Result<Self> {
        let mut values = std::collections::HashMap::new();
        for key in [Self::URL, Self::HEADERS, Self::INTERVAL, Self::TRAFFIC] {
            match std::env::var(key) {
                Ok(value) => {
                    values.insert(key.to_owned(), value);
                }
                Err(std::env::VarError::NotPresent) => {}
                Err(_) => bail!("{key} is not valid Unicode"),
            }
        }
        Self::from_lookup(
            |key| values.get(key).cloned().or_else(|| std::env::var(key).ok()),
            environment,
        )
    }

    pub fn from_lookup(
        get: impl Fn(&str) -> Option<String>,
        environment: Environment,
    ) -> Result<Self> {
        let url = get(Self::URL);
        for key in [Self::HEADERS, Self::INTERVAL, Self::TRAFFIC] {
            anyhow::ensure!(
                url.is_some() || get(key).is_none(),
                "{key} requires GATEWAY_METRICS_OTLP_URL"
            );
        }
        let endpoint = url
            .map(|value| {
                crate::otlp::endpoint(
                    Self::URL,
                    &value,
                    "/v1/metrics",
                    environment,
                    Duration::from_secs(5),
                )
            })
            .transpose()?;
        let headers = get(Self::HEADERS)
            .map(|name| crate::otlp::headers(Self::HEADERS, &name, &get))
            .transpose()?
            .unwrap_or_default();
        let interval = match get(Self::INTERVAL) {
            None => 60,
            Some(value) => {
                let invalid = || {
                    anyhow::anyhow!(
                        "GATEWAY_METRICS_OTLP_INTERVAL_SECONDS must be an integer from 10 to 300"
                    )
                };
                anyhow::ensure!(
                    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
                    "GATEWAY_METRICS_OTLP_INTERVAL_SECONDS must be an integer from 10 to 300"
                );
                let n = value.parse::<u64>().map_err(|_| invalid())?;
                anyhow::ensure!(
                    (10..=300).contains(&n),
                    "GATEWAY_METRICS_OTLP_INTERVAL_SECONDS must be an integer from 10 to 300"
                );
                n
            }
        };
        let traffic = match get(Self::TRAFFIC).as_deref() {
            None | Some("off") => false,
            Some("on") => true,
            _ => bail!("GATEWAY_METRICS_OTLP_TRAFFIC must be off or on"),
        };
        Ok(Self {
            endpoint,
            headers,
            interval: Duration::from_secs(interval),
            traffic,
        })
    }
}

/// Validated at startup and exposed only to authenticated dashboard users.
#[derive(Clone, Default)]
pub struct InferenceBaseUrl(pub Option<String>);

impl InferenceBaseUrl {
    pub fn parse(value: Option<&str>, environment: Environment) -> Result<Self> {
        let Some(value) = value else {
            return Ok(Self::default());
        };
        let invalid = || {
            anyhow::anyhow!(
                "GATEWAY_INFERENCE_BASE_URL must be an absolute HTTPS URL ending in /v1, without credentials, query or fragment (HTTP only on development loopback)"
            )
        };
        let mut url = reqwest::Url::parse(value).map_err(|_| invalid())?;
        let loopback = url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        let path = url.path().strip_suffix('/').unwrap_or(url.path());
        if url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !(url.scheme() == "https"
                || (environment == Environment::Development && loopback && url.scheme() == "http"))
            || !path.ends_with("/v1")
            || value.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(invalid());
        }
        let path = path.to_owned();
        url.set_path(&path);
        Ok(Self(Some(url.to_string())))
    }
}

/// Operator approval is canonical URL equality, independent of DNS/egress policy.
/// Never Debug: paths and reference names are management-only metadata.
#[derive(Clone, Default)]
pub struct AlertWebhookPolicy {
    urls: std::collections::BTreeSet<String>,
    pub(crate) secrets: crate::providers::secrets::EnvSecrets,
    development: bool,
}
impl AlertWebhookPolicy {
    pub fn parse(urls: &[String], names: Vec<String>, environment: Environment) -> Result<Self> {
        Ok(Self {
            urls: urls
                .iter()
                .map(|url| Self::canonical(url, environment == Environment::Development))
                .collect::<Result<_>>()?,
            secrets: crate::providers::secrets::EnvSecrets::new(names),
            development: environment == Environment::Development,
        })
    }
    fn canonical(value: &str, development: bool) -> Result<String> {
        let invalid = || {
            anyhow::anyhow!(
                "invalid alert webhook URL (HTTPS required except development loopback; no credentials, query or fragment)"
            )
        };
        let url = reqwest::Url::parse(value).map_err(|_| invalid())?;
        let loopback = url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        anyhow::ensure!(
            value.len() <= 2048
                && url.as_str().len() <= 2048
                && !value.chars().any(|c| c.is_whitespace() || c.is_control())
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && (url.scheme() == "https" || (development && loopback && url.scheme() == "http")),
            "{}",
            invalid()
        );
        Ok(url.to_string())
    }
    /// Saving validates only approval; it never resolves DNS or signing bytes.
    pub(crate) fn approved_url(&self, value: &str) -> Option<String> {
        let canonical = Self::canonical(value, self.development).ok()?;
        self.urls.contains(&canonical).then_some(canonical)
    }
}
fn env_list(name: &str) -> Result<Vec<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect()),
        Err(std::env::VarError::NotPresent) => Ok(Vec::new()),
        Err(_) => bail!("{name} is not valid Unicode"),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    Development,
    Production,
}

impl Environment {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::Production => "production",
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL is required")?;
        let listen = std::env::var("GATEWAY_LISTEN")
            .unwrap_or_else(|_| "127.0.0.1:8080".into())
            .parse()
            .context("GATEWAY_LISTEN must be an IP address and port")?;
        let environment = match std::env::var("GATEWAY_ENV").as_deref() {
            Ok("development") => Environment::Development,
            Ok("production") | Err(std::env::VarError::NotPresent) => Environment::Production,
            _ => bail!("GATEWAY_ENV must be development or production"),
        };
        let inference_base_url = match std::env::var("GATEWAY_INFERENCE_BASE_URL") {
            Ok(value) => InferenceBaseUrl::parse(Some(&value), environment)?,
            Err(std::env::VarError::NotPresent) => InferenceBaseUrl::default(),
            Err(_) => bail!("GATEWAY_INFERENCE_BASE_URL is not valid Unicode"),
        };
        let web_directory = std::env::var_os("GATEWAY_WEB_DIR").map(PathBuf::from);
        if web_directory
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            bail!("GATEWAY_WEB_DIR must be unset or a non-empty directory path");
        }
        let secret_env_allowlist: Vec<String> = std::env::var("GATEWAY_SECRET_ENV_ALLOWLIST")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect();
        let smtp_env_allowlist: Vec<String> = std::env::var(crate::email::SMTP_ALLOWLIST_VAR)
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect();
        // Validate metrics before generic allowlist checks: its diagnostics
        // name only the setting, never the selected secret variable or value.
        let metrics_export = MetricsExportConfig::from_env(environment)?;
        let webhook_names = env_list("GATEWAY_ALERT_WEBHOOK_SECRET_ENV_ALLOWLIST")?;
        disjoint_allowlists(
            &secret_env_allowlist,
            &smtp_env_allowlist,
            &webhook_names,
            &|name| std::env::var(name).ok(),
        )?;
        let webhook_urls = env_list("GATEWAY_ALERT_WEBHOOK_URL_ALLOWLIST")?;
        let alert_webhooks = AlertWebhookPolicy::parse(&webhook_urls, webhook_names, environment)?;
        // After the allowlist checks, so a trace pointer that a provider or SMTP
        // allowlist could read is reported like every other pointer: by setting
        // and variable name, never value. Trace errors themselves stay generic.
        let traces = TraceConfig::from_env(environment)?;
        let max_concurrent = std::env::var("GATEWAY_MAX_CONCURRENT_REQUESTS")
            .unwrap_or_else(|_| "128".into())
            .parse::<usize>()
            .context("GATEWAY_MAX_CONCURRENT_REQUESTS must be an integer")?;
        let seconds = std::env::var("GATEWAY_REQUEST_TIMEOUT_SECONDS")
            .unwrap_or_else(|_| "120".into())
            .parse::<u64>()
            .context("GATEWAY_REQUEST_TIMEOUT_SECONDS must be an integer")?;
        anyhow::ensure!(
            (1..=10_000).contains(&max_concurrent) && (1..=3600).contains(&seconds),
            "inference limits out of range"
        );
        let inference_limits = crate::inference::EngineLimits {
            max_concurrent,
            request_timeout: Duration::from_secs(seconds),
            workloads: crate::inference::workload::WorkloadLimits::from_lookup(|name| {
                std::env::var(name).ok()
            })?,
            audio: crate::inference::audio::AudioLimits::from_lookup(|name| {
                std::env::var(name).ok()
            })?,
        };
        let metrics_listen = crate::metrics::listen_from(
            std::env::var("GATEWAY_METRICS_ADDR").ok().as_deref(),
            listen,
        )?;
        let database_max_connections = std::env::var("GATEWAY_DATABASE_MAX_CONNECTIONS")
            .unwrap_or_else(|_| "10".into())
            .parse::<u32>()
            .ok()
            .filter(|n| (2..=500).contains(n))
            .context("GATEWAY_DATABASE_MAX_CONNECTIONS must be an integer from 2 to 500")?;
        let reporting_database_url = match std::env::var("GATEWAY_REPORTING_DATABASE_URL") {
            Ok(url) if url.trim().is_empty() => {
                bail!("GATEWAY_REPORTING_DATABASE_URL must be unset or a PostgreSQL URL")
            }
            Ok(url) => {
                // Validate now (never echo the value: it may hold credentials).
                <sqlx::postgres::PgConnectOptions as std::str::FromStr>::from_str(&url).map_err(
                    |_| {
                        anyhow::anyhow!(
                            "GATEWAY_REPORTING_DATABASE_URL is not a valid PostgreSQL URL"
                        )
                    },
                )?;
                Some(url)
            }
            Err(std::env::VarError::NotPresent) => None,
            Err(_) => bail!("GATEWAY_REPORTING_DATABASE_URL is not valid Unicode"),
        };
        let reporting_max_lag = std::env::var("GATEWAY_REPORTING_MAX_LAG_SECONDS")
            .unwrap_or_else(|_| "30".into())
            .parse::<u64>()
            .ok()
            .filter(|n| (1..=3600).contains(n))
            .map(Duration::from_secs)
            .context("GATEWAY_REPORTING_MAX_LAG_SECONDS must be an integer from 1 to 3600")?;
        let admission_mode = crate::governance::locks::AdmissionMode::from_env()?;
        let listen_database_url = match std::env::var("GATEWAY_LISTEN_DATABASE_URL") {
            Ok(url) if url.trim().is_empty() => {
                bail!("GATEWAY_LISTEN_DATABASE_URL must be unset or a PostgreSQL URL")
            }
            Ok(url) => {
                <sqlx::postgres::PgConnectOptions as std::str::FromStr>::from_str(&url).map_err(
                    |_| {
                        anyhow::anyhow!("GATEWAY_LISTEN_DATABASE_URL is not a valid PostgreSQL URL")
                    },
                )?;
                Some(url)
            }
            Err(std::env::VarError::NotPresent) => None,
            Err(_) => bail!("GATEWAY_LISTEN_DATABASE_URL is not valid Unicode"),
        };
        let config_cache = match std::env::var("GATEWAY_CONFIG_CACHE").as_deref() {
            Ok("on") | Err(std::env::VarError::NotPresent) => true,
            Ok("off") => false,
            _ => bail!("GATEWAY_CONFIG_CACHE must be on or off"),
        };
        Ok(Self {
            database_url,
            listen,
            environment,
            web_directory,
            inference_base_url,
            secret_env_allowlist,
            smtp_env_allowlist,
            alert_webhooks,
            inference_limits,
            metrics_listen,
            metrics_export,
            database_max_connections,
            reporting_database_url,
            reporting_max_lag,
            admission_mode,
            listen_database_url,
            config_cache,
            traces,
        })
    }

    /// The dedicated LISTEN pool (one session connection, connected lazily
    /// and reconnected by `crate::notify`), or `None` when caching is off.
    pub fn connect_listener(&self) -> Result<Option<sqlx::PgPool>> {
        if !self.config_cache {
            return Ok(None);
        }
        let url = self
            .listen_database_url
            .as_ref()
            .unwrap_or(&self.database_url);
        Ok(Some(
            PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(Duration::from_secs(3))
                .connect_lazy(url)
                .map_err(|_| {
                    anyhow::anyhow!("the LISTEN database URL is not a valid PostgreSQL URL")
                })?,
        ))
    }

    pub async fn connect(&self) -> Result<sqlx::PgPool> {
        PgPoolOptions::new()
            .max_connections(self.database_max_connections)
            .acquire_timeout(Duration::from_secs(3))
            .connect(&self.database_url)
            .await
            .context("could not connect to PostgreSQL")
    }

    /// The primary pool without an initial connection (`wait-for-database`).
    pub fn connect_lazy(&self) -> Result<sqlx::PgPool> {
        PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(3))
            .connect_lazy(&self.database_url)
            .context("DATABASE_URL is not a valid PostgreSQL URL")
    }

    /// The optional reporting pool, connected lazily: an unreachable replica
    /// never blocks startup or readiness; reads fall back to the primary.
    pub fn connect_reporting(&self) -> Result<Option<sqlx::PgPool>> {
        let Some(url) = &self.reporting_database_url else {
            return Ok(None);
        };
        Ok(Some(
            PgPoolOptions::new()
                .max_connections(self.database_max_connections)
                // Short: a missing replica costs at most this before the primary fallback.
                .acquire_timeout(Duration::from_secs(1))
                .connect_lazy(url)
                .map_err(|_| {
                    anyhow::anyhow!("GATEWAY_REPORTING_DATABASE_URL is not a valid PostgreSQL URL")
                })?,
        ))
    }
}

/// Each transport has a dedicated secret boundary, never a shared variable.
fn disjoint_allowlists(
    providers: &[String],
    smtp: &[String],
    webhooks: &[String],
    lookup: &impl Fn(&str) -> Option<String>,
) -> anyhow::Result<()> {
    let shared: Vec<&str> = smtp
        .iter()
        .filter(|name| providers.contains(name))
        .map(String::as_str)
        .collect();
    let mut failures = Vec::new();
    if !shared.is_empty() {
        failures.push(format!("GATEWAY_SECRET_ENV_ALLOWLIST and {} both list {}: a variable may be readable by provider connections or by SMTP, not both", crate::email::SMTP_ALLOWLIST_VAR, shared.join(",")));
    }
    for (setting, names) in [
        ("GATEWAY_SECRET_ENV_ALLOWLIST", providers),
        (crate::email::SMTP_ALLOWLIST_VAR, smtp),
        ("GATEWAY_ALERT_WEBHOOK_SECRET_ENV_ALLOWLIST", webhooks),
    ] {
        if setting == "GATEWAY_ALERT_WEBHOOK_SECRET_ENV_ALLOWLIST"
            && names
                .iter()
                .any(|name| providers.contains(name) || smtp.contains(name))
        {
            failures.push(
                "GATEWAY_ALERT_WEBHOOK_SECRET_ENV_ALLOWLIST overlaps provider or SMTP allowlists"
                    .to_owned(),
            );
        }
        let reserved: Vec<String> = names
            .iter()
            .map(String::as_str)
            .filter(|name| {
                crate::providers::secrets::reserved_env_name(name, lookup)
                    || (setting != "GATEWAY_SECRET_ENV_ALLOWLIST"
                        && crate::startup::SECRET_NAMES.contains(name))
            })
            .map(|name| {
                let pointers = crate::providers::secrets::selecting_secret_pointers(name, lookup);
                if pointers.is_empty() {
                    name.to_owned()
                } else {
                    format!("{name} (selected by {})", pointers.join(","))
                }
            })
            .collect();
        if !reserved.is_empty() {
            failures.push(format!(
                "{setting} lists reserved variables: {}",
                reserved.join(",")
            ));
        }
    }
    anyhow::ensure!(failures.is_empty(), "{}", failures.join("; "));
    let provider_keys: Vec<&str> = providers
        .iter()
        .map(String::as_str)
        .filter(|name| matches!(*name, "OPENAI_API_KEY" | "ANTHROPIC_API_KEY"))
        .collect();
    if !provider_keys.is_empty() {
        tracing::warn!(setting = "GATEWAY_SECRET_ENV_ALLOWLIST", names = %provider_keys.join(","), "built-in provider keys remain referenceable; a custom-endpoint connection using these references receives their credentials");
    }
    Ok(())
}

#[cfg(test)]
#[path = "metrics_export_config_tests.rs"]
mod metrics_export_config_tests;

#[cfg(test)]
mod allowlist_tests {
    #[test]
    fn webhook_url_approval_uses_canonical_equality_and_refuses_unsafe_inputs() {
        use super::{
            AlertWebhookPolicy,
            Environment::{Development, Production},
        };
        let policy = AlertWebhookPolicy::parse(
            &["https://EXAMPLE.test:443/hook".into()],
            vec!["HOOK".into()],
            Production,
        )
        .unwrap();
        assert_eq!(
            policy.approved_url("https://example.test/hook").as_deref(),
            Some("https://example.test/hook")
        );
        for input in [
            "https://example.test/hook/extra",
            "https://example.test/hook?x=1",
            "https://example.test/hook#fragment",
            "https://user:pass@example.test/hook",
            "https://example.test.evil/hook",
        ] {
            assert!(policy.approved_url(input).is_none());
        }
        for input in [
            "http://127.0.0.1/hook",
            "http://localhost/hook",
            "https://u:p@example.test/",
            "https://example.test/?",
            "https://example.test/#",
            "https://example.test/\n",
            "ftp://example.test/",
        ] {
            assert!(AlertWebhookPolicy::parse(&[input.into()], vec![], Production).is_err());
        }
        for input in [
            "http://127.0.0.1/hook",
            "http://[::1]/hook",
            "http://localhost/hook",
        ] {
            assert!(AlertWebhookPolicy::parse(&[input.into()], vec![], Development).is_ok());
        }
        assert!(
            AlertWebhookPolicy::parse(&["http://192.168.1.1/hook".into()], vec![], Development)
                .is_err()
        );
        assert!(policy.secrets.allows("env:HOOK"));
        assert!(!policy.secrets.allows("file:/secret"));
        assert!(!policy.secrets.allows("env:OTHER"));
        for pointer in crate::providers::secrets::SECRET_POINTERS {
            assert!(
                super::disjoint_allowlists(&[], &[], &["MATERIAL".into()], &|key| (key
                    == *pointer)
                    .then(|| "MATERIAL".into()))
                .is_err()
            );
        }
    }

    #[test]
    fn webhook_secret_allowlist_is_disjoint_and_reserved() {
        assert!(
            super::disjoint_allowlists(&["HOOK".into()], &[], &["HOOK".into()], &|_| None).is_err()
        );
        assert!(
            super::disjoint_allowlists(&[], &["HOOK".into()], &["HOOK".into()], &|_| None).is_err()
        );
        for name in [
            "DATABASE_URL",
            "PGPASSWORD",
            "OPENAI_API_KEY",
            "GATEWAY_OWNED",
        ] {
            assert!(super::disjoint_allowlists(&[], &[], &[name.into()], &|_| None).is_err());
        }
    }
    #[test]
    fn trace_configuration_checks_environment_and_dedicated_sensitive_headers() {
        use super::{Environment, TraceConfig};
        let parse = |url: &str, environment| {
            TraceConfig::from_lookup(
                |key| (key == TraceConfig::URL).then(|| url.into()),
                environment,
            )
        };
        assert!(parse("http://127.0.0.1:4318/v1/traces", Environment::Production).is_err());
        assert!(parse("http://localhost:4318/v1/traces", Environment::Production).is_err());
        assert!(parse("https://8.8.8.8/v1/traces", Environment::Production).is_ok());
        for ratio in ["0", "1", "0.01"] {
            assert!(
                TraceConfig::from_lookup(
                    |key| (key == TraceConfig::RATIO).then(|| ratio.into()),
                    Environment::Production
                )
                .is_ok()
            );
        }
        for name in [
            "",
            "lowercase",
            "AWS_SECRET",
            "GATEWAY_SECRET",
            "DATABASE_URL",
            "OTEL_EXPORTER_OTLP_HEADERS",
            "PGPASSWORD",
        ] {
            assert!(
                TraceConfig::from_lookup(
                    |key| match key {
                        TraceConfig::URL => Some("https://8.8.8.8/v1/traces".into()),
                        TraceConfig::HEADERS => Some(name.into()),
                        key if key == name => Some("X=secret".into()),
                        _ => None,
                    },
                    Environment::Production
                )
                .is_err()
            );
        }
        let config = TraceConfig::from_lookup(
            |key| match key {
                TraceConfig::URL => Some("https://8.8.8.8/v1/traces".into()),
                TraceConfig::HEADERS => Some("TRACE_MATERIAL".into()),
                "TRACE_MATERIAL" => Some("Authorization=Bearer secret".into()),
                _ => None,
            },
            Environment::Production,
        )
        .unwrap();
        assert!(config.headers["authorization"].is_sensitive());
        assert!(!format!("{:?}", config.headers).contains("secret"));
    }

    #[test]
    fn database_proxy_and_telemetry_names_are_reserved_in_both_lists() {
        for name in [
            "PGPASSWORD",
            "PGSSLKEY",
            "PGPASSFILE",
            "PGFUTURE_OPTION",
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
            "OTEL_EXPORTER_OTLP_HEADERS",
        ] {
            for (providers, smtp) in [
                (vec![name.to_owned()], vec![]),
                (vec![], vec![name.to_owned()]),
            ] {
                assert!(
                    super::disjoint_allowlists(&providers, &smtp, &[], &|_| None).is_err(),
                    "operational credential name must be reserved"
                );
            }
        }
    }

    #[test]
    fn startup_reports_both_failing_lists_and_the_selecting_pointer() {
        let error = super::disjoint_allowlists(
            &["DATABASE_URL".into()],
            &["PRIVATE_MATERIAL".into()],
            &[],
            &|name| (name == "GATEWAY_FILE_ENCRYPTION_KEYS_ENV").then(|| "PRIVATE_MATERIAL".into()),
        )
        .unwrap_err()
        .to_string();
        for name in [
            "GATEWAY_SECRET_ENV_ALLOWLIST",
            "GATEWAY_SMTP_SECRET_ENV_ALLOWLIST",
            "DATABASE_URL",
            "PRIVATE_MATERIAL",
            "GATEWAY_FILE_ENCRYPTION_KEYS_ENV",
        ] {
            assert!(error.contains(name), "missing diagnostic name");
        }
    }

    #[test]
    fn provider_allowlist_refuses_reserved_names_at_startup() {
        for name in [
            "DATABASE_URL",
            "DATABASE_URL_FILE",
            "GATEWAY_OIDC_CLIENT_SECRET",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            let error = super::disjoint_allowlists(&[name.to_owned()], &[], &[], &|_| None)
                .expect_err("reserved provider allowlist must stop startup")
                .to_string();
            assert!(error.contains("GATEWAY_SECRET_ENV_ALLOWLIST"));
            assert!(error.contains(name));
        }
    }

    #[test]
    fn both_allowlists_refuse_every_indirect_secret_name() {
        for pointer in crate::providers::secrets::SECRET_POINTERS {
            let lookup = |setting: &str| {
                assert!(crate::providers::secrets::SECRET_POINTERS.contains(&setting));
                (setting == *pointer).then(|| "PRIVATE_MATERIAL".to_owned())
            };
            for (providers, smtp, setting) in [
                (
                    vec!["PRIVATE_MATERIAL".to_owned()],
                    vec![],
                    "GATEWAY_SECRET_ENV_ALLOWLIST",
                ),
                (
                    vec![],
                    vec!["PRIVATE_MATERIAL".to_owned()],
                    crate::email::SMTP_ALLOWLIST_VAR,
                ),
            ] {
                let error = super::disjoint_allowlists(&providers, &smtp, &[], &lookup)
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(setting));
                assert!(error.contains("PRIVATE_MATERIAL"));
                assert!(error.contains(pointer));
            }
        }
        super::disjoint_allowlists(
            &["OPENAI_API_KEY".into(), "ANTHROPIC_API_KEY".into()],
            &["SMTP_PASSWORD".into()],
            &[],
            &|_| None,
        )
        .unwrap();
    }

    #[test]
    fn built_in_provider_keys_remain_referenceable() {
        // Log level, names and custom-endpoint warning are asserted in the
        // clean-process startup integration test, not a thread-local tracing
        // subscriber racing other tests' global callsite interest caches.
        let names = ["OPENAI_API_KEY".to_owned(), "ANTHROPIC_API_KEY".to_owned()];
        let lookup = |name: &str| {
            assert!(
                crate::providers::secrets::SECRET_POINTERS.contains(&name),
                "must not read sensitive values"
            );
            None
        };
        super::disjoint_allowlists(&names, &[], &[], &lookup).unwrap();
        let resolver = crate::providers::secrets::EnvSecrets::from_lookup(names, lookup);
        assert!(resolver.allows("env:OPENAI_API_KEY"));
        assert!(resolver.allows("env:ANTHROPIC_API_KEY"));
    }

    #[test]
    fn inference_base_url_valid_invalid_and_unset() {
        use super::{
            Environment::{Development, Production},
            InferenceBaseUrl,
        };
        assert!(
            InferenceBaseUrl::parse(None, Production)
                .unwrap()
                .0
                .is_none()
        );
        for (input, expected, env) in [
            (
                "https://api.bitop.dev/v1",
                "https://api.bitop.dev/v1",
                Production,
            ),
            (
                "https://API.bitop.dev:443/prefix/v1/",
                "https://api.bitop.dev/prefix/v1",
                Production,
            ),
            (
                "http://localhost:8080/v1",
                "http://localhost:8080/v1",
                Development,
            ),
            ("http://127.0.0.1/v1/", "http://127.0.0.1/v1", Development),
            ("http://[::1]/v1", "http://[::1]/v1", Development),
        ] {
            assert_eq!(
                InferenceBaseUrl::parse(Some(input), env)
                    .unwrap()
                    .0
                    .as_deref(),
                Some(expected)
            );
        }
        for input in [
            "",
            "/v1",
            "//api.bitop.dev/v1",
            "https://api.bitop.dev",
            "https://api.bitop.dev/v10",
            "https://api.bitop.dev/v1//",
            "http://api.bitop.dev/v1",
            "ftp://localhost/v1",
            "https://user:password@api.bitop.dev/v1",
            "https://api.bitop.dev/v1?",
            "https://api.bitop.dev/v1#",
            " https://api.bitop.dev/v1",
        ] {
            for env in [Development, Production] {
                let error = InferenceBaseUrl::parse(Some(input), env)
                    .err()
                    .expect("invalid URL");
                assert!(error.to_string().contains("GATEWAY_INFERENCE_BASE_URL"));
            }
        }
        assert!(InferenceBaseUrl::parse(Some("http://localhost/v1"), Production).is_err());
    }

    /// #140: one variable must never be readable by both SMTP and provider
    /// connections, in either direction.
    #[test]
    fn overlapping_secret_allowlists_are_refused() {
        let providers = vec!["OPENAI_API_KEY".to_owned(), "SHARED".to_owned()];
        let error = super::disjoint_allowlists(
            &providers,
            &["SMTP_PASSWORD".into(), "SHARED".into()],
            &[],
            &|_| None,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("SHARED"), "{error}");
        assert!(!error.contains("OPENAI_API_KEY"));
        super::disjoint_allowlists(&providers, &["SMTP_PASSWORD".into()], &[], &|_| None).unwrap();
    }

    /// Credentials the gateway reads outside the provider allowlist (database,
    /// OIDC, the built-in provider keys, Bedrock's AWS chain, gateway settings)
    /// can never be named on the SMTP list either.
    #[test]
    fn smtp_allowlist_refuses_the_gateways_own_credentials() {
        for name in [
            "DATABASE_URL",
            "GATEWAY_OIDC_CLIENT_SECRET",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "GATEWAY_LISTEN_DATABASE_URL",
        ] {
            let error = super::disjoint_allowlists(&[], &[name.to_owned()], &[], &|_| None)
                .expect_err(name)
                .to_string();
            assert!(error.contains(name), "{error}");
        }
    }
}
