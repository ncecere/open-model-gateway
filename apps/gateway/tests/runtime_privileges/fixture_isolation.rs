//! #382: the complete embedded verifier must coexist with legitimate incidents.
//! Each caller gets its own SQLx-migrated disposable database. Run this target
//! serially with --ignored --test-threads=1 on the dedicated role/ACL cluster.
use std::panic::AssertUnwindSafe;

use futures_util::FutureExt;
use open_model_gateway::store::Store;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

pub(super) async fn run(pool: PgPool, personal: bool, scim: bool) {
    // This is the existing runtime test's cluster-wide role/maintenance ACL
    // boundary. Detach as well: a panic must never return a locked session.
    let mut lock = pool.acquire().await.unwrap().detach();
    sqlx::query("SELECT pg_advisory_lock(72419505)")
        .execute(&mut lock)
        .await
        .unwrap();
    let result = AssertUnwindSafe(async {
        sqlx::raw_sql(
            "DO $$ BEGIN
               IF NOT EXISTS(SELECT FROM pg_roles WHERE rolname='gateway_runtime') THEN
                CREATE ROLE gateway_runtime NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
               END IF;
             END $$;
             REVOKE CONNECT ON DATABASE postgres FROM PUBLIC,gateway_runtime;
             REVOKE CONNECT ON DATABASE template1 FROM PUBLIC,gateway_runtime;
             DO $$ BEGIN
               EXECUTE format('REVOKE ALL ON DATABASE %I FROM PUBLIC,gateway_runtime',current_database());
               EXECUTE format('GRANT CONNECT ON DATABASE %I TO gateway_runtime',current_database());
             END $$;",
        )
        .execute(&pool)
        .await
        .unwrap();
        let safe_runtime: bool = sqlx::query_scalar(
            "SELECT NOT (rolcanlogin OR rolsuper OR rolcreatedb OR rolcreaterole OR rolreplication OR rolbypassrls)
             AND NOT EXISTS(SELECT FROM pg_auth_members WHERE member=r.oid)
             FROM pg_roles r WHERE rolname='gateway_runtime'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(safe_runtime, "dedicated gateway_runtime must remain NOLOGIN and unprivileged");
        let store = Store::new(pool.clone());
        store.migrate_enterprise().await.unwrap();
        store.preflight_enterprise().await.unwrap();
        store.apply_runtime_grants().await.unwrap();
        let runtime_temp: bool = sqlx::query_scalar(
            "SELECT has_database_privilege('gateway_runtime',current_database(),'TEMPORARY')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!runtime_temp, "runtime must not regain database TEMP");
        check_incidents(&pool, &store, personal, scim).await;
    })
    .catch_unwind()
    .await;
    let unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock(72419505)")
        .fetch_one(&mut lock)
        .await
        .unwrap();
    assert!(
        unlocked,
        "release the shared role/maintenance ACL lock explicitly"
    );
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn check_incidents(pool: &PgPool, store: &Store, personal: bool, scim: bool) {
    let mut seeded = Vec::new();
    let mut open = Vec::new();
    if personal {
        // Same identity at both valid levels: 0058 changes severity/threshold,
        // not the open-incident key. Keep both levels as resolved history too.
        // Personal-only covers open level 2; the combined case covers level 1.
        let open_level = if scim { 1 } else { 2 };
        for (level, resolved) in [(1, true), (2, true), (open_level, false)] {
            let id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO alert_events(id,kind,subject_key,level,severity,summary,details,fired_at,resolved_at,resolution)
                 VALUES($1,'aged_unknown_hold','personal',$2,
                  CASE $2 WHEN 1 THEN 'warning' ELSE 'critical' END,
                  CASE $2 WHEN 1 THEN 'Holds with unknown cost are over 24 hours old'
                   ELSE 'Holds with unknown cost are over 168 hours old' END,
                  jsonb_build_object('threshold_hours',CASE $2 WHEN 1 THEN 24 ELSE 168 END),
                  clock_timestamp()-interval '2 days',
                  CASE WHEN $3 THEN clock_timestamp()-interval '1 day' END,
                  CASE WHEN $3 THEN 'cleared' END)",
            )
            .bind(id)
            .bind(level)
            .bind(resolved)
            .execute(pool)
            .await
            .unwrap();
            seeded.push(id);
            if !resolved {
                open.push(id);
            }
        }
    }
    if scim {
        for resolved in [true, false] {
            let id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary,fired_at,resolved_at,resolution)
                 VALUES($1,'scim_last_admin','scim_last_admin','scim_last_admin',1,'critical',
                  'SCIM tried to remove the last Platform Admin',clock_timestamp()-interval '2 days',
                  CASE WHEN $2 THEN clock_timestamp()-interval '1 day' END,
                  CASE WHEN $2 THEN 'cleared' END)",
            )
            .bind(id)
            .bind(resolved)
            .execute(pool)
            .await
            .unwrap();
            seeded.push(id);
            if !resolved {
                open.push(id);
            }
        }
    }

    // Local database evidence only: a reference, never an actual credential or
    // network send. The normal delivery trigger must construct webhook bytes.
    sqlx::query(
        "INSERT INTO alert_webhook_destinations(label,url,current_secret_ref,enabled)
         VALUES('Issue 382 fixture','https://fixture.invalid/hook','env:ISSUE_382_UNUSED',true)",
    )
    .execute(pool)
    .await
    .unwrap();
    for id in &seeded {
        let delivery = Uuid::new_v4();
        sqlx::query("INSERT INTO alert_deliveries(id,event_id,transition) VALUES($1,$2,'fired')")
            .bind(delivery)
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO alert_recipient_outbox(delivery_id,fingerprint)
             VALUES($1,sha256('issue-382-fixture'::bytea))",
        )
        .bind(delivery)
        .execute(pool)
        .await
        .unwrap();
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM alert_webhook_outbox")
            .fetch_one(pool)
            .await
            .unwrap(),
        seeded.len() as i64,
        "every seeded incident must have trigger-generated webhook evidence"
    );

    // Snapshot equality alone would miss UPDATE/resolve/DELETE followed by
    // rollback. This test-only BEFORE guard sorts before production triggers
    // and refuses *any* attempt on exactly our fresh UUIDs, including no-ops.
    let ids = seeded
        .iter()
        .map(|id| format!("'{id}'::uuid"))
        .collect::<Vec<_>>()
        .join(",");
    sqlx::raw_sql(&format!(
        "CREATE FUNCTION issue_382_refuse_incident_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'verifier attempted to mutate an existing incident' USING ERRCODE='P0382'; END $$;
         CREATE TRIGGER aaa_issue_382_incident_guard BEFORE UPDATE OR DELETE ON alert_events
         FOR EACH ROW WHEN (OLD.id IN ({ids})) EXECUTE FUNCTION issue_382_refuse_incident_mutation();"
    ))
    .execute(pool)
    .await
    .unwrap();

    let before = snapshot(pool).await;
    assert_open_identities(pool, &open).await;
    let mut failures = Vec::new();
    for attempt in 1..=2 {
        // Do not copy or shorten the SQL: all webhook, dedup, privilege and
        // rollback assertions must execute via the operator's actual path.
        let verification = store.verify_runtime_grants().await;
        assert_eq!(
            snapshot(pool).await,
            before,
            "verifier attempt {attempt} changed durable evidence or left synthetic probe rows"
        );
        assert_open_identities(pool, &open).await;
        let probe_lease: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT FROM work_leases WHERE name='privilege_probe')",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        assert!(
            !probe_lease,
            "rollback-only privilege_probe lease must be absent"
        );
        if let Err(error) = verification {
            failures.push(format!("attempt {attempt}: {error:#?}"));
        }
    }
    // Expected RED before the two INSERT fixes: native PostgreSQL 23505 on
    // alert_events_open (SCIM first when both fixtures exist), not P0382.
    assert!(
        failures.is_empty(),
        "complete verifier must succeed twice without touching existing incidents: {failures:#?}"
    );
}

async fn assert_open_identities(pool: &PgPool, expected: &[Uuid]) {
    let actual: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM alert_events WHERE resolved_at IS NULL ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();
    let mut expected = expected.to_vec();
    expected.sort();
    assert_eq!(
        actual, expected,
        "the original open incident identities must remain"
    );
}

async fn snapshot(pool: &PgPool) -> Vec<(&'static str, Vec<Value>)> {
    let mut snapshot = Vec::new();
    // Whole-table, every-column snapshots also detect leaked synthetic UUIDs,
    // deliveries and outboxes, not just edits to the seeded rows. Accounting
    // relations stay at their migrated baseline; no unrelated fixtures needed.
    for table in [
        "alert_events",
        "alert_deliveries",
        "alert_recipient_outbox",
        "alert_webhook_outbox",
        "alert_webhook_destinations",
        "alert_reads",
        "deployment_prices",
        "inference_executions",
        "monetary_ledger",
        "governance_reservations",
        "budget_totals",
        "rate_minute_counters",
        "inflight_counters",
        "configuration_changes",
        "config_versions",
        "work_leases",
    ] {
        let rows = sqlx::query_scalar::<_, Value>(&format!(
            "SELECT to_jsonb(t) FROM {table} t ORDER BY to_jsonb(t)::text"
        ))
        .fetch_all(pool)
        .await
        .unwrap();
        snapshot.push((table, rows));
    }
    snapshot
}
