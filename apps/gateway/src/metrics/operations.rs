//! Operational counters and gauges: partitions, rollups, caches, configuration,
//! OTLP and audit export, egress and sign-in refusals, leases, reports and
//! background runs.
use super::*;

impl Metrics {
    pub(crate) fn set_partition_months(&self, table: &'static str, months: i64) {
        self.partition_months
            .get_or_create(&[("table", table.to_owned())])
            .set(months);
    }

    pub(crate) fn set_history_findings(&self, findings: i64) {
        self.history_findings.set(findings);
    }

    pub(crate) fn observe_rollup_hours(&self, reason: &'static str, n: u64) {
        if n > 0 {
            self.rollup_hours
                .get_or_create(&[("reason", reason.to_owned())])
                .inc_by(n);
        }
    }

    /// One cache lookup (`cache` and `result` from fixed sets).
    pub(crate) fn observe_cache(&self, cache: &'static str, result: &'static str) {
        self.observe_cache_n(cache, result, 1);
    }

    pub(crate) fn observe_cache_n(&self, cache: &'static str, result: &'static str, n: u64) {
        if n > 0 {
            self.cache_lookups
                .get_or_create(&[("cache", cache.to_owned()), ("result", result.to_owned())])
                .inc_by(n);
        }
    }

    /// Lookups so far of one cache and result (tests, load reports).
    pub fn cache_lookups(&self, cache: &str, result: &str) -> u64 {
        self.cache_lookups
            .get_or_create(&[("cache", cache.to_owned()), ("result", result.to_owned())])
            .get()
    }

    pub(crate) fn observe_config_change(&self, topic: &'static str) {
        self.config_changes
            .get_or_create(&[("topic", topic.to_owned())])
            .inc();
    }

    /// Fixed outcomes shared with the trace transport; never a destination or error string.
    pub fn otlp_metrics_push(&self, outcome: crate::metrics_export::Delivery) {
        self.otlp_metrics_pushes[outcome as usize].inc();
    }

    pub fn otlp_metrics_drop(&self, reason: crate::metrics_export::DropReason, n: u64) {
        self.otlp_metrics_dropped[reason as usize].inc_by(n);
    }

    pub fn otlp_metrics_rejected(&self, n: u64) {
        self.otlp_metrics_rejected_points.inc_by(n);
    }

    /// Fixed outcomes shared with the trace/metrics transports; never a destination or error string.
    pub(crate) fn otlp_logs_push(&self, outcome: crate::otlp::Delivery) {
        self.otlp_logs_pushes[outcome as usize].inc();
    }

    pub(crate) fn otlp_logs_drop(&self, reason: crate::ops_log::DropReason, n: u64) {
        self.otlp_logs_dropped[reason as usize].inc_by(n);
    }

    pub(crate) fn otlp_logs_rejected(&self, n: u64) {
        self.otlp_logs_rejected_records.inc_by(n);
    }

    /// `index` is an `OpsEvent` discriminant (see `OpsEvent::SLUGS`), not an
    /// arbitrary caller-chosen string.
    pub(crate) fn otlp_logs_suppressed(&self, index: usize, n: u64) {
        self.otlp_logs_suppressed[index].inc_by(n);
    }

    /// Pushes so far for one outcome (tests).
    pub fn otlp_logs_push_count(&self, outcome: crate::otlp::Delivery) -> u64 {
        self.otlp_logs_pushes[outcome as usize].get()
    }

    /// Drops so far for one reason (tests).
    pub fn otlp_logs_drop_count(&self, reason: crate::ops_log::DropReason) -> u64 {
        self.otlp_logs_dropped[reason as usize].get()
    }

    /// Collector-rejected records so far (tests).
    pub fn otlp_logs_rejected_count(&self) -> u64 {
        self.otlp_logs_rejected_records.get()
    }

    /// Suppressed-by-the-limiter count so far for one `OpsEvent` index (tests).
    pub fn otlp_logs_suppressed_count(&self, index: usize) -> u64 {
        self.otlp_logs_suppressed[index].get()
    }

    pub(crate) fn trace_drop(&self, reason: crate::telemetry::DropReason, n: u64) {
        self.trace_dropped_spans[reason as usize].inc_by(n);
    }

    pub(crate) fn trace_delivery(&self, outcome: crate::telemetry::Delivery) {
        self.trace_deliveries[outcome as usize].inc();
    }

    pub(crate) fn trace_rejected(&self, n: u64) {
        self.trace_rejected_spans.inc_by(n);
    }

    pub(crate) fn audit_export_delivery(&self, outcome: &'static str) {
        self.audit_export_deliveries
            .get_or_create(&[("outcome", outcome.to_owned())])
            .inc();
    }

    pub(crate) fn audit_export_skipped(&self, events: u64) {
        self.audit_export_deliveries
            .get_or_create(&[("outcome", "skipped".to_owned())])
            .inc_by(events);
    }

    /// Records an OTLP collector refused permanently (#108): delivered, not
    /// retried, so they count here instead.
    pub(crate) fn audit_export_rejected(&self, records: u64) {
        self.audit_export_rejected_records.inc_by(records);
    }

    /// Refused records counted so far (tests).
    #[cfg(test)]
    pub(crate) fn audit_export_rejected_count(&self) -> u64 {
        self.audit_export_rejected_records.get()
    }

    pub(crate) fn audit_export_backlog(&self, queued: i64, oldest_seconds: f64) {
        self.audit_export_queued.set(queued);
        self.audit_export_oldest.set(oldest_seconds);
    }

    pub(crate) fn egress_denied(&self, purpose: &'static str) {
        self.egress_denied
            .get_or_create(&[("purpose", purpose.to_owned())])
            .inc();
    }

    /// Refusals so far for one purpose (tests).
    pub fn egress_denied_count(&self, purpose: &str) -> u64 {
        self.egress_denied
            .get_or_create(&[("purpose", purpose.to_owned())])
            .get()
    }

    pub(crate) fn auth_throttled(&self, endpoint: &'static str, reason: &'static str) {
        self.auth_throttled
            .get_or_create(&[
                ("endpoint", endpoint.to_owned()),
                ("reason", reason.to_owned()),
            ])
            .inc();
    }

    /// Refusals so far of one sign-in step and reason (tests).
    pub fn auth_throttled_count(&self, endpoint: &str, reason: &str) -> u64 {
        self.auth_throttled
            .get_or_create(&[
                ("endpoint", endpoint.to_owned()),
                ("reason", reason.to_owned()),
            ])
            .get()
    }

    pub(crate) fn key_issuance_refused(&self, scope: &'static str, operation: &'static str) {
        self.key_issuance_refused
            .get_or_create(&[
                ("scope", scope.to_owned()),
                ("operation", operation.to_owned()),
            ])
            .inc();
    }

    /// Issuance refusals so far for one scope and operation (tests).
    pub fn key_issuance_refused_count(&self, scope: &str, operation: &str) -> u64 {
        self.key_issuance_refused
            .get_or_create(&[
                ("scope", scope.to_owned()),
                ("operation", operation.to_owned()),
            ])
            .get()
    }

    pub(crate) fn observe_config_poll_failure(&self) {
        self.config_poll_failures.inc();
    }

    pub(crate) fn set_config_listener(&self, up: bool) {
        self.config_listener.set(i64::from(up));
    }

    pub(crate) fn set_lease_held(&self, lease: &'static str, held: bool) {
        self.leases_held
            .get_or_create(&[("lease", lease.to_owned())])
            .set(i64::from(held));
    }

    pub(crate) fn observe_lease_term(&self, lease: &'static str) {
        self.lease_terms
            .get_or_create(&[("lease", lease.to_owned())])
            .inc();
    }

    pub(crate) fn observe_report_delivery(&self, outcome: &'static str, n: u64) {
        self.report_delivery_attempts
            .get_or_create(&[("outcome", outcome.to_owned())])
            .inc_by(n);
    }
    pub(crate) fn observe_report_outcome(&self, category: crate::scheduled_reports::Outcome) {
        self.report_outcomes
            .get_or_create(&[("category", category.as_str().to_owned())])
            .inc();
    }
    pub(crate) fn set_report_queue(&self, backlog: i64, capped: bool, lag: i64) {
        self.report_backlog.get_or_create(&[]).set(backlog);
        self.report_backlog_capped
            .get_or_create(&[])
            .set(i64::from(capped));
        self.report_oldest_due_lag.get_or_create(&[]).set(lag);
    }
    pub(crate) fn clear_report_queue(&self) {
        self.report_backlog.clear();
        self.report_backlog_capped.clear();
        self.report_oldest_due_lag.clear();
    }
    pub(crate) fn observe_background_run(&self, job: &'static str, result: &'static str) {
        self.background_runs
            .get_or_create(&[("job", job.to_owned()), ("result", result.to_owned())])
            .inc();
    }
}
