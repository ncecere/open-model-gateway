//! `serve`: process start-up as named steps, in the order they always ran
//! (#525): engine, configuration and leases, listeners, background tasks,
//! the HTTP server, then shutdown. Never migrates or bootstraps.

use std::sync::Arc;

use anyhow::{Context, Result};
use open_model_gateway::{
    audit_export::Exporter,
    config::Config,
    filestore::FileStoreRuntime,
    http,
    identity::{IdentityConfig, IdentityState},
    inference::Engine,
    leases::Leases,
    providers::{
        ProviderRegistry,
        anthropic::AnthropicAdapter,
        bedrock::BedrockAdapter,
        local::endpoints::ApprovedEndpoints,
        openai::OpenAiAdapter,
        openrouter::{OpenRouterAdapter, OpenRouterConfig},
        secrets::EnvSecrets,
    },
    store::Store,
    telemetry::{self, Recorder},
    web::WebAssets,
};
use sqlx::PgPool;
use tokio::task::JoinHandle;

use crate::shutdown_signal;

/// Start the gateway: the steps below, in this order, then serve until a
/// shutdown signal and stop everything that was started.
pub(crate) async fn serve(
    config: Config,
    store: Store,
    pool: &PgPool,
    provider_secrets: EnvSecrets,
    local_approvals: &mut Option<ApprovedEndpoints>,
    process_start_unix_nano: u64,
) -> Result<()> {
    let (traces, _recorder, store, web, engine) =
        build_engine(&config, store, provider_secrets, local_approvals).await?;
    let Prepared {
        identity,
        files,
        settings,
    } = prepare(store.clone()).await?;
    let (leases, lease_task) = take_leases(&settings.audit_setting, pool).await;
    // The leases are held from here on: a failure of any later start-up
    // step hands them back before its error is returned (#527).
    let Started {
        store,
        listener,
        tasks,
    } = match start(
        &config,
        store,
        &web,
        &engine,
        settings,
        &leases,
        process_start_unix_nano,
    )
    .await
    {
        Ok(started) => started,
        Err(error) => {
            release_leases(lease_task, &leases, pool).await;
            return Err(error);
        }
    };
    let served = axum::serve(
        listener,
        http::router_with_identity(store, web, engine, identity)
            .layer(axum::Extension(files))
            .layer(axum::Extension(config.inference_base_url))
            // The peer address feeds sign-in abuse controls (#78).
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await;
    shutdown(traces, tasks, lease_task, &leases, pool).await;
    served?;
    Ok(())
}

/// Schema check, traces, the web distribution, provider adapters and the
/// inference engine.
async fn build_engine(
    config: &Config,
    store: Store,
    provider_secrets: EnvSecrets,
    local_approvals: &mut Option<ApprovedEndpoints>,
) -> Result<(
    Option<telemetry::Runtime>,
    Option<Recorder>,
    Store,
    Option<WebAssets>,
    Engine,
)> {
    anyhow::ensure!(
        store.is_ready().await,
        "database schema is not ready; run `open-model-gateway migrate` with the matching release"
    );
    // Started early so both Store (background-root spans,
    // maintenance.rs) and Engine (interactive-generation spans) can
    // carry the same recorder. A collector failure never fails
    // inference.
    let traces = open_model_gateway::telemetry::Runtime::start(config.traces.clone());
    let recorder = traces.as_ref().map(|t| t.recorder());
    let store = store.with_traces(recorder.clone());
    let web = config
        .web_directory
        .as_deref()
        .map(WebAssets::load)
        .transpose()?;
    let mut registry = ProviderRegistry::default();
    let secrets = Arc::new(provider_secrets);
    registry.register(Arc::new(OpenAiAdapter::new(secrets.clone())?))?;
    registry.register(Arc::new(AnthropicAdapter::new(secrets.clone())?))?;
    registry.register(Arc::new(BedrockAdapter::new()?))?;
    registry.register(Arc::new(OpenRouterAdapter::new(
        secrets.clone(),
        OpenRouterConfig::from_env()?,
    )?))?;
    let approvals = local_approvals
        .take()
        .context("local upstream approvals are loaded before serving")?;
    for adapter in open_model_gateway::providers::local::adapters(secrets, approvals) {
        registry.register(adapter)?;
    }
    let engine = Engine::new(Arc::new(store.clone()), registry, config.inference_limits)?
        .with_realtime_limits(
            open_model_gateway::inference::realtime::RealtimeLimits::from_lookup(|name| {
                std::env::var(name).ok()
            })?,
        )?
        .with_telemetry(recorder.clone());
    Ok((traces, recorder, store, web, engine))
}

/// What [`prepare`] configured.
struct Prepared {
    identity: IdentityState,
    files: FileStoreRuntime,
    settings: Settings,
}

/// The configuration [`start`] consumes.
struct Settings {
    retention: Option<i32>,
    files: FileStoreRuntime,
    alert_interval: Option<std::time::Duration>,
    audit_setting: open_model_gateway::audit_export::Setting,
    partitions_ahead: i32,
}

/// Identity, retention, the file store, job and upload limits and the audit
/// export setting. Takes no lease.
async fn prepare(store: Store) -> Result<Prepared> {
    let identity = IdentityState::new(store, IdentityConfig::from_env()?)
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
    // Audit export (#106, #107, #108: webhook, syslog over TLS or OTLP
    // logs; TLS files and OTLP headers are read here):
    // operator-configured. Only an explicit `off`
    // turns it off; a replica without the setting leaves it alone and
    // never takes the export lease.
    let audit_setting = open_model_gateway::audit_export::Setting::from_env()?;
    // Parsed before any lease is taken, so a refused value never holds one.
    let partitions_ahead = open_model_gateway::partitions::ahead_from_env()?;
    Ok(Prepared {
        identity,
        files: files.clone(),
        settings: Settings {
            retention,
            files,
            alert_interval,
            audit_setting,
            partitions_ahead,
        },
    })
}

/// Background work leases (P5): singleton jobs run on one replica. Takes
/// them once, then renews them in the returned task. Infallible: every step
/// after this one runs inside [`start`], whose failure releases them.
async fn take_leases(
    audit_setting: &open_model_gateway::audit_export::Setting,
    pool: &PgPool,
) -> (Arc<Leases>, JoinHandle<()>) {
    let leases = open_model_gateway::leases::Leases::new();
    let leases = Arc::new(match audit_setting {
        open_model_gateway::audit_export::Setting::On(_) => leases,
        _ => leases.without(open_model_gateway::leases::Lease::AuditExport),
    });
    leases.renew_once(pool).await;
    let lease_task = leases
        .clone()
        .start(pool.clone(), open_model_gateway::leases::RENEW);
    (leases, lease_task)
}

/// What [`start`] started.
struct Started {
    store: Store,
    listener: tokio::net::TcpListener,
    tasks: Tasks,
}

/// Every start-up step that runs while the leases are held: the audit export
/// state, the listeners, background tasks, history jobs and the exporters.
async fn start(
    config: &Config,
    store: Store,
    web: &Option<WebAssets>,
    engine: &Engine,
    settings: Settings,
    leases: &Arc<Leases>,
    process_start_unix_nano: u64,
) -> Result<Started> {
    let Settings {
        retention,
        files,
        alert_interval,
        audit_setting,
        partitions_ahead,
    } = settings;
    let store = store.with_leases(leases.clone());
    open_model_gateway::audit_export::sync_state(&store, &audit_setting).await?;
    let audit_exporter = audit_setting.exporter().map(Arc::new);
    let store = match &audit_exporter {
        Some(exporter) => store.with_audit_export(exporter.clone()),
        None => store,
    };
    let (notifications, listener, metrics) = listen(config, &store, web, leases).await?;
    let background = start_background(
        config,
        &store,
        retention,
        &files,
        engine,
        alert_interval,
        audit_exporter,
    )?;
    let history = spawn_history(&store, leases, partitions_ahead);
    let (history_verify, lifecycle) = spawn_history_verify_and_lifecycle(&store, leases);
    // Independent of the Prometheus listener and traces; the Store
    // already carries leases and all configured listeners are bound.
    let metrics_export = open_model_gateway::metrics_export::Runtime::start(
        config.metrics_export.clone(),
        config.environment,
        process_start_unix_nano,
        store.clone(),
    );
    // Optional, disabled-by-default OTLP operational logs (#294):
    // typed events only, independent of traces/metrics/the Prometheus
    // listener. A collector failure never gates readiness.
    let logs_export =
        open_model_gateway::ops_log::Runtime::start(config.logs_export.clone(), config.environment);
    Ok(Started {
        store,
        listener,
        tasks: Tasks {
            metrics_export,
            logs_export,
            background,
            metrics,
            history,
            history_verify,
            lifecycle,
            notifications,
        },
    })
}

/// Configuration change notifications, then the public listener, then the
/// separate, optional metrics listener.
async fn listen(
    config: &Config,
    store: &Store,
    web: &Option<WebAssets>,
    leases: &Arc<Leases>,
) -> Result<(
    Option<JoinHandle<()>>,
    tokio::net::TcpListener,
    Option<JoinHandle<()>>,
)> {
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
    Ok((notifications, listener, metrics))
}

/// Tasks started by [`start_background`].
struct Background {
    maintenance: JoinHandle<()>,
    file_sweeper: Option<JoinHandle<()>>,
    job_poller: Option<JoinHandle<()>>,
    batch_runner: Option<JoinHandle<()>>,
    alerts: Option<JoinHandle<()>>,
    reports: JoinHandle<()>,
    audit_export: Option<JoinHandle<()>>,
}

/// Maintenance, the file sweeper, async jobs, alerts, scheduled reports and
/// audit export.
fn start_background(
    config: &Config,
    store: &Store,
    retention: Option<i32>,
    files: &FileStoreRuntime,
    engine: &Engine,
    alert_interval: Option<std::time::Duration>,
    audit_exporter: Option<Arc<Exporter>>,
) -> Result<Background> {
    let maintenance = open_model_gateway::maintenance::start(store.clone(), retention);
    let file_sweeper = open_model_gateway::filestore::sweep::start(store.clone(), files.clone());
    // Async jobs: the provider poller (video, native batches) and the
    // gateway-run batch runner (GATEWAY_BATCH_WORKERS).
    let batch_jobs = open_model_gateway::jobs::Jobs::new(store.clone(), engine)
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
    // Reports publish into the same fair email outbox dispatched by
    // the alert/email loop above; disabling that loop pauses all mail.
    let reports = open_model_gateway::scheduled_reports::start(store.clone());
    let audit_export = audit_exporter
        .map(|exporter| open_model_gateway::audit_export::start(store.clone(), exporter));
    Ok(Background {
        maintenance,
        file_sweeper,
        job_poller,
        batch_runner,
        alerts,
        reports,
        audit_export,
    })
}

/// History partitions and usage rollups (see the comment inside).
fn spawn_history(store: &Store, leases: &Arc<Leases>, partitions_ahead: i32) -> JoinHandle<()> {
    // History partitions (hourly) and usage rollups (every minute):
    // the `partitions` and `rollups` lease holders only.
    let history_leases = leases.clone();
    let history_store = store.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut minute = 0u64;
        loop {
            tick.tick().await;
            if minute.is_multiple_of(60) {
                open_model_gateway::leases::run_singleton(
                    &history_leases,
                    open_model_gateway::leases::Lease::Partitions,
                    open_model_gateway::ops_log::Job::Partitions,
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
                open_model_gateway::ops_log::Job::Rollups,
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
    })
}

/// History parent consistency (hourly), then access request expiry and
/// inactive account cleanup (every minute); see the comments inside.
fn spawn_history_verify_and_lifecycle(
    store: &Store,
    leases: &Arc<Leases>,
) -> (JoinHandle<()>, JoinHandle<()>) {
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
                open_model_gateway::ops_log::Job::HistoryVerify,
                std::time::Duration::from_secs(900),
                |fence| {
                    let store = verify_store.clone();
                    async move { open_model_gateway::history::run_job(&store, Some(&fence)).await }
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
                open_model_gateway::ops_log::Job::Lifecycle,
                std::time::Duration::from_secs(30),
                |fence| {
                    let store = lifecycle_store.clone();
                    async move {
                        open_model_gateway::lifecycle::expire_access_requests_fenced(
                            &store,
                            Some(&fence),
                        )
                        .await?;
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
    (history_verify, lifecycle)
}

/// Everything [`shutdown`] stops.
struct Tasks {
    metrics_export: Option<open_model_gateway::metrics_export::Runtime>,
    logs_export: Option<open_model_gateway::ops_log::Runtime>,
    background: Background,
    metrics: Option<JoinHandle<()>>,
    history: JoinHandle<()>,
    history_verify: JoinHandle<()>,
    lifecycle: JoinHandle<()>,
    notifications: Option<JoinHandle<()>>,
}

/// Drain the exporters together, stop every task, then release the leases.
async fn shutdown(
    traces: Option<telemetry::Runtime>,
    tasks: Tasks,
    lease_task: JoinHandle<()>,
    leases: &Arc<Leases>,
    pool: &PgPool,
) {
    let Tasks {
        metrics_export,
        logs_export,
        background,
        metrics,
        history,
        history_verify,
        lifecycle,
        notifications,
    } = tasks;
    let Background {
        maintenance,
        file_sweeper,
        job_poller,
        batch_runner,
        alerts,
        reports,
        audit_export,
    } = background;
    // All three exporters share the same shutdown window, not serial 3s drains.
    tokio::join!(
        async {
            if let Some(traces) = traces {
                traces.shutdown().await;
            }
        },
        async {
            if let Some(metrics_export) = metrics_export {
                metrics_export.shutdown().await;
            }
        },
        async {
            if let Some(logs_export) = logs_export {
                logs_export.shutdown().await;
            }
        },
    );
    if let Some(alerts) = alerts {
        alerts.abort();
        let _ = alerts.await;
    }
    reports.abort();
    let _ = reports.await;
    if let Some(audit_export) = audit_export {
        audit_export.abort();
        let _ = audit_export.await;
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
    release_leases(lease_task, leases, pool).await;
}

/// Stop renewing, then end every term this replica holds, so another replica
/// takes the singleton work now instead of at expiry: on shutdown and on
/// every start-up failure after [`take_leases`]. Bounded, and never fails
/// the caller: an unreleased term still ends at its expiry.
async fn release_leases(lease_task: JoinHandle<()>, leases: &Arc<Leases>, pool: &PgPool) {
    lease_task.abort();
    let _ = lease_task.await;
    // Each release is bounded on its own (`Leases::release_all`), so one
    // release waiting behind a running job cannot cost the others theirs.
    leases.release_all(pool).await;
}
