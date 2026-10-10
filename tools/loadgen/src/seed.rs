//! `loadgen seed`: identities, keys, catalog, policies and optional settled
//! history, generated server-side into a THROWAWAY `omg_loadtest*` database.
//!
//! Connect as the schema owner (migrator), never as the runtime role. The
//! budget-totals triggers maintain `budget_totals` for every inserted row, so
//! `open-model-gateway budget verify` stays exact after seeding.
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::Serialize;
use sqlx::{Connection, PgConnection};

pub const SEED_SQL: &str = include_str!("../seed.sql");

#[derive(Clone, Debug, Serialize)]
pub struct SeedConfig {
    #[serde(skip)]
    pub key_seed: String,
    pub users: u32,
    pub shared_workspaces: u32,
    pub keys: u64,
    pub model: String,
    pub endpoint: String,
    pub policies: bool,
    /// Settled historical attempts (each: execution, reservation, two ledger rows).
    pub history: u64,
    /// History spans this many days before now (ending 10 minutes ago).
    pub history_days: u32,
    /// Of `history`, attempts left `unknown` (hold retained) in the previous month.
    pub history_unknown: u64,
    pub chunk: u64,
}

#[derive(Debug, Serialize)]
pub struct SeedReport {
    pub database: String,
    pub config: SeedConfig,
    pub identities_seconds: f64,
    pub history_seconds: f64,
    pub analyze_seconds: f64,
    pub users: i64,
    pub workspaces: i64,
    pub api_keys: i64,
    pub executions: i64,
    pub reservations: i64,
    pub ledger_entries: i64,
    pub database_bytes: i64,
}

pub fn valid_seed(seed: &str) -> bool {
    (1..=64).contains(&seed.len())
        && seed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Throwaway load-test databases only.
pub fn allowed_database(name: &str) -> bool {
    name.starts_with("omg_loadtest")
}

fn chunk_sql(c: &SeedConfig, lo: u64, hi: u64, partitioned_ledger: bool) -> String {
    // Only integers are interpolated; text parameters are session settings.
    let (days, keys, unknown) = (c.history_days.max(1), c.keys, c.history_unknown);
    // From migration 0030 the ledger carries its reservation's admission time.
    let (lcol, lval) = if partitioned_ledger {
        (", admitted_at", ", ts")
    } else {
        ("", "")
    };
    format!(
        r#"CREATE TEMP TABLE seed_chunk ON COMMIT DROP AS
  SELECT gen_random_uuid() AS id, k.id AS key_id, k.workspace_id, i < {unknown} AS unknown,
         CASE WHEN i < {unknown}
              THEN date_trunc('month', now(), 'UTC') - interval '1 day' - interval '1 second' * floor(random() * 20 * 86400)
              ELSE now() - interval '10 minutes' - interval '1 second' * floor(random() * {days} * 86400) END AS ts
  FROM generate_series({lo}, {hi}) i JOIN seed_keys k ON k.k = i % {keys};
INSERT INTO inference_executions(id, workspace_id, api_key_id, deployment_id, public_model, provider, streamed, state,
    error_code, root_request_id, started_at, completed_at, input_tokens, output_tokens, elapsed_ms, upstream_model)
  SELECT id, workspace_id, key_id, pg_temp.seed_id('deployment'), current_setting('omg_seed.model'), 'openai_compatible', false,
         CASE WHEN unknown THEN 'failed' ELSE 'succeeded' END, CASE WHEN unknown THEN 'upstream_unavailable' END,
         id, ts, ts + interval '50 milliseconds', CASE WHEN NOT unknown THEN 12 END, CASE WHEN NOT unknown THEN 4 END, 50, 'mock-chat'
  FROM seed_chunk;
INSERT INTO governance_reservations(execution_id, workspace_id, api_key_id, deployment_id, price_id, admitted_at, minute_start,
    month_start, lease_expires_at, state, reserved_tokens, held_microusd, actual_microusd, input_tokens, output_tokens)
  SELECT id, workspace_id, key_id, pg_temp.seed_id('deployment'), pg_temp.seed_id('price'), ts, date_trunc('minute', ts, 'UTC'),
         date_trunc('month', ts, 'UTC'), ts + interval '120 seconds', CASE WHEN unknown THEN 'unknown' ELSE 'settled' END,
         1016, 1032, CASE WHEN NOT unknown THEN 20 END, CASE WHEN NOT unknown THEN 12 END, CASE WHEN NOT unknown THEN 4 END
  FROM seed_chunk;
INSERT INTO monetary_ledger(id, execution_id, kind, amount_microusd, created_at{lcol})
  SELECT gen_random_uuid(), id, 'hold', 1032, ts{lval} FROM seed_chunk;
INSERT INTO monetary_ledger(id, execution_id, kind, amount_microusd, input_tokens, output_tokens, created_at{lcol})
  SELECT gen_random_uuid(), id, CASE WHEN unknown THEN 'unknown' ELSE 'settlement' END, CASE WHEN unknown THEN 1032 ELSE 20 END,
         CASE WHEN NOT unknown THEN 12 END, CASE WHEN NOT unknown THEN 4 END, ts + interval '50 milliseconds'{lval}
  FROM seed_chunk;"#
    )
}

pub async fn seed(url: &str, config: SeedConfig) -> anyhow::Result<SeedReport> {
    anyhow::ensure!(
        valid_seed(&config.key_seed),
        "seed must be 1-64 characters [A-Za-z0-9_-]"
    );
    anyhow::ensure!(config.users >= 1, "at least one user");
    anyhow::ensure!(config.keys >= 1, "at least one key");
    anyhow::ensure!(
        config.history_unknown <= config.history,
        "unknown history is part of --history"
    );
    anyhow::ensure!((1..=5_000_000).contains(&config.chunk), "chunk size");
    let mut conn = PgConnection::connect(url)
        .await
        .context("seed database connection")?;
    let (database,): (String,) = sqlx::query_as("SELECT current_database()")
        .fetch_one(&mut conn)
        .await?;
    anyhow::ensure!(
        allowed_database(&database),
        "refusing to seed database {database:?}: only throwaway omg_loadtest* databases"
    );
    for (name, value) in [
        ("omg_seed.seed", config.key_seed.clone()),
        ("omg_seed.users", config.users.to_string()),
        ("omg_seed.shared", config.shared_workspaces.to_string()),
        ("omg_seed.keys", config.keys.to_string()),
        ("omg_seed.model", config.model.clone()),
        ("omg_seed.endpoint", config.endpoint.clone()),
        (
            "omg_seed.policies",
            if config.policies { "on" } else { "off" }.into(),
        ),
    ] {
        sqlx::query("SELECT set_config($1, $2, false)")
            .bind(name)
            .bind(value)
            .execute(&mut conn)
            .await?;
    }
    let started = Instant::now();
    sqlx::raw_sql(SEED_SQL)
        .execute(&mut conn)
        .await
        .context("seeding identities, keys and catalog")?;
    let identities_seconds = started.elapsed().as_secs_f64();
    let (partitioned_ledger,): (bool,) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema='public' AND table_name='monetary_ledger' AND column_name='admitted_at')",
    )
    .fetch_one(&mut conn)
    .await?;
    // From 0032, every row written into an hour that already ended appends a
    // usage-rollup change marker (one per row). Bulk history seeding would
    // write millions; the throwaway seeder disables those two insert
    // triggers (owner DDL) and marks each seeded hour once instead, which is
    // exact because nothing else writes during seeding.
    let (rollups,): (bool,) =
        sqlx::query_as("SELECT to_regclass('public.usage_rollup_dirty') IS NOT NULL")
            .fetch_one(&mut conn)
            .await?;
    let bulk = rollups && config.history > 0;
    if bulk {
        sqlx::raw_sql("ALTER TABLE inference_executions DISABLE TRIGGER usage_rollup_execution_insert; ALTER TABLE governance_reservations DISABLE TRIGGER usage_rollup_reservation_insert")
            .execute(&mut conn)
            .await
            .context("pausing rollup markers")?;
    }
    let started = Instant::now();
    let mut lo = 0;
    while lo < config.history {
        let hi = (lo + config.chunk).min(config.history) - 1;
        sqlx::raw_sql(&chunk_sql(&config, lo, hi, partitioned_ledger))
            .execute(&mut conn)
            .await
            .with_context(|| format!("seeding history rows {lo}..={hi}"))?;
        lo = hi + 1;
        eprintln!(
            "seeded {lo}/{} history attempts ({:.0}s)",
            config.history,
            started.elapsed().as_secs_f64()
        );
    }
    if bulk {
        sqlx::raw_sql("INSERT INTO usage_rollup_dirty(hour_start) SELECT DISTINCT date_trunc('hour',started_at,'UTC') FROM inference_executions; ALTER TABLE inference_executions ENABLE TRIGGER usage_rollup_execution_insert; ALTER TABLE governance_reservations ENABLE TRIGGER usage_rollup_reservation_insert")
            .execute(&mut conn)
            .await
            .context("marking seeded hours for rollup")?;
    }
    let history_seconds = started.elapsed().as_secs_f64();
    let started = Instant::now();
    sqlx::raw_sql("ANALYZE")
        .execute(&mut conn)
        .await
        .context("ANALYZE")?;
    let analyze_seconds = started.elapsed().as_secs_f64();
    let counts: (i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM users), (SELECT count(*) FROM workspaces), (SELECT count(*) FROM api_keys),
                (SELECT count(*) FROM inference_executions), (SELECT count(*) FROM governance_reservations),
                (SELECT count(*) FROM monetary_ledger), pg_database_size(current_database())",
    )
    .fetch_one(&mut conn)
    .await?;
    let _ = tokio::time::timeout(Duration::from_secs(5), conn.close()).await;
    Ok(SeedReport {
        database,
        config,
        identities_seconds,
        history_seconds,
        analyze_seconds,
        users: counts.0,
        workspaces: counts.1,
        api_keys: counts.2,
        executions: counts.3,
        reservations: counts.4,
        ledger_entries: counts.5,
        database_bytes: counts.6,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guards() {
        assert!(allowed_database("omg_loadtest"));
        assert!(allowed_database("omg_loadtest_seeded"));
        for name in [
            "gateway",
            "gateway_enterprise_demo",
            "postgres",
            "omg_load_x",
        ] {
            assert!(!allowed_database(name), "{name}");
        }
        assert!(valid_seed("lt-1_A"));
        assert!(!valid_seed(""));
        assert!(!valid_seed("a'b"));
        assert!(!valid_seed(&"x".repeat(65)));
    }

    #[test]
    fn seed_sql_refuses_other_databases_server_side() {
        assert!(SEED_SQL.contains("current_database() !~ '^omg_loadtest'"));
        let sql = chunk_sql(
            &SeedConfig {
                key_seed: "s".into(),
                users: 1,
                shared_workspaces: 0,
                keys: 3,
                model: "m".into(),
                endpoint: "e".into(),
                policies: true,
                history: 10,
                history_days: 7,
                history_unknown: 2,
                chunk: 5,
            },
            5,
            9,
            true,
        );
        assert!(sql.contains("generate_series(5, 9)"));
        assert!(sql.contains("created_at, admitted_at"));
        assert!(sql.contains("i % 3"));
        assert!(sql.contains("i < 2"));
    }
}
