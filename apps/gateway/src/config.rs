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
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    Development,
    Production,
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
        };
        Ok(Self {
            database_url,
            listen,
            environment,
            web_directory,
            secret_env_allowlist,
            inference_limits,
        })
    }

    pub async fn connect(&self) -> Result<sqlx::PgPool> {
        PgPoolOptions::new()
            .max_connections(10)
            .acquire_timeout(Duration::from_secs(3))
            .connect(&self.database_url)
            .await
            .context("could not connect to PostgreSQL")
    }
}
