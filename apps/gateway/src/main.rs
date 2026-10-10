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
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cli))
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

async fn run(cli: Cli) -> Result<()> {
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
    // Reports, usage and logs only (never admission, settlement or writes).
    let store = Store::new(pool.clone())
        .with_reporting(config.connect_reporting()?, config.reporting_max_lag)
        .with_admission_mode(config.admission_mode);
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
                "{} budget total bucket(s) and {} rate counter(s) differ from the full scan",
                report.mismatch_count,
                report.rate_mismatch_count
            );
        }
        Command::History {
            action: HistoryCommand::Verify { since },
        } => {
            store.preflight_enterprise().await?;
            let since = since
                .map(|s| {
                    chrono::DateTime::parse_from_rfc3339(&s)
                        .map(|t| t.with_timezone(&chrono::Utc))
                        .context("--since must be an RFC 3339 time")
                })
                .transpose()?;
            let report = open_model_gateway::history::verify_store(&store, since)
                .await
                .context("history verification failed")?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            // Alert bookkeeping needs write access (runtime or migrator);
            // a read-only role still gets the report and the exit status.
            if let Err(error) =
                open_model_gateway::history::record_cli_result(&store, &report).await
            {
                tracing::warn!(%error, "could not update the history_orphans incident");
            }
            anyhow::ensure!(
                report.consistent(),
                "{} history finding(s); see the report",
                report.finding_count
            );
        }
        Command::Partitions { action } => {
            let report = match action {
                PartitionsCommand::Prepare => {
                    store.preflight_upgrade().await?;
                    let built = open_model_gateway::partitions::prepare(&pool).await?;
                    println!("{}", serde_json::json!({ "built": built }));
                    pool.close().await;
                    return Ok(());
                }
                PartitionsCommand::Ensure { ahead } => {
                    store.preflight_enterprise().await?;
                    anyhow::ensure!((0..=12).contains(&ahead), "--ahead must be 0..12");
                    open_model_gateway::partitions::ensure(&pool, ahead).await?
                }
                PartitionsCommand::Status => {
                    store.preflight_enterprise().await?;
                    let coverage = open_model_gateway::partitions::coverage(&pool, None).await?;
                    open_model_gateway::partitions::EnsureReport {
                        short: coverage
                            .iter()
                            .filter(|c| {
                                c.months_ahead < open_model_gateway::partitions::ALERT_BELOW_MONTHS
                            })
                            .map(|c| c.parent.clone())
                            .collect(),
                        coverage,
                        ..Default::default()
                    }
                }
            };
            println!("{}", serde_json::to_string_pretty(&report)?);
            anyhow::ensure!(
                report.error.is_none() && report.short.is_empty(),
                "history partitions are short: {:?} {}",
                report.short,
                report.error.as_deref().unwrap_or("")
            );
        }
        Command::Archive { action } => match action {
            ArchiveCommand::Partition {
                group,
                month,
                to,
                drop,
                retention_months,
            } => {
                store.preflight_enterprise().await?;
                let request = open_model_gateway::archive::Request {
                    group: open_model_gateway::archive::Group::parse(&group)
                        .context("group must be history, audit or storage")?,
                    month: open_model_gateway::archive::Month::parse(&month)
                        .context("month must be YYYY-MM or legacy")?,
                    to,
                    drop,
                    retention_months: match retention_months {
                        Some(n) => {
                            anyhow::ensure!(
                                (1..=1200).contains(&n),
                                "--retention-months must be 1..1200"
                            );
                            n
                        }
                        None => open_model_gateway::archive::retention_from_env()?,
                    },
                };
                let report = open_model_gateway::archive::archive(&pool, &request).await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
            }
            ArchiveCommand::Verify { .. } => unreachable!("handled before configuration"),
        },
        Command::Rollups {
            action:
                RollupsCommand::Run {
                    once,
                    budget_seconds,
                },
        } => {
            anyhow::ensure!(once, "pass --once; `serve` rolls hours every minute");
            store.preflight_enterprise().await?;
            // Batches of up to 168 hours until nothing is due or the budget is spent.
            let budget = std::time::Duration::from_secs(budget_seconds.clamp(1, 86_400));
            let started = std::time::Instant::now();
            let mut total = open_model_gateway::rollups::RollupReport::default();
            loop {
                let left = budget.saturating_sub(started.elapsed());
                let report = open_model_gateway::rollups::run_once(&store, None, left).await?;
                total.new_hours += report.new_hours;
                total.changed_hours += report.changed_hours;
                total.groups += report.groups;
                total.remaining = report.remaining;
                total.bound = report.bound;
                if report.remaining == 0
                    || report.new_hours + report.changed_hours == 0
                    || started.elapsed() >= budget
                {
                    break;
                }
            }
            println!("{}", serde_json::to_string_pretty(&total)?);
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
        Command::SchemaVersion | Command::Healthcheck { .. } => {
            unreachable!("handled before configuration")
        }
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
            let secrets = Arc::new(EnvSecrets::new(config.secret_env_allowlist.clone()));
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
            // Background work leases (P5): singleton jobs run on one replica.
            let leases = Arc::new(open_model_gateway::leases::Leases::new());
            leases.renew_once(&pool).await;
            let lease_task = leases
                .clone()
                .start(pool.clone(), open_model_gateway::leases::RENEW);
            let store = store.with_leases(leases.clone());
            // Per-replica caches (P4): version polling plus LISTEN.
            let notifications = config
                .config_cache
                .then(|| config.connect_listener())
                .transpose()?
                .map(|listen| store.start_change_notifications(listen));
            let listener = tokio::net::TcpListener::bind(config.listen).await?;
            tracing::info!(address = %listener.local_addr()?, serving_web = web.is_some(), admission_mode = store.admission_mode().as_str(), config_cache = config.config_cache, work_lease_holder = %leases.holder(), "gateway listening");
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
            // History partitions (hourly) and usage rollups (every minute):
            // the `partitions` and `rollups` lease holders only.
            let partitions_ahead = open_model_gateway::partitions::ahead_from_env()?;
            let history_leases = leases.clone();
            let history_store = store.clone();
            let history = tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut minute = 0u64;
                loop {
                    tick.tick().await;
                    if minute.is_multiple_of(60) {
                        open_model_gateway::leases::run_singleton(
                            &history_leases,
                            open_model_gateway::leases::Lease::Partitions,
                            "partitions",
                            std::time::Duration::from_secs(60),
                            |fence| {
                                let store = history_store.clone();
                                async move {
                                    open_model_gateway::partitions::run_job(
                                        &store,
                                        partitions_ahead,
                                        Some(&fence),
                                    )
                                    .await
                                }
                            },
                        )
                        .await;
                    }
                    open_model_gateway::leases::run_singleton(
                        &history_leases,
                        open_model_gateway::leases::Lease::Rollups,
                        "rollups",
                        std::time::Duration::from_secs(50),
                        |fence| {
                            let store = history_store.clone();
                            async move {
                                open_model_gateway::rollups::run_once(
                                    &store,
                                    Some(&fence),
                                    std::time::Duration::from_secs(30),
                                )
                                .await
                            }
                        },
                    )
                    .await;
                    minute = minute.wrapping_add(1);
                }
            });
            // History parent consistency (0034, hourly, its own task so a
            // long first window never delays rollups): the `history_verify`
            // lease holder only.
            let verify_leases = leases.clone();
            let verify_store = store.clone();
            let history_verify = tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                // First run after the lease task has had time to take terms.
                tokio::time::sleep(std::time::Duration::from_secs(120)).await;
                loop {
                    tick.tick().await;
                    open_model_gateway::leases::run_singleton(
                        &verify_leases,
                        open_model_gateway::leases::Lease::HistoryVerify,
                        "history_verify",
                        std::time::Duration::from_secs(900),
                        |fence| {
                            let store = verify_store.clone();
                            async move {
                                open_model_gateway::history::run_job(&store, Some(&fence)).await
                            }
                        },
                    )
                    .await;
                }
            });
            let lifecycle_leases = leases.clone();
            let lifecycle_store = store.clone();
            // The `lifecycle` lease holder only (one replica per minute).
            let lifecycle = tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tick.tick().await;
                    open_model_gateway::leases::run_singleton(
                        &lifecycle_leases,
                        open_model_gateway::leases::Lease::Lifecycle,
                        "lifecycle",
                        std::time::Duration::from_secs(30),
                        |fence| {
                            let store = lifecycle_store.clone();
                            async move {
                                open_model_gateway::lifecycle::cleanup_inactive_accounts_fenced(
                                    &store,
                                    Some(&fence),
                                )
                                .await
                            }
                        },
                    )
                    .await;
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
            history.abort();
            let _ = history.await;
            history_verify.abort();
            let _ = history_verify.await;
            let _ = maintenance.await;
            let _ = lifecycle.await;
            if let Some(notifications) = notifications {
                notifications.abort();
                let _ = notifications.await;
            }
            // Hand singleton work to another replica now instead of at expiry.
            lease_task.abort();
            let _ = lease_task.await;
            leases.release_all(&pool).await;
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
            secret_env_allowlist: vec![],
            inference_limits: Default::default(),
            metrics_listen: None,
            database_max_connections: 10,
            reporting_database_url: None,
            reporting_max_lag: std::time::Duration::from_secs(30),
            admission_mode: Default::default(),
            listen_database_url: None,
            config_cache: true,
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
