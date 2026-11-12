//! The registry: every metric family, its help text and its histogram buckets.
use super::*;

fn request_buckets() -> Histogram {
    // 5 ms .. ~82 s
    Histogram::new(exponential_buckets(0.005, 2.0, 15))
}
fn upstream_buckets() -> Histogram {
    // 10 ms .. ~164 s
    Histogram::new(exponential_buckets(0.01, 2.0, 15))
}
fn phase_buckets() -> Histogram {
    // 100 µs .. ~52 s: admission/settlement phases are sub-millisecond when
    // uncontended and seconds when queued behind the installation lock.
    Histogram::new(exponential_buckets(0.0001, 2.0, 20))
}

impl Metrics {
    #[cfg_attr(not(test), expect(clippy::too_many_lines, reason = "#499"))]
    pub(crate) fn new() -> Self {
        let mut registry = Registry::with_prefix("gateway");
        // Pre-create every trace label once. Recording increments plain atomic
        // counters, never takes a Family's lock or allocates label strings.
        let trace_drops = Family::<L1, Counter>::default();
        let trace_dropped_spans = std::array::from_fn(|i| {
            trace_drops
                .get_or_create(&[(
                    "reason",
                    crate::telemetry::DropReason::ALL[i].as_str().to_owned(),
                )])
                .clone()
        });
        let trace_exchanges = Family::<L1, Counter>::default();
        let trace_deliveries = std::array::from_fn(|i| {
            trace_exchanges
                .get_or_create(&[(
                    "outcome",
                    crate::telemetry::Delivery::ALL[i].as_str().to_owned(),
                )])
                .clone()
        });
        let metric_pushes = Family::<L1, Counter>::default();
        let otlp_metrics_pushes = std::array::from_fn(|i| {
            metric_pushes
                .get_or_create(&[("outcome", crate::otlp::Delivery::ALL[i].as_str().to_owned())])
                .clone()
        });
        let metric_drops = Family::<L1, Counter>::default();
        let otlp_metrics_dropped = std::array::from_fn(|i| {
            metric_drops
                .get_or_create(&[(
                    "reason",
                    crate::metrics_export::DropReason::ALL[i]
                        .as_str()
                        .to_owned(),
                )])
                .clone()
        });
        let logs_pushes = Family::<L1, Counter>::default();
        let otlp_logs_pushes = std::array::from_fn(|i| {
            logs_pushes
                .get_or_create(&[("outcome", crate::otlp::Delivery::ALL[i].as_str().to_owned())])
                .clone()
        });
        let logs_drops = Family::<L1, Counter>::default();
        let otlp_logs_dropped = std::array::from_fn(|i| {
            logs_drops
                .get_or_create(&[(
                    "reason",
                    crate::ops_log::DropReason::ALL[i].as_str().to_owned(),
                )])
                .clone()
        });
        let logs_suppressed = Family::<L1, Counter>::default();
        let otlp_logs_suppressed = std::array::from_fn(|i| {
            logs_suppressed
                .get_or_create(&[("event", crate::ops_log::OpsEvent::SLUGS[i].to_owned())])
                .clone()
        });
        let metrics = Self {
            http_requests: Family::default(),
            http_duration: Family::new_with_constructor(request_buckets),
            attempts: Family::default(),
            attempt_errors: Family::default(),
            upstream_latency: Family::new_with_constructor(upstream_buckets),
            time_to_first_token: Family::new_with_constructor(upstream_buckets),
            tokens: Family::default(),
            settlements: Family::default(),
            accounting_failures: Family::default(),
            denials: Family::default(),
            admission_phases: Family::new_with_constructor(phase_buckets),
            settlement_phases: Family::new_with_constructor(phase_buckets),
            reservations: Family::default(),
            alert_runs: Family::default(),
            alert_rule_failures: Counter::default(),
            aged_hold_alerts: Family::default(),
            alert_delivery_attempts: Family::default(),
            alert_delivery_queue: Family::default(),
            alert_webhook_attempts: Family::default(),
            alert_webhook_queue: Family::default(),
            alert_webhook_oldest: Family::default(),
            alert_pending_buckets: Family::default(),
            alert_oldest_pending_age: Family::default(),
            deadlock_retries: Family::default(),
            scope_lock_wait: Family::new_with_constructor(phase_buckets),
            scope_lock_waiting: Family::default(),
            pool_connections: Family::default(),
            pool_max: Gauge::default(),
            collection_errors: Family::default(),
            file_store_ops: Family::default(),
            file_store_bytes: Family::default(),
            batches: Family::default(),
            batch_lines: Family::default(),
            batch_queue: Family::default(),
            batch_workers: Family::default(),
            batch_route_lines: Family::default(),
            batch_paused_routes: Family::default(),
            batch_route_pauses: Family::default(),
            batch_route_providers: Mutex::default(),
            cache_lookups: Family::default(),
            config_changes: Family::default(),
            config_poll_failures: Counter::default(),
            auth_throttled: Family::default(),
            key_issuance_refused: Family::default(),
            egress_denied: Family::default(),
            audit_export_deliveries: Family::default(),
            audit_export_rejected_records: Counter::default(),
            trace_dropped_spans,
            trace_deliveries,
            trace_rejected_spans: Counter::default(),
            otlp_metrics_pushes,
            otlp_metrics_dropped,
            otlp_metrics_rejected_points: Counter::default(),
            otlp_logs_pushes,
            otlp_logs_dropped,
            otlp_logs_rejected_records: Counter::default(),
            otlp_logs_suppressed,
            audit_export_queued: Gauge::default(),
            audit_export_oldest: Default::default(),
            config_listener: Gauge::default(),
            leases_held: Family::default(),
            lease_terms: Family::default(),
            background_runs: Family::default(),
            report_outcomes: Family::default(),
            report_delivery_attempts: Family::default(),
            report_delivery_queue: Family::default(),
            report_backlog: Family::default(),
            report_backlog_capped: Family::default(),
            report_oldest_due_lag: Family::default(),
            partition_months: Family::default(),
            history_findings: Gauge::default(),
            rollup_hours: Family::default(),
            providers: Mutex::default(),
            models: Mutex::default(),
            reservations_refreshed: Mutex::default(),
            registry: Registry::default(),
        };
        // A labelled gauge rather than an OpenMetrics `info` type, so the
        // exposition also parses as classic Prometheus text.
        let build = Family::<L1, Gauge>::default();
        build
            .get_or_create(&[("version", env!("CARGO_PKG_VERSION").to_owned())])
            .set(1);
        registry.register("build_info", "Build information (always 1)", build);
        registry.register(
            "http_requests",
            "HTTP responses by method, route template and status",
            metrics.http_requests.clone(),
        );
        registry.register(
            "http_request_duration_seconds",
            "Time until response headers (streams: time to first byte) by method and route template",
            metrics.http_duration.clone(),
        );
        registry.register(
            "inference_attempts",
            "Finished upstream attempts by provider kind, public model and outcome",
            metrics.attempts.clone(),
        );
        registry.register(
            "inference_attempt_errors",
            "Failed upstream attempts by provider kind and safe error code",
            metrics.attempt_errors.clone(),
        );
        registry.register(
            "upstream_duration_seconds",
            "Upstream dispatch to end of response, by provider kind and public model",
            metrics.upstream_latency.clone(),
        );
        registry.register(
            "upstream_time_to_first_token_seconds",
            "Streams: upstream dispatch to first content delta",
            metrics.time_to_first_token.clone(),
        );
        registry.register(
            "inference_tokens",
            "Provider-reported tokens by provider kind, public model and direction (unreported usage is not counted)",
            metrics.tokens.clone(),
        );
        registry.register(
            "settlements",
            "Attempt settlements: settled (exact), unknown (hold retained), held (finalization failed; pending reconciliation)",
            metrics.settlements.clone(),
        );
        registry.register(
            "accounting_failures",
            "Failed accounting operations (the cause of 503 accounting_unavailable), by stage (route_lookup, pool_acquire_and_begin, governance_storage, explicit_settlement, cancellation_settlement) and cause (pool_timeout, pool_closed, lock_timeout, statement_timeout, backend_terminated, deadlock, read_only_transaction, other_database, io, tls, protocol, worker_crashed, other; for the two settlement stages what the settlement waited on when its deadline elapsed: deadline_in_queue, deadline_acquiring_connection, deadline_in_database). Fixed values only (5 stages and 16 causes, at most 80 label combinations): no SQL, SQLSTATE, message or identifier",
            metrics.accounting_failures.clone(),
        );
        registry.register(
            "admission_denials",
            "Admission denials by error code and limit scope",
            metrics.denials.clone(),
        );
        registry.register(
            "admission_seconds",
            "Durable admission transaction time by phase (queue, connect, locks, read, limits, write, commit, total) and outcome (admitted, denied, rejected, error)",
            metrics.admission_phases.clone(),
        );
        registry.register(
            "settlement_seconds",
            "Terminal settlement transaction time by phase (queue, connect, locks, read, write, commit, total) and outcome (settled, unknown, replay, conflict, error)",
            metrics.settlement_phases.clone(),
        );
        registry.register(
            "reservations_held",
            "Reservations still holding budget: pending (in flight/lease) and unknown (unresolved cost)",
            metrics.reservations.clone(),
        );
        registry.register(
            "alert_evaluations",
            "Alert evaluator runs by result (ok, skipped, failed)",
            metrics.alert_runs.clone(),
        );
        registry.register(
            "aged_hold_alerts",
            "Committed aged unknown-hold fires, escalations, clears, deferred ticks and email suppressions",
            metrics.aged_hold_alerts.clone(),
        );
        registry.register(
            "alert_rule_failures",
            "Individual alert rules that failed during an evaluation",
            metrics.alert_rule_failures.clone(),
        );
        registry.register(
            "alert_webhook_attempts",
            "Webhook attempt outcomes (fixed labels only)",
            metrics.alert_webhook_attempts.clone(),
        );
        registry.register(
            "alert_webhook_deliveries",
            "Active webhook queue and terminal outcomes completed in the last 24 hours",
            metrics.alert_webhook_queue.clone(),
        );
        registry.register(
            "alert_webhook_oldest_waiting_seconds",
            "Age of oldest generation-matching waiting webhook",
            metrics.alert_webhook_oldest.clone(),
        );
        registry.register(
            "alert_email_attempts",
            "Recipient attempt outcomes (no recipient labels)",
            metrics.alert_delivery_attempts.clone(),
        );
        registry.register(
            "alert_email_deliveries",
            "Pending deliveries created or terminal failures/suppressions completed in the last 24 hours",
            metrics.alert_delivery_queue.clone(),
        );
        registry.register(
            "lock_deadlock_retries",
            "Governance transactions re-run after the database aborted them as a deadlock victim (SQLSTATE 40P01), by path",
            metrics.deadlock_retries.clone(),
        );
        registry.register(
            "scope_lock_wait_seconds",
            "Time to acquire scoped admission locks, by lock (authority: workspace type/workspace/user/key advisory locks; rows: the workspace and key totals and counter rows that serialize one scope) and path (admission, settlement). No scope identifiers; a single hot workspace or key shows as a rising rows/admission tail",
            metrics.scope_lock_wait.clone(),
        );
        registry.register(
            "scope_lock_waiting",
            "Governance transactions of this replica waiting for scoped locks right now, by path (admission, settlement)",
            metrics.scope_lock_waiting.clone(),
        );
        registry.register(
            "db_pool_connections",
            "Database pool connections by state (idle, in_use)",
            metrics.pool_connections.clone(),
        );
        registry.register(
            "db_pool_max_connections",
            "Configured database pool maximum",
            metrics.pool_max.clone(),
        );
        registry.register(
            "metrics_collection_errors",
            "Scrape-time collectors that failed (values keep their last reading)",
            metrics.collection_errors.clone(),
        );
        registry.register(
            "file_store_operations",
            "File store operations by backend (local, s3), operation and outcome (ok or a safe error code)",
            metrics.file_store_ops.clone(),
        );
        registry.register(
            "file_store_bytes",
            "Plaintext bytes written (put) and read to the end (get) by backend",
            metrics.file_store_bytes.clone(),
        );
        registry.register(
            "batches",
            "Batches by mode (native, gateway) and event (created, submitted, or the final state)",
            metrics.batches.clone(),
        );
        registry.register(
            "batch_lines",
            "Batch lines processed by mode, provider kind and outcome (completed, failed)",
            metrics.batch_lines.clone(),
        );
        registry.register(
            "batch_queue_depth",
            "Unfinished batches by mode",
            metrics.batch_queue.clone(),
        );
        registry.register(
            "batch_workers",
            "Gateway-run batch line workers of this process: capacity and busy",
            metrics.batch_workers.clone(),
        );
        registry.register(
            "batch_route_lines",
            "Gateway-run batch lines by route provider kind and state (waiting for capacity, running); installation-wide",
            metrics.batch_route_lines.clone(),
        );
        registry.register(
            "batch_paused_routes",
            "Routes with batch lines waiting, by pause reason; installation-wide",
            metrics.batch_paused_routes.clone(),
        );
        registry.register(
            "batch_route_pauses",
            "Times this process saw a route's batch gate close, by reason",
            metrics.batch_route_pauses.clone(),
        );
        registry.register(
            "cache_lookups",
            "Per-replica cache lookups by cache (keys, candidates, routes, health, prices) and result (hit, miss, bypass: versions unconfirmed or caching off)",
            metrics.cache_lookups.clone(),
        );
        registry.register(
            "config_changes",
            "Configuration version changes this replica observed, by topic (access, catalog, keys, policy, settings)",
            metrics.config_changes.clone(),
        );
        registry.register(
            "audit_export_deliveries",
            "Audit export requests by outcome (delivered, failed); skipped counts events no longer exportable",
            metrics.audit_export_deliveries.clone(),
        );
        registry.register(
            "audit_export_rejected_records",
            "Audit events an OTLP collector refused for good (not retried; receivers see the seq gap)",
            metrics.audit_export_rejected_records.clone(),
        );
        registry.register(
            "trace_dropped_spans",
            "Trace spans dropped locally, by fixed reason",
            trace_drops,
        );
        registry.register(
            "trace_deliveries",
            "Trace export exchanges, by fixed outcome (no retries)",
            trace_exchanges,
        );
        registry.register(
            "trace_rejected_spans",
            "Trace spans rejected by the collector partialSuccess acknowledgement",
            metrics.trace_rejected_spans.clone(),
        );
        registry.register(
            "otlp_metrics_pushes",
            "Metrics export exchanges, by fixed outcome (no retries)",
            metric_pushes,
        );
        registry.register(
            "otlp_metrics_dropped",
            "Metrics export points dropped locally, by fixed reason",
            metric_drops,
        );
        registry.register(
            "otlp_metrics_rejected_points",
            "Metrics points rejected by the collector partialSuccess acknowledgement",
            metrics.otlp_metrics_rejected_points.clone(),
        );
        registry.register(
            "otlp_logs_pushes",
            "Operational-log export exchanges, by fixed outcome (no retries)",
            logs_pushes,
        );
        registry.register(
            "otlp_logs_dropped",
            "Operational-log records dropped locally, by fixed reason",
            logs_drops,
        );
        registry.register(
            "otlp_logs_rejected_records",
            "Operational-log records rejected by the collector partialSuccess acknowledgement",
            metrics.otlp_logs_rejected_records.clone(),
        );
        registry.register(
            "otlp_logs_suppressed",
            "Operational-log events suppressed by the per-variant rate limiter, by fixed event",
            logs_suppressed,
        );
        registry.register(
            "audit_export_queued",
            "Audit events queued for export, not yet accepted by the receiver",
            metrics.audit_export_queued.clone(),
        );
        registry.register(
            "audit_export_oldest_seconds",
            "Age of the oldest audit event queued for export (0 when none)",
            metrics.audit_export_oldest.clone(),
        );
        registry.register(
            "egress_denied",
            "Outbound connections refused by the egress policy, by purpose (provider, oidc, storage, catalog, smtp, audit)",
            metrics.egress_denied.clone(),
        );
        registry.register(
            "auth_throttled",
            "Sign-in steps refused by abuse controls, by step (login, callback) and reason (client_rate, pending_cap)",
            metrics.auth_throttled.clone(),
        );
        registry.register(
            "key_issuance_refused",
            "API key creations and rotations refused by the issuance budget (#253), by scope (holder, workspace) and operation (create, rotate). No workspace or user identifiers",
            metrics.key_issuance_refused.clone(),
        );
        registry.register(
            "config_poll_failures",
            "Failed configuration version polls (caches are bypassed after 3 s without a successful poll)",
            metrics.config_poll_failures.clone(),
        );
        registry.register(
            "config_listener_up",
            "Whether this replica's configuration LISTEN connection is up (1) or not (0)",
            metrics.config_listener.clone(),
        );
        registry.register(
            "work_leases_held",
            "Background work leases this replica holds (1) or not (0), by lease",
            metrics.leases_held.clone(),
        );
        registry.register(
            "work_lease_terms",
            "Lease terms this replica started (acquisitions and takeovers), by lease",
            metrics.lease_terms.clone(),
        );
        registry.register(
            "background_runs",
            "Singleton background job runs on this replica by job and result (ok, failed, fenced: the lease term ended)",
            metrics.background_runs.clone(),
        );
        for category in crate::scheduled_reports::Outcome::ALL {
            metrics
                .report_outcomes
                .get_or_create(&[("category", category.as_str().to_owned())])
                .inc_by(0);
        }
        registry.register(
            "report_generation_outcomes",
            "Scheduled report generation outcomes, fixed categories only; no scope identifiers",
            metrics.report_outcomes.clone(),
        );
        registry.register(
            "report_email_attempts",
            "Report recipient send outcomes; fixed categories only",
            metrics.report_delivery_attempts.clone(),
        );
        registry.register("report_email_recipients", "Recipient states in the latest 1024 report deliveries created within 24 hours; ineligible means authorization suppression", metrics.report_delivery_queue.clone());
        registry.register(
            "report_due_backlog",
            "Active due schedules, saturated at 1024; includes uninitialized calendars",
            metrics.report_backlog.clone(),
        );
        registry.register(
            "report_due_backlog_capped",
            "Whether due backlog exceeds the 1024-schedule bounded sample",
            metrics.report_backlog_capped.clone(),
        );
        registry.register(
            "report_oldest_due_lag_seconds",
            "Oldest initialized active due schedule lag at the scheduler's last bounded check",
            metrics.report_oldest_due_lag.clone(),
        );
        registry.register(
            "history_partition_months_ahead",
            "Future UTC months covered by partitions, by history table (the partitions job's last check; alert below 2)",
            metrics.partition_months.clone(),
        );
        registry.register(
            "history_verify_findings",
            "Orphaned or scope-mismatched history references found by the history_verify job's last run on this replica (0034; alert on any)",
            metrics.history_findings.clone(),
        );
        registry.register(
            "usage_rollup_hours",
            "Hours the rollups job (re)computed on this replica, by reason (new, changed)",
            metrics.rollup_hours.clone(),
        );
        // Empty-label families can be cleared on replicas that do not hold
        // the metrics lease, rather than exporting stale data.
        registry.register(
            "alert_delivery_pending_buckets",
            "Distinct pending delivery buckets, including overdue work awaiting cleanup",
            metrics.alert_pending_buckets.clone(),
        );
        registry.register(
            "alert_delivery_oldest_pending_age_seconds",
            "Age in seconds of the oldest pending delivery, or zero for an empty queue",
            metrics.alert_oldest_pending_age.clone(),
        );
        Self {
            registry,
            ..metrics
        }
    }
}
