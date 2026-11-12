//! Scrape-time collection of pool, alert-delivery, report-delivery, webhook
//! and reservation gauges from the database, and collector failures.
use super::*;

impl Metrics {
    /// Scrape-time or background collector failure (`collector` from a fixed set).
    pub(crate) fn observe_collection_error(&self, collector: &'static str) {
        self.collection_error(collector);
    }

    fn collection_error(&self, collector: &'static str) {
        self.collection_errors
            .get_or_create(&[("collector", collector.to_owned())])
            .inc();
    }

    #[cfg_attr(not(test), expect(clippy::too_many_lines, reason = "#499"))]
    pub(super) async fn refresh(&self, store: &Store) {
        let pool = &store.pool;
        let size = i64::from(pool.size());
        let idle = pool.num_idle() as i64;
        self.pool_connections
            .get_or_create(&[("state", "idle".to_owned())])
            .set(idle);
        self.pool_connections
            .get_or_create(&[("state", "in_use".to_owned())])
            .set((size - idle).max(0));
        self.pool_max
            .set(i64::from(pool.options().get_max_connections()));
        // Installation-wide gauge: with leased background work (serve) only
        // the `metrics` lease holder exports it, so summing over replicas
        // does not multiply it.
        if let Some(leases) = store.leases()
            && leases.held(crate::leases::Lease::Metrics).is_none()
        {
            self.reservations.clear();
            self.alert_delivery_queue.clear();
            self.report_delivery_queue.clear();
            self.alert_webhook_queue.clear();
            self.alert_webhook_oldest.clear();
            self.alert_pending_buckets.clear();
            self.alert_oldest_pending_age.clear();
            *self
                .reservations_refreshed
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            return;
        }
        {
            let mut refreshed = self
                .reservations_refreshed
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if refreshed.is_some_and(|at| at.elapsed() < RESERVATION_REFRESH) {
                return;
            }
            *refreshed = Some(Instant::now());
        }
        match tokio::time::timeout(
            Duration::from_secs(2),
            sqlx::query_as::<_, (i64, i64, i64, i64, i64, i64)>(ALERT_DELIVERY_COUNTS)
                .fetch_one(pool),
        )
        .await
        {
            Ok(Ok((retrying, exhausted, failed, suppressed, buckets, oldest_age))) => {
                self.alert_pending_buckets.get_or_create(&[]).set(buckets);
                self.alert_oldest_pending_age
                    .get_or_create(&[])
                    .set(oldest_age);
                for (state, count) in [
                    ("retrying", retrying),
                    ("exhausted", exhausted),
                    ("failed", failed),
                    ("suppressed", suppressed),
                ] {
                    self.alert_delivery_queue
                        .get_or_create(&[("state", state.to_owned())])
                        .set(count);
                }
            }
            _ => self.collection_error("alert_email"),
        }
        match tokio::time::timeout(Duration::from_secs(2), sqlx::query_as::<_,(String,i64)>("SELECT q.state,count(*) FROM (SELECT id FROM alert_deliveries WHERE report_run_id IS NOT NULL AND created_at>clock_timestamp()-interval '24 hours' ORDER BY created_at DESC,id LIMIT 1024) d JOIN alert_recipient_outbox q ON q.delivery_id=d.id GROUP BY q.state").fetch_all(pool)).await {
            Ok(Ok(rows)) => {
                for state in ["waiting","leased","sent","failed","ineligible","exhausted","not_configured"] {
                    self.report_delivery_queue.get_or_create(&[("state",state.to_owned())]).set(rows.iter().find(|(s,_)|s==state).map_or(0,|(_,n)|*n));
                }
            }
            _ => self.collection_error("report_email"),
        }
        match tokio::time::timeout(
            Duration::from_secs(2),
            sqlx::query_as::<_, (String, i64, i64)>(ALERT_WEBHOOK_COUNTS).fetch_all(pool),
        )
        .await
        {
            Ok(Ok(rows)) => {
                for (state, count, age) in rows {
                    if state == "waiting" {
                        self.alert_webhook_oldest.get_or_create(&[]).set(age);
                    }
                    self.alert_webhook_queue
                        .get_or_create(&[("state", state)])
                        .set(count);
                }
            }
            _ => self.collection_error("alert_webhook"),
        }
        // The workspace lifetime totals rows (0015) count every pending and
        // unknown reservation exactly (each belongs to one workspace): one row
        // per workspace through the 0026 window index, not a scan of history.
        let counted = tokio::time::timeout(
            Duration::from_secs(2),
            sqlx::query_as::<_, (i64, i64)>(crate::governance::totals::RESERVATION_COUNTS)
                .fetch_one(pool),
        )
        .await;
        match counted {
            Ok(Ok((pending, unknown))) => {
                for (state, n) in [("pending", pending), ("unknown", unknown)] {
                    self.reservations
                        .get_or_create(&[("state", state.to_owned())])
                        .set(n);
                }
            }
            _ => self.collection_error("reservations"),
        }
    }
}
