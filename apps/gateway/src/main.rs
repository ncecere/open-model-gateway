#![deny(unsafe_code)]
// Structure ratchet (#499, docs/verification.md): no new long or tangled function
// in the non-test build. Today's offenders carry an `expect` that names #499;
// remove the marker when the function is fixed. Never `allow`, never a threshold.
#![cfg_attr(not(test), warn(clippy::too_many_lines, clippy::cognitive_complexity))]

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use open_model_gateway::{config::Config, providers::secrets::EnvSecrets, store::Store};
use tracing_subscriber::EnvFilter;

mod commands;

#[derive(Parser)]
#[command(version, about = "Single-enterprise model gateway")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the gateway HTTP server (default).
    Serve,
    /// Apply database migrations explicitly, before starting replicas.
    Migrate,
    /// Wait until PostgreSQL accepts connections (and, with --schema, until this
    /// binary's schema and grants are in place), then exit 0. For init containers.
    WaitForDatabase {
        /// Also wait until the schema exactly matches this binary and is readable.
        #[arg(long)]
        schema: bool,
        /// Give up (exit 1) after this many seconds.
        #[arg(long, default_value_t = 600)]
        timeout_seconds: u64,
    },
    /// Runtime-role privileges (deploy/staging/runtime-grants.sql, embedded).
    Grants {
        #[command(subcommand)]
        action: GrantsCommand,
    },
    /// Provision local development workspaces and print new keys once.
    BootstrapDev,
    /// Seed fixed local-demo personas and disabled sample configuration in gateway_demo only.
    BootstrapDemo {
        /// Add only missing new personas to an active demo; never reset existing identities.
        #[arg(long)]
        add_missing_personas: bool,
    },
    /// Close a bounded batch of expired executions without refunding unknown cost.
    ReconcileExecutions {
        #[arg(long, default_value_t = 100)]
        limit: i64,
    },
    /// Compact settled execution display metadata; retain pricing, usage and ledger.
    CompactHistory {
        #[arg(long)]
        older_than_days: i32,
        #[arg(long, default_value_t = 1000)]
        limit: i64,
    },
    /// Print the migrations embedded in this binary as JSON (no database access);
    /// backup/restore tooling compares it with a backup's recorded lineage.
    SchemaVersion,
    /// Alert rules (docs/alerts.md).
    Alerts {
        #[command(subcommand)]
        action: AlertsCommand,
    },
    /// Budget accounting maintenance (docs/operations.md).
    Budget {
        #[command(subcommand)]
        action: BudgetCommand,
    },
    /// Request history consistency (docs/operations.md "MultiXacts and history parent checks").
    History {
        #[command(subcommand)]
        action: HistoryCommand,
    },
    /// Monthly history partitions (docs/operations.md "History partitions").
    Partitions {
        #[command(subcommand)]
        action: PartitionsCommand,
    },
    /// Operator-only archival of closed history months (schema owner credentials).
    Archive {
        #[command(subcommand)]
        action: ArchiveCommand,
    },
    /// Hourly usage rollups (`serve` maintains them every minute).
    Rollups {
        #[command(subcommand)]
        action: RollupsCommand,
    },
    /// Encrypted file store maintenance (docs/file-storage.md).
    Files {
        #[command(subcommand)]
        action: FilesCommand,
    },
    /// Explicitly provision an email for first OIDC linking; requires trusted database access.
    ProvisionUser {
        #[arg(long)]
        email: String,
        #[arg(long)]
        platform_admin: bool,
    },
    /// Probe the readiness endpoint and exit 0 when healthy, 1 otherwise (the
    /// container HEALTHCHECK; no shell or curl). Reads no configuration or secrets.
    Healthcheck {
        /// Plain http:// URL to probe [default: http://127.0.0.1:<port of
        /// GATEWAY_LISTEN>/health/ready]. Proxies and redirects are never used.
        #[arg(long)]
        url: Option<String>,
        /// Overall timeout, such as 3s or 500ms (at most 60s).
        #[arg(long, default_value = open_model_gateway::healthcheck::DEFAULT_TIMEOUT)]
        timeout: String,
        /// Permit a destination that is not a loopback address.
        #[arg(long)]
        allow_non_loopback: bool,
    },
}

#[derive(Subcommand)]
enum GrantsCommand {
    /// Apply the reviewed runtime grants as the migrator; run after every `migrate`.
    Apply,
    /// Check the runtime role's privileges with rollback-only probes (needs a role that can SET ROLE gateway_runtime).
    Verify,
}

#[derive(Subcommand)]
enum AlertsCommand {
    /// Evaluate every alert rule once and send pending alert email (`serve` runs this on an interval).
    Evaluate {
        /// Required: run one evaluation and exit.
        #[arg(long)]
        once: bool,
    },
}

#[derive(Subcommand)]
enum FilesCommand {
    /// Read-only: compare `stored_files` metadata with the configured store;
    /// exits nonzero on missing objects, size or backend mismatches, or unknown key ids.
    Verify {
        #[arg(long, default_value_t = 100_000)]
        limit: i64,
    },
    /// Re-wrap every live file's data key from one master key to another so
    /// the old key can be removed (#35). Contents are not re-encrypted.
    /// Resumable; exits nonzero while any file still uses the old key.
    Rekey {
        /// The key id to retire (still configured, decrypt-only).
        #[arg(long)]
        from: String,
        /// The key id to move to (normally the active, first key).
        #[arg(long)]
        to: String,
        /// Count only; change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Most files to process in this run.
        #[arg(long, default_value_t = 1_000_000)]
        limit: i64,
    },
    /// Delete expired, pending-deletion and abandoned objects once (`serve` sweeps every minute).
    Sweep {
        /// Required: run one sweep and exit.
        #[arg(long)]
        once: bool,
        #[arg(long, default_value_t = 1000)]
        limit: i64,
    },
}

#[derive(Subcommand)]
enum PartitionsCommand {
    /// Create missing month partitions up to --ahead months after the current
    /// one and print coverage; exits nonzero below 2 future months.
    Ensure {
        #[arg(long, default_value_t = open_model_gateway::partitions::DEFAULT_AHEAD)]
        ahead: i32,
    },
    /// Read-only: print partition coverage; exits nonzero below 2 future months.
    Status,
    /// Before upgrading to partitioned history: build the new keys online
    /// (CREATE INDEX CONCURRENTLY) so the migration only validates and swaps.
    Prepare,
}

#[derive(Subcommand)]
enum ArchiveCommand {
    /// Export one closed month of a group (history, audit or storage) with a
    /// checksummed manifest, then detach it (kept in schema omg_archive, or
    /// dropped with --drop). Refuses months with pending/unknown cost or
    /// within retention. Requires the schema owner.
    Partition {
        /// history (executions, reservations, ledger), audit or storage.
        group: String,
        /// YYYY-MM, or `legacy` (rows from before partitioning).
        month: String,
        /// Directory receiving <group>-<month>-<id>/.
        #[arg(long)]
        to: std::path::PathBuf,
        /// Drop the detached partitions instead of keeping them in omg_archive.
        #[arg(long)]
        drop: bool,
        /// Months kept hot [default: GATEWAY_HISTORY_RETENTION_MONTHS or 25].
        #[arg(long)]
        retention_months: Option<u32>,
    },
    /// Re-check an archive directory's manifest and file checksums (no database).
    Verify { directory: std::path::PathBuf },
}

#[derive(Subcommand)]
enum RollupsCommand {
    /// Roll due hours once (catch-up after an upgrade); prints a JSON report.
    Run {
        #[arg(long)]
        once: bool,
        #[arg(long, default_value_t = 300)]
        budget_seconds: u64,
    },
}

#[derive(Subcommand)]
enum HistoryCommand {
    /// Read-only (one snapshot): find executions and reservations whose
    /// parents (workspace key, deployment, cost center, batch job, price
    /// version) are missing or belong to another scope, and check that the
    /// 0034 enforcement is intact; exits nonzero on any finding. A full,
    /// clean run resolves the built-in `history_orphans` incident; findings
    /// open it.
    Verify {
        /// Only history admitted at or after this RFC 3339 time (default: all).
        #[arg(long)]
        since: Option<String>,
    },
}

#[derive(Subcommand)]
enum BudgetCommand {
    /// Read-only: compare maintained budget totals with a full scan of
    /// reservations and executions in one snapshot; exits nonzero on any mismatch.
    Verify,
}

fn main() -> Result<()> {
    let process_start_unix_nano = open_model_gateway::telemetry::now_unix_nanos();
    // Arguments only: no clap argument reads the environment.
    let cli = Cli::parse();
    if let Some(Command::Healthcheck {
        url,
        timeout,
        allow_non_loopback,
    }) = &cli.command
    {
        return healthcheck(url.as_deref(), timeout, *allow_non_loopback);
    }
    // Former container entrypoint (docs/operations.md): private umask and
    // `*_FILE` secret import for the four well-known secrets, before any
    // `.env` file, configuration, logging or thread. Never migrates/bootstraps.
    open_model_gateway::startup::restrict_umask();
    // SAFETY: still single-threaded; no runtime or thread has been started.
    #[allow(unsafe_code)] // Single-threaded startup, before the runtime exists.
    unsafe { open_model_gateway::startup::import_secret_files() }?;
    if let Some(path) = std::env::var_os("GATEWAY_ENV_FILE") {
        dotenvy::from_path(path)
            .map_err(|_| anyhow::anyhow!("Could not load selected environment file"))?;
    } else {
        dotenvy::dotenv().ok();
    }
    if matches!(cli.command, Some(Command::SchemaVersion)) {
        println!("{}", schema_version()?);
        return Ok(());
    }
    if let Some(Command::Archive {
        action: ArchiveCommand::Verify { directory },
    }) = &cli.command
    {
        let manifest = open_model_gateway::archive::verify_directory(directory)?;
        println!("{}", serde_json::to_string_pretty(&manifest)?);
        return Ok(());
    }
    // Outbound address policy (#33), before any HTTP client exists.
    let egress = open_model_gateway::egress::Policy::from_env()?;
    open_model_gateway::egress::check_configured_endpoints(&egress, |name| {
        std::env::var(name).ok().filter(|v| !v.trim().is_empty())
    })?;
    open_model_gateway::egress::init(egress)?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cli, process_start_unix_nano))
}

fn healthcheck(url: Option<&str>, timeout: &str, allow_non_loopback: bool) -> Result<()> {
    use open_model_gateway::healthcheck::{Target, parse_timeout, probe};
    let timeout = parse_timeout(timeout)?;
    let target = match url {
        Some(url) => Target::parse(url)?,
        None => Target::from_listen(std::env::var("GATEWAY_LISTEN").ok().as_deref())?,
    };
    let status = probe(&target, timeout, allow_non_loopback).context("health check failed")?;
    println!("healthy (HTTP {status})");
    Ok(())
}

async fn wait_for_database(config: &Config, schema: bool, timeout_seconds: u64) -> Result<()> {
    let deadline = std::time::Instant::now()
        .checked_add(std::time::Duration::from_secs(timeout_seconds))
        .context("--timeout-seconds is too large")?;
    // Lazy: an unreachable or not-yet-created database is retried, not fatal.
    let store = Store::new(config.connect_lazy()?);
    loop {
        let ready = store.readiness().await;
        if ready.database && (!schema || ready.schema) {
            println!(
                "{}",
                if schema {
                    "Database schema is ready."
                } else {
                    "Database is reachable."
                }
            );
            return Ok(());
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "timed out after {timeout_seconds}s waiting for the database{}",
            if schema {
                " schema; run `open-model-gateway migrate` and `grants apply`"
            } else {
                ""
            }
        );
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// The log filter used when `RUST_LOG` is unset (or invalid): only this
/// crate, at `info`. #321 (reqwest's own DEBUG instrumentation, e.g.
/// "starting new connection", names the destination it's connecting to -
/// see docs/operations.md#outbound-connections): a single targeted
/// directive with no bare/global level enables nothing for any other
/// target, including `reqwest` and `hyper`, at any level - see
/// `default_log_filter_cannot_enable_dependency_debug_logs` below. An
/// operator who overrides `RUST_LOG` (e.g. `RUST_LOG=debug` or
/// `RUST_LOG=reqwest=debug`) can re-enable them; that must never be done in
/// production.
const DEFAULT_LOG_FILTER: &str = "open_model_gateway=info";

async fn run(cli: Cli, process_start_unix_nano: u64) -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER)),
        )
        .init();
    let config = Config::from_env()?;
    if matches!(cli.command, Some(Command::BootstrapDemo { .. }))
        || std::env::var("GATEWAY_OIDC_ISSUER").as_deref() == Ok("http://127.0.0.1:18084")
    {
        ensure_demo_config(&config)?;
    }
    if let Some(Command::WaitForDatabase {
        schema,
        timeout_seconds,
    }) = cli.command
    {
        return wait_for_database(&config, schema, timeout_seconds).await;
    }
    // Operator-approved local upstreams (#203): a refused approval stops
    // `serve` here, before any database connection, naming the approval and
    // GATEWAY_LOCAL_UPSTREAMS. Other commands never use the approvals.
    let mut local_approvals = matches!(cli.command, None | Some(Command::Serve))
        .then(|| {
            open_model_gateway::providers::local::endpoints::ApprovedEndpoints::from_env(
                config.environment.as_str(),
            )
        })
        .transpose()?;
    let pool = config.connect().await?;
    // Reports, usage and logs only (never admission, settlement or writes).
    let provider_secrets = EnvSecrets::new(config.secret_env_allowlist.clone());
    let smtp_secrets = EnvSecrets::new(config.smtp_env_allowlist.clone());
    let store = Store::new(pool.clone())
        .with_secret_allowlists(provider_secrets.clone(), smtp_secrets)
        .with_alert_webhooks(config.alert_webhooks.clone())
        .with_reporting(config.connect_reporting()?, config.reporting_max_lag)
        .with_admission_mode(config.admission_mode);
    commands::dispatch(
        cli.command.unwrap_or(Command::Serve),
        config,
        store,
        &pool,
        provider_secrets,
        &mut local_approvals,
        process_start_unix_nano,
    )
    .await?;
    pool.close().await;
    Ok(())
}

fn schema_version() -> Result<String> {
    let migrations: Vec<_> = open_model_gateway::store::MIGRATOR
        .iter()
        .map(|m| serde_json::json!({"version": m.version, "checksum": hex::encode(&m.checksum)}))
        .collect();
    let latest = open_model_gateway::store::MIGRATOR
        .iter()
        .map(|m| m.version)
        .max();
    Ok(serde_json::to_string(&serde_json::json!({
        "schema_family": "enterprise_v1",
        "latest_version": latest,
        "migrations": migrations,
    }))?)
}

fn ensure_demo_config(config: &Config) -> Result<()> {
    use std::str::FromStr;
    let options = sqlx::postgres::PgConnectOptions::from_str(&config.database_url)
        .context("invalid demo database configuration")?;
    anyhow::ensure!(
        config.environment == open_model_gateway::config::Environment::Development
            && config.listen.ip().is_loopback()
            && matches!(options.get_host(), "127.0.0.1" | "::1")
            && options.get_database() == Some(open_model_gateway::demo::DEMO_DATABASE),
        "local demo requires development mode, loopback binding/database, and the dedicated gateway_enterprise_demo database"
    );
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("could not register Ctrl-C handler")
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("could not register SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
    tracing::info!("shutdown requested");
}

#[cfg(test)]
mod demo {
    use super::*;

    #[test]
    fn upgrade_is_an_explicit_cli_option() {
        assert!(matches!(
            Cli::try_parse_from(["gateway", "bootstrap-demo"])
                .unwrap()
                .command,
            Some(Command::BootstrapDemo {
                add_missing_personas: false
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["gateway", "bootstrap-demo", "--add-missing-personas"])
                .unwrap()
                .command,
            Some(Command::BootstrapDemo {
                add_missing_personas: true
            })
        ));
        assert!(Cli::try_parse_from(["gateway", "serve", "--add-missing-personas"]).is_err());
    }

    #[test]
    fn schema_version_lists_embedded_lineage_without_a_database() {
        let value: serde_json::Value = serde_json::from_str(&schema_version().unwrap()).unwrap();
        let migrations = value["migrations"].as_array().unwrap();
        assert_eq!(
            value["latest_version"],
            migrations.last().unwrap()["version"]
        );
        assert_eq!(migrations[0]["version"], 1);
        // At least this release's newest migration; later releases add more.
        assert!(value["latest_version"].as_i64().unwrap() >= 65);
        assert!(migrations.iter().any(|m| m["version"] == 58));
        assert!(migrations.iter().any(|m| m["version"] == 59));
        assert!(migrations.iter().any(|m| m["version"] == 65));
        assert!(
            migrations
                .iter()
                .all(|m| m["checksum"].as_str().unwrap().len() == 96)
        );
    }

    /// #321: reqwest's own DEBUG instrumentation (and hyper's) names the
    /// destination it connects to - a diagnostic that bypasses the egress
    /// layer's per-purpose, destination-free logging policy entirely
    /// (docs/operations.md#outbound-connections). The production default
    /// (`RUST_LOG` unset) must never be able to show it: a single targeted
    /// `open_model_gateway=info` directive with no bare/global level
    /// enables nothing for `reqwest`/`hyper`, at any level, including
    /// `ERROR`. Only an operator who explicitly overrides `RUST_LOG` (never
    /// supported in production) can re-enable them.
    #[test]
    fn default_log_filter_cannot_enable_dependency_debug_logs() {
        #[derive(Clone, Default)]
        struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Captured {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
            type Writer = Captured;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let out = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(out.clone())
            .with_ansi(false)
            .with_env_filter(EnvFilter::new(DEFAULT_LOG_FILTER))
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();

        tracing::error!(target: "reqwest::connect", host = "collector.example.org", "starting new connection");
        tracing::error!(target: "hyper::client::connect", "test");
        // Sanity: this crate's own target at `info` does come through, so an
        // empty capture above means the dependency targets were filtered,
        // not that nothing in this subscriber is enabled at all.
        tracing::info!(target: "open_model_gateway::egress", "test");

        let logged = String::from_utf8_lossy(&out.0.lock().unwrap()).into_owned();
        assert!(
            logged.contains("open_model_gateway::egress"),
            "sanity check failed, the filter enabled nothing: {logged}"
        );
        for forbidden in ["reqwest", "hyper", "collector.example.org"] {
            assert!(
                !logged.contains(forbidden),
                "the default log filter must never enable dependency debug/error logs (found {forbidden:?}): {logged}"
            );
        }
    }

    #[test]
    fn alert_evaluation_is_an_explicit_one_shot_command() {
        assert!(matches!(
            Cli::try_parse_from(["gateway", "alerts", "evaluate", "--once"])
                .unwrap()
                .command,
            Some(Command::Alerts {
                action: AlertsCommand::Evaluate { once: true }
            })
        ));
        assert!(Cli::try_parse_from(["gateway", "alerts"]).is_err());
    }

    #[test]
    fn file_store_commands_are_explicit() {
        assert!(matches!(
            Cli::try_parse_from(["gateway", "files", "verify"])
                .unwrap()
                .command,
            Some(Command::Files {
                action: FilesCommand::Verify { limit: 100_000 }
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["gateway", "files", "sweep", "--once"])
                .unwrap()
                .command,
            Some(Command::Files {
                action: FilesCommand::Sweep {
                    once: true,
                    limit: 1000
                }
            })
        ));
        assert!(Cli::try_parse_from(["gateway", "files"]).is_err());
    }

    #[test]
    fn healthcheck_defaults_and_options() {
        let Some(Command::Healthcheck {
            url,
            timeout,
            allow_non_loopback,
        }) = Cli::try_parse_from(["gateway", "healthcheck"])
            .unwrap()
            .command
        else {
            panic!("healthcheck must parse");
        };
        assert_eq!(
            (url, timeout.as_str(), allow_non_loopback),
            (None, "3s", false)
        );
        assert!(
            Cli::try_parse_from([
                "gateway",
                "healthcheck",
                "--url",
                "http://127.0.0.1:8080/health/ready",
                "--timeout",
                "1s",
                "--allow-non-loopback",
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["gateway", "healthcheck", "extra"]).is_err());
    }

    #[test]
    fn budget_verify_is_an_explicit_subcommand() {
        assert!(matches!(
            Cli::try_parse_from(["gateway", "budget", "verify"])
                .unwrap()
                .command,
            Some(Command::Budget {
                action: BudgetCommand::Verify
            })
        ));
        assert!(Cli::try_parse_from(["gateway", "budget"]).is_err());
    }

    #[test]
    fn local_demo_guard_rejects_real_databases_and_nonlocal_bindings() {
        let mut config = Config {
            database_url: "postgres://gateway:gateway@127.0.0.1:54339/gateway_enterprise_demo"
                .into(),
            listen: "127.0.0.1:3000".parse().unwrap(),
            environment: open_model_gateway::config::Environment::Development,
            web_directory: None,
            inference_base_url: Default::default(),
            secret_env_allowlist: vec![],
            smtp_env_allowlist: vec![],
            alert_webhooks: Default::default(),
            inference_limits: Default::default(),
            metrics_listen: None,
            metrics_export: open_model_gateway::config::MetricsExportConfig::from_lookup(
                |_| None,
                open_model_gateway::config::Environment::Development,
            )
            .unwrap(),
            logs_export: open_model_gateway::config::LogsExportConfig::from_lookup(
                |_| None,
                open_model_gateway::config::Environment::Development,
            )
            .unwrap(),
            database_max_connections: 10,
            reporting_database_url: None,
            reporting_max_lag: std::time::Duration::from_secs(30),
            admission_mode: Default::default(),
            listen_database_url: None,
            config_cache: true,
            traces: open_model_gateway::config::TraceConfig::from_lookup(
                |_| None,
                open_model_gateway::config::Environment::Development,
            )
            .unwrap(),
        };
        assert!(ensure_demo_config(&config).is_ok());
        for url in [
            "postgres://gateway:gateway@127.0.0.1:54329/gateway",
            "postgres://gateway:gateway@database.example/gateway_enterprise_demo",
            "postgres://gateway:gateway@127.0.0.1:54339/gateway_enterprise_demo?host=database.example",
        ] {
            config.database_url = url.into();
            assert!(ensure_demo_config(&config).is_err());
        }
        config.database_url =
            "postgres://gateway:gateway@127.0.0.1:54339/gateway_enterprise_demo".into();
        config.listen = "0.0.0.0:3000".parse().unwrap();
        assert!(ensure_demo_config(&config).is_err());
        config.listen = "127.0.0.1:3000".parse().unwrap();
        config.environment = open_model_gateway::config::Environment::Production;
        assert!(ensure_demo_config(&config).is_err());
    }
}
