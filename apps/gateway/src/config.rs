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
        let secret_env_allowlist = std::env::var("GATEWAY_SECRET_ENV_ALLOWLIST")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect();
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
        Ok(Self {
            database_url,
            listen,
            environment,
            web_directory,
            secret_env_allowlist,
            inference_limits,
            metrics_listen,
            database_max_connections,
            reporting_database_url,
            reporting_max_lag,
        })
    }

    pub async fn connect(&self) -> Result<sqlx::PgPool> {
        PgPoolOptions::new()
            .max_connections(self.database_max_connections)
            .acquire_timeout(Duration::from_secs(3))
            .connect(&self.database_url)
            .await
            .context("could not connect to PostgreSQL")
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
