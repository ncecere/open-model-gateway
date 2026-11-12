//! Admission, settlement and accounting: phases, denials, deadlock retries and
//! scoped lock waits.
use super::*;

impl Metrics {
    /// One failed accounting operation (`gateway_accounting_failures_total`).
    pub(crate) fn observe_accounting_failure(
        &self,
        stage: crate::db::accounting_failure::Stage,
        cause: crate::db::accounting_failure::Cause,
    ) {
        self.accounting_failures
            .get_or_create(&[
                ("stage", stage.as_str().to_owned()),
                ("cause", cause.as_str().to_owned()),
            ])
            .inc();
    }

    #[cfg(test)]
    pub(crate) fn accounting_failure_count(
        &self,
        stage: crate::db::accounting_failure::Stage,
        cause: crate::db::accounting_failure::Cause,
    ) -> u64 {
        self.accounting_failures
            .get_or_create(&[
                ("stage", stage.as_str().to_owned()),
                ("cause", cause.as_str().to_owned()),
            ])
            .get()
    }

    #[cfg(test)]
    pub(crate) fn settlement_count(&self, settlement: Settlement) -> u64 {
        self.settlements
            .get_or_create(&[("outcome", settlement.as_str().to_owned())])
            .get()
    }

    pub fn observe_settlement(&self, settlement: Settlement) {
        self.settlements
            .get_or_create(&[("outcome", settlement.as_str().to_owned())])
            .inc();
    }

    /// Phase timings of one admission transaction (`gateway_admission_seconds`).
    pub(crate) fn observe_admission_phases(&self, timer: PhaseTimer, outcome: &'static str) {
        timer.observe(&self.admission_phases, outcome);
    }

    /// A governance transaction re-run after a deadlock (`path`: admission,
    /// settlement or reconciliation).
    pub(crate) fn observe_deadlock_retry(&self, path: &'static str) {
        self.deadlock_retries
            .get_or_create(&[("path", path.to_owned())])
            .inc();
    }

    /// A scoped lock acquisition is starting on `path`; the returned guard
    /// keeps `gateway_scope_lock_waiting{path}` raised until it is dropped
    /// (also on error or cancellation) and then observes the wait.
    pub(crate) fn scope_lock_wait(
        &'static self,
        lock: &'static str,
        path: &'static str,
    ) -> ScopeLockWait {
        self.scope_lock_waiting
            .get_or_create(&[("path", path.to_owned())])
            .inc();
        ScopeLockWait {
            metrics: self,
            lock,
            path,
            started: Instant::now(),
        }
    }

    /// Phase timings of one settlement transaction (`gateway_settlement_seconds`).
    pub(crate) fn observe_settlement_phases(&self, timer: PhaseTimer, outcome: &'static str) {
        timer.observe(&self.settlement_phases, outcome);
    }

    /// Observations so far of one admission phase and outcome (tests).
    #[cfg(any(test, feature = "integration-tests"))]
    pub fn admission_phase_count(&self, phase: &str, outcome: &str) -> u64 {
        self.histogram_count("admission_seconds", phase, outcome)
    }

    /// Observations so far of one settlement phase and outcome (tests).
    #[cfg(any(test, feature = "integration-tests"))]
    pub fn settlement_phase_count(&self, phase: &str, outcome: &str) -> u64 {
        self.histogram_count("settlement_seconds", phase, outcome)
    }

    /// Reads `_count` from the exposition (the histogram's own accessor is
    /// test-only upstream); absent series read 0.
    #[cfg(any(test, feature = "integration-tests"))]
    fn histogram_count(&self, metric: &str, phase: &str, outcome: &str) -> u64 {
        let mut out = String::new();
        if encode(&mut out, &self.registry).is_err() {
            return 0;
        }
        let prefix = format!("gateway_{metric}_count{{phase=\"{phase}\",outcome=\"{outcome}\"}} ");
        out.lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    }

    /// Admission denials only: rate/concurrency, budget, unresolved usage and
    /// oversize token reservations. Other admission errors are not denials.
    pub fn observe_admission_error(&self, error: InferenceError) {
        let Some(scope) = denial_scope(error) else {
            return;
        };
        self.denials
            .get_or_create(&[
                ("code", error.code().to_owned()),
                ("scope", scope.to_owned()),
            ])
            .inc();
    }

    /// Gateway-local concurrency limit (`GATEWAY_MAX_CONCURRENT_REQUESTS`).
    pub fn observe_capacity_denial(&self) {
        self.denials
            .get_or_create(&[
                ("code", InferenceError::Busy.code().to_owned()),
                ("scope", "gateway_capacity".to_owned()),
            ])
            .inc();
    }
}
