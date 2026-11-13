//! The subcommands that `run` (main.rs) dispatches to, one function per
//! command, after logging, configuration and the database pool are set up.
//! Part of the binary only: declared by `main.rs`, not `lib.rs`.

mod serve;

use anyhow::{Context, Result};
use open_model_gateway::{bootstrap, config::Config, store::Store};
use sqlx::PgPool;

use open_model_gateway::providers::{local::endpoints::ApprovedEndpoints, secrets::EnvSecrets};

use crate::{
    AlertsCommand, ArchiveCommand, BudgetCommand, Command, FilesCommand, GrantsCommand,
    HistoryCommand, PartitionsCommand, RollupsCommand,
};

use serve::serve;

/// Runs one subcommand after `run` has set up logging, configuration and the
/// database pool. `schema-version`, `healthcheck` and `archive verify` are
/// handled in `main`, and `wait-for-database` in `run`, before this point.
pub(crate) async fn dispatch(
    command: Command,
    config: Config,
    store: Store,
    pool: &PgPool,
    provider_secrets: EnvSecrets,
    local_approvals: &mut Option<ApprovedEndpoints>,
    process_start_unix_nano: u64,
) -> Result<()> {
    match command {
        Command::WaitForDatabase { .. } => unreachable!("handled before connecting"),
        Command::Migrate => migrate(&store).await?,
        Command::Grants { action } => grants(&store, action).await?,
        Command::BootstrapDev => bootstrap_dev(&store, &config).await?,
        Command::BootstrapDemo {
            add_missing_personas,
        } => bootstrap_demo(&store, &config, add_missing_personas).await?,
        Command::ReconcileExecutions { limit } => {
            reconcile_executions(&store, limit).await?;
        }
        Command::CompactHistory {
            older_than_days,
            limit,
        } => compact_history(&store, older_than_days, limit).await?,
        Command::Alerts {
            action: AlertsCommand::Evaluate { once },
        } => alerts_evaluate(&store, once).await?,
        Command::Budget {
            action: BudgetCommand::Verify,
        } => budget_verify(&store).await?,
        Command::History {
            action: HistoryCommand::Verify { since },
        } => history_verify(&store, since).await?,
        Command::Partitions { action } => partitions(&store, pool, action).await?,
        Command::Archive { action } => archive(&store, pool, action).await?,
        Command::Rollups {
            action:
                RollupsCommand::Run {
                    once,
                    budget_seconds,
                },
        } => rollups_run(&store, once, budget_seconds).await?,
        Command::Files { action } => files(&store, action).await?,
        Command::SchemaVersion | Command::Healthcheck { .. } => {
            unreachable!("handled before configuration")
        }
        Command::ProvisionUser {
            email,
            platform_admin,
        } => provision_user(&store, email, platform_admin).await?,
        Command::Serve => {
            serve(
                config,
                store,
                pool,
                provider_secrets,
                local_approvals,
                process_start_unix_nano,
            )
            .await?;
        }
    }
    Ok(())
}

/// `migrate`: apply the embedded migrations.
pub(crate) async fn migrate(store: &Store) -> Result<()> {
    store
        .migrate_enterprise()
        .await
        .context("enterprise database initialization failed")?;
    println!("Migrations applied.");
    Ok(())
}

/// `grants apply` and `grants verify`.
pub(crate) async fn grants(store: &Store, action: GrantsCommand) -> Result<()> {
    match action {
        GrantsCommand::Apply => {
            store
                .apply_runtime_grants()
                .await
                .context("applying runtime grants failed")?;
            println!("Runtime grants applied.");
        }
        GrantsCommand::Verify => {
            store
                .verify_runtime_grants()
                .await
                .context("runtime privilege verification failed")?;
            println!("Runtime privileges verified.");
        }
    }
    Ok(())
}

/// `bootstrap-dev`: seed local development workspaces and print new keys once.
pub(crate) async fn bootstrap_dev(store: &Store, config: &Config) -> Result<()> {
    store.preflight_enterprise().await?;
    let seeded = bootstrap::seed(store, config.environment).await?;
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
    Ok(())
}

/// `bootstrap-demo`: seed or extend the local demo.
pub(crate) async fn bootstrap_demo(
    store: &Store,
    config: &Config,
    add_missing_personas: bool,
) -> Result<()> {
    if add_missing_personas {
        let created =
            open_model_gateway::demo::add_missing_personas(store, config.environment).await?;
        println!(
            "{}",
            if created {
                "Enterprise demo personas added; no inference tokens were printed."
            } else {
                "Enterprise demo already exists; no data changed."
            }
        );
    } else {
        let created = open_model_gateway::demo::seed(store, config.environment).await?;
        println!(
            "{}",
            if created {
                "Local demo created. Sample provider connections are disabled; no inference tokens were printed."
            } else {
                "Local demo already exists; no changes made. Use --add-missing-personas for an explicit additive upgrade."
            }
        );
    }
    Ok(())
}

/// `reconcile-executions`.
pub(crate) async fn reconcile_executions(store: &Store, limit: i64) -> Result<()> {
    store.preflight_enterprise().await?;
    let n = open_model_gateway::governance::reconcile_expired(store, limit).await?;
    println!("Reconciled {n} expired executions; unknown cost holds retained.");
    Ok(())
}

/// `compact-history`.
pub(crate) async fn compact_history(store: &Store, older_than_days: i32, limit: i64) -> Result<()> {
    store.preflight_enterprise().await?;
    let n = open_model_gateway::maintenance::compact_history(store, older_than_days, limit).await?;
    println!("Compacted {n} settled execution records; financial history retained.");
    Ok(())
}

/// `alerts evaluate --once`.
pub(crate) async fn alerts_evaluate(store: &Store, once: bool) -> Result<()> {
    anyhow::ensure!(
        once,
        "pass --once; `serve` evaluates alerts every GATEWAY_ALERT_INTERVAL_SECONDS"
    );
    store.preflight_enterprise().await?;
    let report = open_model_gateway::alerts::evaluate_once(store).await?;
    // Delivery is fenced per recipient claim, not by the evaluation
    // lease, so draining is safe (and useful) even when another
    // replica is evaluating. Bounded so a hanging relay cannot hang
    // the command.
    let emails = tokio::time::timeout(
        std::time::Duration::from_secs(25 * 40 + 60),
        open_model_gateway::alerts::deliver_pending(store, 1000),
    )
    .await
    .unwrap_or(0);
    match report {
        Some(report) => println!(
            "Evaluated {} alert rules: {} fired, {} resolved, {} failed; {} email deliveries processed.",
            report.rules, report.fired, report.resolved, report.failed_rules, emails
        ),
        None => println!(
            "Another replica is evaluating alerts right now; {emails} email deliveries processed."
        ),
    }
    Ok(())
}

/// `budget verify`.
pub(crate) async fn budget_verify(store: &Store) -> Result<()> {
    store.preflight_enterprise().await?;
    let report = open_model_gateway::governance::totals::verify(store)
        .await
        .context("budget totals verification failed")?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    anyhow::ensure!(
        report.consistent(),
        "{} budget total bucket(s) and {} rate counter(s) differ from the full scan",
        report.mismatch_count,
        report.rate_mismatch_count
    );
    Ok(())
}

/// `history verify`.
pub(crate) async fn history_verify(store: &Store, since: Option<String>) -> Result<()> {
    store.preflight_enterprise().await?;
    let since = since
        .map(|s| {
            chrono::DateTime::parse_from_rfc3339(&s)
                .map(|t| t.with_timezone(&chrono::Utc))
                .context("--since must be an RFC 3339 time")
        })
        .transpose()?;
    let report = open_model_gateway::history::verify_store(store, since)
        .await
        .context("history verification failed")?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    // Alert bookkeeping needs write access (runtime or migrator);
    // a read-only role still gets the report and the exit status.
    if let Err(error) = open_model_gateway::history::record_cli_result(store, &report).await {
        tracing::warn!(%error, "could not update the history_orphans incident");
    }
    anyhow::ensure!(
        report.consistent(),
        "{} history finding(s); see the report",
        report.finding_count
    );
    Ok(())
}

/// `partitions prepare|ensure|status`.
pub(crate) async fn partitions(
    store: &Store,
    pool: &PgPool,
    action: PartitionsCommand,
) -> Result<()> {
    let report = match action {
        PartitionsCommand::Prepare => {
            store.preflight_upgrade().await?;
            let built = open_model_gateway::partitions::prepare(pool).await?;
            println!("{}", serde_json::json!({ "built": built }));
            pool.close().await;
            return Ok(());
        }
        PartitionsCommand::Ensure { ahead } => {
            store.preflight_enterprise().await?;
            anyhow::ensure!((0..=12).contains(&ahead), "--ahead must be 0..12");
            open_model_gateway::partitions::ensure(pool, ahead).await?
        }
        PartitionsCommand::Status => {
            store.preflight_enterprise().await?;
            let coverage = open_model_gateway::partitions::coverage(pool, None).await?;
            open_model_gateway::partitions::EnsureReport {
                short: coverage
                    .iter()
                    .filter(|c| c.months_ahead < open_model_gateway::partitions::ALERT_BELOW_MONTHS)
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
    Ok(())
}

/// `archive partition` (`archive verify` runs before configuration).
pub(crate) async fn archive(store: &Store, pool: &PgPool, action: ArchiveCommand) -> Result<()> {
    match action {
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
            let report = open_model_gateway::archive::archive(pool, &request).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        ArchiveCommand::Verify { .. } => unreachable!("handled before configuration"),
    }
    Ok(())
}

/// `rollups run --once`.
pub(crate) async fn rollups_run(store: &Store, once: bool, budget_seconds: u64) -> Result<()> {
    anyhow::ensure!(once, "pass --once; `serve` rolls hours every minute");
    store.preflight_enterprise().await?;
    // Batches of up to 168 hours until nothing is due or the budget is spent.
    let budget = std::time::Duration::from_secs(budget_seconds.clamp(1, 86_400));
    let started = std::time::Instant::now();
    let mut total = open_model_gateway::rollups::RollupReport::default();
    loop {
        let left = budget.saturating_sub(started.elapsed());
        let report = open_model_gateway::rollups::run_once(store, None, left).await?;
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
    Ok(())
}

/// `files verify|rekey|sweep`.
pub(crate) async fn files(store: &Store, action: FilesCommand) -> Result<()> {
    store.preflight_enterprise().await?;
    let files = open_model_gateway::filestore::FileStoreRuntime::build(
        open_model_gateway::filestore::FileStoreConfig::from_env()?,
    )
    .await?;
    match action {
        FilesCommand::Verify { limit } => {
            let report = open_model_gateway::filestore::sweep::verify(store, &files, limit).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            anyhow::ensure!(
                report.consistent(),
                "stored file metadata and the file store differ"
            );
        }
        FilesCommand::Rekey {
            from,
            to,
            dry_run,
            limit,
        } => {
            let report = open_model_gateway::filestore::rekey::rekey(
                store, &files, &from, &to, dry_run, limit,
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            anyhow::ensure!(
                dry_run || report.complete,
                "{} files still use {from}; rerun to retry them (see failed, skipped_deleting, other_backend)",
                report.remaining
            );
        }
        FilesCommand::Sweep { once, limit } => {
            anyhow::ensure!(once, "pass --once; `serve` sweeps every minute");
            anyhow::ensure!(
                files.store().is_some(),
                "no file store is configured (GATEWAY_FILE_STORE=off)"
            );
            let report =
                open_model_gateway::filestore::sweep::sweep_once(store, &files, limit).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}

/// `provision-user`.
pub(crate) async fn provision_user(
    store: &Store,
    email: String,
    platform_admin: bool,
) -> Result<()> {
    store.preflight_enterprise().await?;
    let id = bootstrap::provision_user(store, &email, platform_admin).await?;
    println!("Provisioned user {id}. OIDC must provide the same verified email on first sign-in.");
    Ok(())
}
