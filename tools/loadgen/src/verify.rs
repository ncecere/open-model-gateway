//! Durable-accounting checks for one run, keyed by the gateway's
//! `x-request-id` (= `inference_executions.root_request_id`).
//!
//! Read-only. Every successful request must have exactly one execution, one
//! settled reservation and exactly one hold and one settlement ledger entry,
//! with exact sums; denied requests must have no execution; and no
//! reservation admitted during the run may remain pending. `budget verify`
//! (maintained totals vs a full scan) is run separately by the runner.
use std::time::Duration;

use anyhow::Context;
use serde::Serialize;
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

use crate::run::RequestRecord;

#[derive(Debug, Default, Serialize)]
pub struct DatabaseCheck {
    pub ok_requests: u64,
    pub executions: i64,
    pub distinct_requests: i64,
    pub succeeded: i64,
    pub reservations: i64,
    pub settled: i64,
    pub exact_usage: i64,
    pub hold_entries: i64,
    pub settlement_entries: i64,
    pub other_entries: i64,
    pub held_sum_microusd: i64,
    pub hold_ledger_sum_microusd: i64,
    pub actual_sum_microusd: i64,
    pub settlement_ledger_sum_microusd: i64,
    /// Expected exact actual cost per request (v1 price with whole µUSD
    /// per-token rates and the mock's deterministic usage), if derivable.
    pub expected_actual_per_request_microusd: Option<i64>,
    pub denied_requests: u64,
    pub denied_with_execution: i64,
    /// Reservations admitted since the run started and still pending after
    /// a bounded wait for in-flight settlement.
    pub pending_after_run: i64,
    pub unknown_after_run: i64,
    pub executions_without_reservation: i64,
    pub violations: Vec<String>,
}

pub struct Database {
    pool: PgPool,
}

const CHUNK: usize = 5000;

impl Database {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(10))
            .connect(url)
            .await
            .context("verification database connection")?;
        Ok(Self { pool })
    }

    /// Database clock (epoch seconds) at the start of a run.
    pub async fn now(&self) -> anyhow::Result<f64> {
        let (seconds,): (f64,) =
            sqlx::query_as("SELECT extract(epoch FROM clock_timestamp())::float8")
                .fetch_one(&self.pool)
                .await?;
        Ok(seconds)
    }

    pub async fn check(
        &self,
        records: &[RequestRecord],
        since: f64,
        expected_usage: Option<(i64, i64)>,
    ) -> anyhow::Result<DatabaseCheck> {
        let ok: Vec<Uuid> = records
            .iter()
            .filter(|r| r.ok)
            .filter_map(|r| r.request_id)
            .collect();
        let denied: Vec<Uuid> = records
            .iter()
            .filter(|r| r.status == 429)
            .filter_map(|r| r.request_id)
            .collect();
        let mut c = DatabaseCheck {
            ok_requests: records.iter().filter(|r| r.ok).count() as u64,
            denied_requests: denied.len() as u64,
            ..DatabaseCheck::default()
        };
        // Streams may report `[DONE]` before settlement commits: wait (bounded)
        // until nothing admitted during the run is pending.
        for _ in 0..60 {
            let (pending,): (i64,) = sqlx::query_as(
                "SELECT count(*) FROM governance_reservations WHERE state='pending' AND admitted_at >= to_timestamp($1)",
            )
            .bind(since)
            .fetch_one(&self.pool)
            .await?;
            c.pending_after_run = pending;
            if pending == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let (unknown, orphans): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM governance_reservations WHERE state='unknown' AND admitted_at >= to_timestamp($1)),
                    (SELECT count(*) FROM inference_executions e WHERE e.started_at >= to_timestamp($1)
                       AND NOT EXISTS(SELECT 1 FROM governance_reservations r WHERE r.execution_id=e.id))",
        )
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        c.unknown_after_run = unknown;
        c.executions_without_reservation = orphans;
        let (prompt, completion) = expected_usage.unwrap_or((-1, -1));
        let mut rates = std::collections::BTreeSet::new();
        for chunk in ok.chunks(CHUNK) {
            let row: (i64, i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
                "SELECT count(*), count(DISTINCT e.root_request_id),
                        count(*) FILTER (WHERE e.state='succeeded'), count(r.execution_id),
                        count(*) FILTER (WHERE r.state='settled'),
                        count(*) FILTER (WHERE r.input_tokens=$2 AND r.output_tokens=$3),
                        coalesce(sum(r.held_microusd),0)::bigint, coalesce(sum(r.actual_microusd),0)::bigint
                 FROM inference_executions e LEFT JOIN governance_reservations r ON r.execution_id=e.id
                 WHERE e.root_request_id = ANY($1)",
            )
            .bind(chunk)
            .bind(prompt)
            .bind(completion)
            .fetch_one(&self.pool)
            .await?;
            c.executions += row.0;
            c.distinct_requests += row.1;
            c.succeeded += row.2;
            c.reservations += row.3;
            c.settled += row.4;
            c.exact_usage += row.5;
            c.held_sum_microusd += row.6;
            c.actual_sum_microusd += row.7;
            let ledger: (i64, i64, i64, i64, i64) = sqlx::query_as(
                "SELECT count(*) FILTER (WHERE l.kind='hold'), coalesce(sum(l.amount_microusd) FILTER (WHERE l.kind='hold'),0)::bigint,
                        count(*) FILTER (WHERE l.kind='settlement'), coalesce(sum(l.amount_microusd) FILTER (WHERE l.kind='settlement'),0)::bigint,
                        count(*) FILTER (WHERE l.kind NOT IN ('hold','settlement'))
                 FROM monetary_ledger l JOIN inference_executions e ON e.id=l.execution_id
                 WHERE e.root_request_id = ANY($1)",
            )
            .bind(chunk)
            .fetch_one(&self.pool)
            .await?;
            c.hold_entries += ledger.0;
            c.hold_ledger_sum_microusd += ledger.1;
            c.settlement_entries += ledger.2;
            c.settlement_ledger_sum_microusd += ledger.3;
            c.other_entries += ledger.4;
            let prices: Vec<(i16, Option<i64>, Option<i64>)> = sqlx::query_as(
                "SELECT DISTINCT p.pricing_version, p.input_microusd_per_million, p.output_microusd_per_million
                 FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id
                 JOIN deployment_prices p ON p.id=r.price_id WHERE e.root_request_id = ANY($1)",
            )
            .bind(chunk)
            .fetch_all(&self.pool)
            .await?;
            rates.extend(prices);
        }
        for chunk in denied.chunks(CHUNK) {
            let (n,): (i64,) = sqlx::query_as(
                "SELECT count(*) FROM inference_executions WHERE root_request_id = ANY($1)",
            )
            .bind(chunk)
            .fetch_one(&self.pool)
            .await?;
            c.denied_with_execution += n;
        }
        if let (Some((1, Some(i), Some(o))), 1, Some((p, q))) =
            (rates.iter().next().copied(), rates.len(), expected_usage)
            && i % 1_000_000 == 0
            && o % 1_000_000 == 0
        {
            c.expected_actual_per_request_microusd =
                Some(p * (i / 1_000_000) + q * (o / 1_000_000));
        }
        c.violations = c.violations();
        Ok(c)
    }
}

impl DatabaseCheck {
    fn violations(&self) -> Vec<String> {
        let ok = self.ok_requests as i64;
        let mut v = Vec::new();
        let mut expect = |name: &str, got: i64, want: i64| {
            if got != want {
                v.push(format!("{name}: {got} (expected {want})"));
            }
        };
        expect("executions for ok requests", self.executions, ok);
        expect("distinct ok request ids", self.distinct_requests, ok);
        expect("succeeded executions", self.succeeded, ok);
        expect("reservations for ok requests", self.reservations, ok);
        expect("settled reservations", self.settled, ok);
        expect("hold ledger entries", self.hold_entries, ok);
        expect("settlement ledger entries", self.settlement_entries, ok);
        expect("other ledger entries", self.other_entries, 0);
        expect(
            "hold ledger sum vs reservation holds",
            self.hold_ledger_sum_microusd,
            self.held_sum_microusd,
        );
        expect(
            "settlement ledger sum vs reservation actuals",
            self.settlement_ledger_sum_microusd,
            self.actual_sum_microusd,
        );
        expect(
            "denied requests with an execution",
            self.denied_with_execution,
            0,
        );
        expect(
            "pending reservations after the run",
            self.pending_after_run,
            0,
        );
        expect(
            "executions without a reservation",
            self.executions_without_reservation,
            0,
        );
        if self.exact_usage >= 0 && self.expected_actual_per_request_microusd.is_some() {
            expect(
                "reservations with the mock's exact usage",
                self.exact_usage,
                ok,
            );
        }
        if let Some(per) = self.expected_actual_per_request_microusd {
            expect("actual sum", self.actual_sum_microusd, per * ok);
        }
        v
    }
}
