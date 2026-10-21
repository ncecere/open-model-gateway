use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use sqlx::postgres::PgPoolOptions;

/// Deliberately not Debug: DATABASE_URL may contain credentials.
pub struct Config {
    pub database_url: String,
    pub listen: SocketAddr,
    pub environment: Environment,
    pub web_directory: Option<PathBuf>,
    pub secret_env_allowlist: Vec<String>,
    pub smtp_env_allowlist: Vec<String>,
    pub inference_limits: crate::inference::EngineLimits,
    /// Optional separate Prometheus listener (`GATEWAY_METRICS_ADDR`); `None` disables it.
    pub metrics_listen: Option<SocketAddr>,
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
        disjoint_allowlists(&secret_env_allowlist, &smtp_env_allowlist, &|name| {
            std::env::var(name).ok()
        })?;
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
            secret_env_allowlist,
            smtp_env_allowlist,
            inference_limits,
            metrics_listen,
            database_max_connections,
            reporting_database_url,
            reporting_max_lag,
            admission_mode,
            listen_database_url,
            config_cache,
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

/// A variable is readable by provider connections or by SMTP, never both:
/// either side's destination is chosen by a Platform Admin (#140).
fn disjoint_allowlists(
    providers: &[String],
    smtp: &[String],
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
    ] {
        let reserved: Vec<String> = names
            .iter()
            .map(String::as_str)
            .filter(|name| {
                crate::providers::secrets::reserved_env_name(name, lookup)
                    || (setting == crate::email::SMTP_ALLOWLIST_VAR
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
mod allowlist_tests {
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
                    super::disjoint_allowlists(&providers, &smtp, &|_| None).is_err(),
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
            let error = super::disjoint_allowlists(&[name.to_owned()], &[], &|_| None)
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
                let error = super::disjoint_allowlists(&providers, &smtp, &lookup)
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
        super::disjoint_allowlists(&names, &[], &lookup).unwrap();
        let resolver = crate::providers::secrets::EnvSecrets::from_lookup(names, lookup);
        assert!(resolver.allows("env:OPENAI_API_KEY"));
        assert!(resolver.allows("env:ANTHROPIC_API_KEY"));
    }

    /// #140: one variable must never be readable by both SMTP and provider
    /// connections, in either direction.
    #[test]
    fn overlapping_secret_allowlists_are_refused() {
        let providers = vec!["OPENAI_API_KEY".to_owned(), "SHARED".to_owned()];
        let error = super::disjoint_allowlists(
            &providers,
            &["SMTP_PASSWORD".into(), "SHARED".into()],
            &|_| None,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("SHARED"), "{error}");
        assert!(!error.contains("OPENAI_API_KEY"));
        super::disjoint_allowlists(&providers, &["SMTP_PASSWORD".into()], &|_| None).unwrap();
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
            let error = super::disjoint_allowlists(&[], &[name.to_owned()], &|_| None)
                .expect_err(name)
                .to_string();
            assert!(error.contains(name), "{error}");
        }
    }
}
