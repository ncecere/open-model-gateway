use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use open_model_gateway::{
    bootstrap,
    config::Config,
    http,
    identity::{IdentityConfig, IdentityState},
    inference::Engine,
    providers::{
        ProviderRegistry,
        anthropic::AnthropicAdapter,
        bedrock::BedrockAdapter,
        openai::OpenAiAdapter,
        openrouter::{OpenRouterAdapter, OpenRouterConfig},
        secrets::EnvSecrets,
    },
    store::Store,
    web::WebAssets,
};
use tracing_subscriber::EnvFilter;

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
enum BudgetCommand {
    /// Read-only: compare maintained budget totals with a full scan of
    /// reservations and executions in one snapshot; exits nonzero on any mismatch.
    Verify,
}

#[tokio::main]
async fn main() -> Result<()> {
    if let Some(path) = std::env::var_os("GATEWAY_ENV_FILE") {
        dotenvy::from_path(path)
            .map_err(|_| anyhow::anyhow!("Could not load selected environment file"))?;
    } else {
        dotenvy::dotenv().ok();
    }
    let cli = Cli::parse();
    if matches!(cli.command, Some(Command::SchemaVersion)) {
        println!("{}", schema_version()?);
        return Ok(());
    }
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("open_model_gateway=info")),
        )
        .init();
    let config = Config::from_env()?;
    if matches!(cli.command, Some(Command::BootstrapDemo { .. }))
        || std::env::var("GATEWAY_OIDC_ISSUER").as_deref() == Ok("http://127.0.0.1:18084")
    {
        ensure_demo_config(&config)?;
    }
    let pool = config.connect().await?;
    let store = Store::new(pool.clone());
    match cli.command.unwrap_or(Command::Serve) {
        Command::Migrate => {
            store
                .migrate_enterprise()
                .await
                .context("enterprise database initialization failed")?;
            println!("Migrations applied.");
        }
        Command::BootstrapDev => {
            store.preflight_enterprise().await?;
            let seeded = bootstrap::seed(&store, config.environment).await?;
            match seeded {
                Some(keys) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "notice": "Development keys: save these now. They cannot be retrieved again.",
                            "installation_id": keys.installation_id,
                            "personal_workspace_id": keys.personal_workspace_id,
                            "team_workspace_id": keys.team_workspace_id,
                            "personal_api_key": keys.personal_key.token,
                            "team_api_key": keys.team_key.token,
                        }))?
                    );
                }
                None => println!(
                    "Local development installation already initialized. No changes or key rotation performed."
                ),
            }
        }
        Command::BootstrapDemo {
            add_missing_personas,
        } => {
            if add_missing_personas {
                let created =
                    open_model_gateway::demo::add_missing_personas(&store, config.environment)
                        .await?;
                println!(
                    "{}",
                    if created {
                        "Enterprise demo personas added; no inference tokens were printed."
                    } else {
                        "Enterprise demo already exists; no data changed."
                    }
                );
            } else {
                let created = open_model_gateway::demo::seed(&store, config.environment).await?;
                println!(
                    "{}",
                    if created {
                        "Local demo created. Sample provider connections are disabled; no inference tokens were printed."
                    } else {
                        "Local demo already exists; no changes made. Use --add-missing-personas for an explicit additive upgrade."
                    }
                );
            }
        }
        Command::ReconcileExecutions { limit } => {
            store.preflight_enterprise().await?;
            let n = open_model_gateway::governance::reconcile_expired(&store, limit).await?;
            println!("Reconciled {n} expired executions; unknown cost holds retained.");
        }
        Command::CompactHistory {
            older_than_days,
            limit,
        } => {
            store.preflight_enterprise().await?;
            let n =
                open_model_gateway::maintenance::compact_history(&store, older_than_days, limit)
                    .await?;
            println!("Compacted {n} settled execution records; financial history retained.");
        }
        Command::Alerts {
            action: AlertsCommand::Evaluate { once },
        } => {
            anyhow::ensure!(
                once,
                "pass --once; `serve` evaluates alerts every GATEWAY_ALERT_INTERVAL_SECONDS"
            );
            store.preflight_enterprise().await?;
            match open_model_gateway::alerts::evaluate_once(&store).await? {
                Some(report) => {
                    let emails = open_model_gateway::alerts::deliver_pending(&store, 1000).await;
                    println!(
                        "Evaluated {} alert rules: {} fired, {} resolved, {} failed; {} email deliveries processed.",
                        report.rules, report.fired, report.resolved, report.failed_rules, emails
                    );
                }
                None => println!("Another replica is evaluating alerts right now; nothing done."),
            }
        }
        Command::Budget {
            action: BudgetCommand::Verify,
        } => {
            store.preflight_enterprise().await?;
            let report = open_model_gateway::governance::totals::verify(&store)
                .await
                .context("budget totals verification failed")?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            anyhow::ensure!(
                report.consistent(),
                "{} budget total bucket(s) differ from the full scan",
                report.mismatch_count
            );
        }
        Command::Files { action } => {
            store.preflight_enterprise().await?;
            let files = open_model_gateway::filestore::FileStoreRuntime::build(
                open_model_gateway::filestore::FileStoreConfig::from_env()?,
            )
            .await?;
            match action {
                FilesCommand::Verify { limit } => {
                    let report =
                        open_model_gateway::filestore::sweep::verify(&store, &files, limit).await?;
                    println!("{}", serde_json::to_string_pretty(&report)?);
                    anyhow::ensure!(
                        report.consistent(),
                        "stored file metadata and the file store differ"
                    );
                }
                FilesCommand::Sweep { once, limit } => {
                    anyhow::ensure!(once, "pass --once; `serve` sweeps every minute");
                    anyhow::ensure!(
                        files.store().is_some(),
                        "no file store is configured (GATEWAY_FILE_STORE=off)"
                    );
                    let report =
                        open_model_gateway::filestore::sweep::sweep_once(&store, &files, limit)
                            .await?;
                    println!("{}", serde_json::to_string_pretty(&report)?);
                }
            }
        }
        Command::SchemaVersion => unreachable!("handled before configuration"),
        Command::ProvisionUser {
            email,
            platform_admin,
        } => {
            store.preflight_enterprise().await?;
            let id = bootstrap::provision_user(&store, &email, platform_admin).await?;
            println!(
                "Provisioned user {id}. OIDC must provide the same verified email on first sign-in."
            );
        }
        Command::Serve => {
            anyhow::ensure!(
                store.is_ready().await,
                "database schema is not ready; run `open-model-gateway migrate` with the matching release"
            );
            let web = config
                .web_directory
                .as_deref()
                .map(WebAssets::load)
                .transpose()?;
            let mut registry = ProviderRegistry::default();
            let secrets = Arc::new(EnvSecrets::new(config.secret_env_allowlist));
            registry.register(Arc::new(OpenAiAdapter::new(secrets.clone())?))?;
            registry.register(Arc::new(AnthropicAdapter::new(secrets.clone())?))?;
            registry.register(Arc::new(BedrockAdapter::new()?))?;
            registry.register(Arc::new(OpenRouterAdapter::new(
                secrets.clone(),
                OpenRouterConfig::from_env()?,
            )?))?;
            let approvals =
                open_model_gateway::providers::local::endpoints::ApprovedEndpoints::from_env(
                    config.environment.as_str(),
                )?;
            for adapter in open_model_gateway::providers::local::adapters(secrets, approvals) {
                registry.register(adapter)?;
            }
            let engine = Engine::new(Arc::new(store.clone()), registry, config.inference_limits)?
                .with_realtime_limits(
                open_model_gateway::inference::realtime::RealtimeLimits::from_lookup(|name| {
                    std::env::var(name).ok()
                })?,
            )?;
            let identity = IdentityState::new(store.clone(), IdentityConfig::from_env()?)
                .await?
                .with_scim(open_model_gateway::scim::ScimConfig::from_env()?)?;
            let retention = open_model_gateway::maintenance::retention_from_env()?;
            // Encrypted file store: invalid configuration fails startup.
            let files = open_model_gateway::filestore::FileStoreRuntime::build(
                open_model_gateway::filestore::FileStoreConfig::from_env()?,
            )
            .await?;
            tracing::info!(backend = files.backend_name(), "file store configured");
            // Files API upload cap (GATEWAY_FILES_MAX_BYTES).
            open_model_gateway::filestore::upload::configure(
                open_model_gateway::filestore::upload::FilesApiLimits::from_lookup(|name| {
                    std::env::var(name).ok()
                })?,
            )?;
            let alert_interval = open_model_gateway::alerts::interval_from_env()?;
            // Async jobs (video, batch): limits and the background poller.
            open_model_gateway::jobs::configure(open_model_gateway::jobs::JobLimits::from_lookup(
                |name| std::env::var(name).ok(),
            )?)?;
            let listener = tokio::net::TcpListener::bind(config.listen).await?;
            tracing::info!(address = %listener.local_addr()?, serving_web = web.is_some(), "gateway listening");
            // Separate, optional metrics listener: never the public port or SPA.
            let metrics = match config.metrics_listen {
                Some(address) => {
                    let metrics_listener = tokio::net::TcpListener::bind(address).await?;
                    tracing::info!(address = %metrics_listener.local_addr()?, "metrics listening");
                    let app = open_model_gateway::metrics::router(store.clone());
                    Some(tokio::spawn(async move {
                        if axum::serve(metrics_listener, app).await.is_err() {
                            tracing::error!("metrics listener stopped");
                        }
                    }))
                }
                None => None,
            };
            let maintenance = open_model_gateway::maintenance::start(store.clone(), retention);
            let file_sweeper =
                open_model_gateway::filestore::sweep::start(store.clone(), files.clone());
            // Async jobs: the provider poller (video, native batches) and the
            // gateway-run batch runner (GATEWAY_BATCH_WORKERS).
            let batch_jobs = open_model_gateway::jobs::Jobs::new(store.clone(), &engine)
                .with_files(Some(files.clone()))
                // Batch scheduling reads server load signals only from
                // approved local origins (GATEWAY_LOCAL_UPSTREAMS).
                .with_approvals(
                    open_model_gateway::providers::local::endpoints::ApprovedEndpoints::from_env(
                        config.environment.as_str(),
                    )?,
                );
            let job_poller = open_model_gateway::jobs::poller::start(batch_jobs.clone());
            let batch_runner = open_model_gateway::jobs::runner::start(batch_jobs);
            let alerts =
                alert_interval.map(|every| open_model_gateway::alerts::start(store.clone(), every));
            let lifecycle_store = store.clone();
            let lifecycle = tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tick.tick().await;
                    if !matches!(
                        tokio::time::timeout(
                            std::time::Duration::from_secs(30),
                            open_model_gateway::lifecycle::cleanup_inactive_accounts(
                                &lifecycle_store
                            )
                        )
                        .await,
                        Ok(Ok(_))
                    ) {
                        tracing::error!("account lifecycle cleanup failed; will retry");
                    }
                }
            });
            let served = axum::serve(
                listener,
                http::router_with_identity(store, web, engine, identity)
                    .layer(axum::Extension(files)),
            )
            .with_graceful_shutdown(shutdown_signal())
            .await;
            if let Some(alerts) = alerts {
                alerts.abort();
                let _ = alerts.await;
            }
            if let Some(metrics) = metrics {
                metrics.abort();
                let _ = metrics.await;
            }
            if let Some(poller) = job_poller {
                poller.abort();
                let _ = poller.await;
            }
            if let Some(runner) = batch_runner {
                runner.abort();
                let _ = runner.await;
            }
            if let Some(sweeper) = file_sweeper {
                sweeper.abort();
                let _ = sweeper.await;
            }
            maintenance.abort();
            lifecycle.abort();
            let _ = maintenance.await;
            let _ = lifecycle.await;
            served?;
        }
    }
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
        assert!(
            migrations
                .iter()
                .all(|m| m["checksum"].as_str().unwrap().len() == 96)
        );
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
            secret_env_allowlist: vec![],
            inference_limits: Default::default(),
            metrics_listen: None,
            database_max_connections: 10,
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
