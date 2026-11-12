//! File store operations and batch jobs.
use super::*;

impl Metrics {
    /// File store operation outcome. All labels come from fixed sets: backend
    /// kind, operation name and `FileStoreError::code()`.
    pub fn observe_file_store(
        &self,
        backend: &'static str,
        op: &'static str,
        error: Option<&'static str>,
    ) {
        self.file_store_ops
            .get_or_create(&[
                ("backend", backend.to_owned()),
                ("op", op.to_owned()),
                ("outcome", error.unwrap_or("ok").to_owned()),
            ])
            .inc();
    }

    pub fn observe_file_store_bytes(&self, backend: &'static str, op: &'static str, bytes: u64) {
        self.file_store_bytes
            .get_or_create(&[("backend", backend.to_owned()), ("op", op.to_owned())])
            .inc_by(bytes);
    }

    pub fn observe_batch_created(&self, mode: &'static str) {
        self.batches
            .get_or_create(&[("mode", mode.to_owned()), ("event", "created".to_owned())])
            .inc();
    }
    pub fn observe_batch_submitted(&self, _provider: &str) {
        self.batches
            .get_or_create(&[
                ("mode", "native".to_owned()),
                ("event", "submitted".to_owned()),
            ])
            .inc();
    }
    /// `state` is a terminal job state (fixed set).
    pub fn observe_batch_finished(&self, mode: &'static str, state: &'static str) {
        self.batches
            .get_or_create(&[("mode", mode.to_owned()), ("event", state.to_owned())])
            .inc();
    }
    pub fn observe_batch_lines(
        &self,
        mode: &'static str,
        provider: &str,
        completed: u64,
        failed: u64,
    ) {
        let provider = bounded(&self.providers, provider);
        for (outcome, n) in [("completed", completed), ("failed", failed)] {
            if n > 0 {
                self.batch_lines
                    .get_or_create(&[
                        ("mode", mode.to_owned()),
                        ("provider", provider.clone()),
                        ("outcome", outcome.to_owned()),
                    ])
                    .inc_by(n);
            }
        }
    }
    pub fn set_batch_queue(&self, mode: &'static str, unfinished: u64) {
        self.batch_queue
            .get_or_create(&[("mode", mode.to_owned())])
            .set(unfinished.min(i64::MAX as u64) as i64);
    }
    /// Batch lines waiting and running per provider kind (`provider`,
    /// waiting, running); kinds no longer present read 0.
    pub fn set_batch_route_lines(&self, rows: &[(String, u64, u64)]) {
        let mut seen = self
            .batch_route_providers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for provider in seen.iter() {
            for state in ["waiting", "running"] {
                self.batch_route_lines
                    .get_or_create(&[("provider", provider.clone()), ("state", state.to_owned())])
                    .set(0);
            }
        }
        for (provider, waiting, running) in rows {
            let provider = bounded(&self.providers, provider);
            seen.insert(provider.clone());
            for (state, n) in [("waiting", waiting), ("running", running)] {
                self.batch_route_lines
                    .get_or_create(&[("provider", provider.clone()), ("state", state.to_owned())])
                    .set((*n).min(i64::MAX as u64) as i64);
            }
        }
    }
    /// Routes paused per reason (fixed set); absent reasons read 0.
    pub fn set_batch_paused_routes(&self, rows: &[(&'static str, u64)]) {
        for reason in [
            "outside_window",
            "live_traffic",
            "server_busy",
            "metrics_unavailable",
            "concurrency",
            "fair_share",
            "workers",
            "rate_limited",
        ] {
            let n = rows.iter().find(|r| r.0 == reason).map_or(0, |r| r.1);
            self.batch_paused_routes
                .get_or_create(&[("reason", reason.to_owned())])
                .set(n.min(i64::MAX as u64) as i64);
        }
    }
    /// A route's batch gate closed (`reason` from a fixed set).
    pub fn observe_batch_route_pause(&self, reason: &'static str) {
        self.batch_route_pauses
            .get_or_create(&[("reason", reason.to_owned())])
            .inc();
    }
    pub fn set_batch_workers(&self, capacity: u64, busy: u64) {
        self.batch_workers
            .get_or_create(&[("state", "capacity".to_owned())])
            .set(capacity as i64);
        self.batch_workers
            .get_or_create(&[("state", "busy".to_owned())])
            .set(busy as i64);
    }
}
