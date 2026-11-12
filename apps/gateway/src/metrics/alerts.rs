//! Alert runs, aged-hold alerts and alert email and webhook deliveries.
use super::*;

impl Metrics {
    pub(crate) fn observe_alert_webhook(&self, outcome: &'static str) {
        self.alert_webhook_attempts
            .get_or_create(&[("outcome", outcome.to_owned())])
            .inc();
    }

    /// System-health signal, never labeled by workspace, owner or recipient.
    pub(crate) fn observe_aged_hold_alerts(&self, result: &'static str, count: u64) {
        if matches!(
            result,
            "fired" | "escalated" | "cleared" | "deferred" | "suppressed"
        ) {
            self.aged_hold_alerts
                .get_or_create(&[("result", result.to_owned())])
                .inc_by(count);
        }
    }

    pub fn observe_alert_delivery(&self, outcome: &'static str) {
        self.alert_delivery_attempts
            .get_or_create(&[("outcome", outcome.to_owned())])
            .inc();
    }

    pub fn observe_alert_run(&self, result: &'static str, failed_rules: usize) {
        self.alert_runs
            .get_or_create(&[("result", result.to_owned())])
            .inc();
        self.alert_rule_failures.inc_by(failed_rules as u64);
    }
}
