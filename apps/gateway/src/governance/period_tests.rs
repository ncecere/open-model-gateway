//! Budget rotation periods: UTC windows, per-layer composition and history.
use super::*;
use BudgetPeriod::{Day, Lifetime, Month, Week};
use chrono::TimeZone;
fn at(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, h, min, s).unwrap()
}
#[test]
fn windows_are_half_open_utc_day_iso_week_and_calendar_month() {
    let ms = chrono::TimeDelta::milliseconds(1);
    // UTC midnight.
    assert_eq!(
        Day.window(at(2026, 10, 8, 0, 0, 0) - ms),
        (at(2026, 10, 7, 0, 0, 0), at(2026, 10, 8, 0, 0, 0))
    );
    assert_eq!(
        Day.window(at(2026, 10, 8, 0, 0, 0)),
        (at(2026, 10, 8, 0, 0, 0), at(2026, 10, 9, 0, 0, 0))
    );
    // ISO week: Sunday 23:59:59.999 still belongs to the week from Monday;
    // Monday 00:00 UTC starts a new one, also across a year boundary.
    assert_eq!(
        Week.window(at(2026, 10, 12, 0, 0, 0) - ms),
        (at(2026, 10, 5, 0, 0, 0), at(2026, 10, 12, 0, 0, 0))
    );
    assert_eq!(
        Week.window(at(2026, 10, 12, 0, 0, 0)),
        (at(2026, 10, 12, 0, 0, 0), at(2026, 10, 19, 0, 0, 0))
    );
    assert_eq!(
        Week.window(at(2027, 1, 1, 12, 0, 0)),
        (at(2026, 12, 28, 0, 0, 0), at(2027, 1, 4, 0, 0, 0))
    );
    // Month end, year end and leap February.
    assert_eq!(
        Month.window(at(2026, 12, 31, 23, 59, 59)),
        (at(2026, 12, 1, 0, 0, 0), at(2027, 1, 1, 0, 0, 0))
    );
    assert_eq!(
        Month.window(at(2027, 1, 1, 0, 0, 0)),
        (at(2027, 1, 1, 0, 0, 0), at(2027, 2, 1, 0, 0, 0))
    );
    assert_eq!(
        Month.window(at(2028, 2, 29, 23, 0, 0)),
        (at(2028, 2, 1, 0, 0, 0), at(2028, 3, 1, 0, 0, 0))
    );
    assert_eq!(
        Month.window(at(2026, 4, 30, 23, 59, 59) + chrono::TimeDelta::seconds(1)),
        (at(2026, 5, 1, 0, 0, 0), at(2026, 6, 1, 0, 0, 0))
    );
}
#[test]
fn periods_round_trip_and_lifetime_spans_all_admissions() {
    for p in BudgetPeriod::ALL {
        assert_eq!(BudgetPeriod::parse(p.as_str()), Some(p));
    }
    assert_eq!(BudgetPeriod::parse("year"), None);
    let (start, end) = Lifetime.window(at(2026, 10, 8, 12, 0, 0));
    assert_eq!(start, DateTime::<Utc>::UNIX_EPOCH);
    assert_eq!(end, at(9999, 1, 1, 0, 0, 0));
}
#[cfg(feature = "integration-tests")]
mod db {
    use super::*;
    use crate::governance::tests::db::{Fixture, done, fixture, request};
    use sqlx::{PgPool, migrate::Migrator};
    /// A settled synthetic attempt admitted at SQL expression `when`.
    async fn spend(f: &Fixture, when: &str, actual: i64) -> Uuid {
        let id = Uuid::new_v4();
        let t: DateTime<Utc> = sqlx::query_scalar(&format!("SELECT {when}"))
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,started_at,root_request_id,attempt_number,input_tokens,output_tokens) VALUES($1,$2,$3,$4,'company/smart','openai',false,'succeeded',$5,$1,1,0,0)")
            .bind(id).bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).bind(t).execute(&f.store.pool).await.unwrap();
        sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,actual_microusd,input_tokens,output_tokens) VALUES($1,$2,$3,$4,$5,date_trunc('minute',$5,'UTC'),date_trunc('month',$5,'UTC'),$5,'settled',$6,0,0)")
            .bind(id).bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).bind(t).bind(actual).execute(&f.store.pool).await.unwrap();
        id
    }
    async fn move_to(f: &Fixture, id: Uuid, when: &str) {
        sqlx::query(&format!(
            "UPDATE governance_reservations SET admitted_at={when} WHERE execution_id=$1"
        ))
        .bind(id)
        .execute(&f.store.pool)
        .await
        .unwrap();
        sqlx::query(&format!(
            "UPDATE inference_executions SET started_at={when} WHERE id=$1"
        ))
        .bind(id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    }
    async fn period(f: &Fixture, table: &str, p: BudgetPeriod) {
        let layer = if table == "workspace_local_policies" {
            "local"
        } else {
            "override"
        };
        sqlx::query("UPDATE policy_budgets SET period=$1 WHERE layer=$2 AND workspace_id=$3")
            .bind(p.as_str())
            .bind(layer)
            .bind(f.principal.workspace_id)
            .execute(&f.store.pool)
            .await
            .unwrap();
    }
    async fn admit_once(f: &Fixture) -> Result<(), InferenceError> {
        admit(&f.store, &f.start(), &request(), 30).await
    }
    const DENIED: Result<(), InferenceError> =
        Err(InferenceError::BudgetExceeded(LimitScope::Workspace));
    /// The window start is inclusive and the instant before it is outside,
    /// using PostgreSQL's own UTC `date_trunc` (ISO weeks) as the oracle.
    async fn boundary(pool: PgPool, p: BudgetPeriod) {
        let f = fixture(pool).await;
        f.price(1_000_000).await; // each admission holds 110 micro-USD
        f.policy("workspace_local_policies", None, None, None, Some(200))
            .await;
        period(&f, "workspace_local_policies", p).await;
        let start = format!("date_trunc('{}',now(),'UTC')", p.as_str());
        let spent = spend(&f, &start, 100).await;
        assert_eq!(admit_once(&f).await, DENIED, "{p:?} start is inclusive");
        move_to(&f, spent, &format!("{start}-interval '1 microsecond'")).await;
        assert_eq!(admit_once(&f).await, Ok(()), "{p:?} previous window");
        // The admitted hold itself now counts in the current window.
        assert_eq!(admit_once(&f).await, DENIED);
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn day_window_starts_at_utc_midnight(pool: PgPool) {
        boundary(pool, Day).await;
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn week_window_starts_monday_utc(pool: PgPool) {
        boundary(pool, Week).await;
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn month_window_starts_on_the_first_utc(pool: PgPool) {
        boundary(pool, Month).await;
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn sql_and_rust_windows_agree(pool: PgPool) {
        for t in [
            at(2026, 10, 11, 23, 59, 59),
            at(2026, 10, 12, 0, 0, 0),
            at(2026, 12, 31, 23, 59, 59),
            at(2027, 1, 1, 0, 0, 0),
            at(2028, 2, 29, 12, 0, 0),
        ] {
            for p in [Day, Week, Month] {
                let start: DateTime<Utc> =
                    sqlx::query_scalar("SELECT date_trunc($1,$2::timestamptz,'UTC')")
                        .bind(p.as_str())
                        .bind(t)
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                assert_eq!(p.window(t).0, start, "{p:?} {t}");
            }
        }
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn layers_with_different_periods_each_apply(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        // Platform: 300 per month. Local: 250 per UTC day.
        f.policy(
            "workspace_platform_policy_overrides",
            None,
            None,
            None,
            Some(300),
        )
        .await;
        f.policy("workspace_local_policies", None, None, None, Some(250))
            .await;
        period(&f, "workspace_local_policies", Day).await;
        // Earlier this month but not today (or later this month on the 1st).
        let other_day = "CASE WHEN date_trunc('day',now(),'UTC')>date_trunc('month',now(),'UTC') THEN date_trunc('day',now(),'UTC')-interval '12 hours' ELSE date_trunc('day',now(),'UTC')+interval '36 hours' END";
        spend(&f, other_day, 200).await;
        // Day: 110 <= 250, but month: 200+110 > 300.
        assert_eq!(admit_once(&f).await, DENIED);
        f.policy(
            "workspace_platform_policy_overrides",
            None,
            None,
            None,
            Some(1_000),
        )
        .await;
        assert_eq!(admit_once(&f).await, Ok(()));
        assert_eq!(admit_once(&f).await, Ok(()));
        // Now the daily layer binds: 220+110 > 250 while the month has room.
        assert_eq!(admit_once(&f).await, DENIED);
        // A weekly key budget composes as a third independent window.
        set_test_budget(
            &f.store.pool,
            "key",
            None,
            Some(f.principal.workspace_id),
            Some(f.principal.key_id),
            "week",
            Some(100),
        )
        .await;
        f.policy("workspace_local_policies", None, None, None, Some(10_000))
            .await;
        assert_eq!(
            admit_once(&f).await,
            Err(InferenceError::BudgetExceeded(LimitScope::ApiKey))
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn changing_a_period_never_resets_or_rewrites_consumption(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        f.policy(
            "workspace_platform_policy_overrides",
            None,
            None,
            None,
            Some(200),
        )
        .await;
        let a = f.start();
        admit(&f.store, &a, &request(), 30).await.unwrap();
        finish(&f.store, &done(a.id, Some(100), Some(10)))
            .await
            .unwrap();
        assert_eq!(admit_once(&f).await, DENIED);
        let snapshot = |pool: PgPool| async move {
            sqlx::query_as::<_, (Uuid, String, Option<i64>, Option<i64>, DateTime<Utc>)>(
                "SELECT execution_id,state,held_microusd,actual_microusd,admitted_at FROM governance_reservations ORDER BY execution_id",
            )
            .fetch_all(&pool)
            .await
            .unwrap()
        };
        let ledger = |pool: PgPool| async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM monetary_ledger")
                .fetch_one(&pool)
                .await
                .unwrap()
        };
        let before = (
            snapshot(f.store.pool.clone()).await,
            ledger(f.store.pool.clone()).await,
        );
        for p in [Day, Week, Month, Day] {
            period(&f, "workspace_platform_policy_overrides", p).await;
            // Today's spend is inside every current window: still denied.
            assert_eq!(admit_once(&f).await, DENIED, "{p:?}");
        }
        assert_eq!(
            before,
            (
                snapshot(f.store.pool.clone()).await,
                ledger(f.store.pool.clone()).await
            )
        );
    }
    /// Several budgets on one layer are each enforced over their own window.
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn stacked_budgets_on_one_layer_each_apply(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await; // each admission holds 110 micro-USD
        let ws = f.principal.workspace_id;
        sqlx::query("INSERT INTO policy_budgets(layer,workspace_id,period,amount_microusd) VALUES('local',$1,'day',1000),('local',$1,'month',1000),('local',$1,'lifetime',300)").bind(ws).execute(&f.store.pool).await.unwrap();
        // Long-ago spend counts only toward the lifetime budget.
        spend(&f, "now()-interval '400 days'", 100).await;
        assert_eq!(admit_once(&f).await, Ok(())); // 100+110 <= 300
        assert_eq!(admit_once(&f).await, DENIED, "lifetime binds"); // 320 > 300
        sqlx::query("UPDATE policy_budgets SET amount_microusd=100000 WHERE period='lifetime'")
            .execute(&f.store.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE policy_budgets SET amount_microusd=250 WHERE period='day'")
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(admit_once(&f).await, Ok(())); // day 220 <= 250
        assert_eq!(admit_once(&f).await, DENIED, "day binds");
        sqlx::query("UPDATE policy_budgets SET amount_microusd=100000 WHERE period='day'")
            .execute(&f.store.pool)
            .await
            .unwrap();
        // Month 330 + 110 > 400 even though day/lifetime have room.
        sqlx::query("UPDATE policy_budgets SET amount_microusd=400 WHERE period='month'")
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(admit_once(&f).await, Ok(())); // month 330
        assert_eq!(admit_once(&f).await, DENIED, "month binds");
        // Exactly at the boundary admits (<=), one micro-USD less denies.
        sqlx::query("UPDATE policy_budgets SET amount_microusd=440 WHERE period='month'")
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(admit_once(&f).await, Ok(()));
        sqlx::query("UPDATE policy_budgets SET amount_microusd=549 WHERE period='month'")
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(admit_once(&f).await, DENIED);
    }
    /// Override budgets apply only while the replacement header exists; type
    /// budgets only without one. There is no installation layer (0026).
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn platform_budget_layers_follow_the_replacement_header(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        let ws = f.principal.workspace_id;
        let kind: String = sqlx::query_scalar("SELECT kind FROM workspaces WHERE id=$1")
            .bind(ws)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
        set_test_budget(
            &f.store.pool,
            "type",
            Some(&kind),
            None,
            None,
            "day",
            Some(1),
        )
        .await;
        assert_eq!(admit_once(&f).await, DENIED);
        // An all-null override replaces the type default (and its budgets).
        sqlx::query("INSERT INTO workspace_platform_policy_overrides(workspace_id) VALUES($1)")
            .bind(ws)
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(admit_once(&f).await, Ok(()));
        set_test_budget(
            &f.store.pool,
            "override",
            None,
            Some(ws),
            None,
            "week",
            Some(1),
        )
        .await;
        assert_eq!(admit_once(&f).await, DENIED);
        sqlx::query("DELETE FROM workspace_platform_policy_overrides")
            .execute(&f.store.pool)
            .await
            .unwrap();
        set_test_budget(&f.store.pool, "type", Some(&kind), None, None, "day", None).await;
        // Orphaned override budgets never apply without their header.
        assert_eq!(admit_once(&f).await, Ok(()));
    }
    type BudgetRow = (
        String,
        Option<String>,
        Option<Uuid>,
        Option<Uuid>,
        String,
        i64,
    );
    #[sqlx::test(migrations = false)]
    async fn migration_moves_single_budgets_into_stacked_rows(pool: PgPool) {
        let prefix = Migrator {
            migrations: std::borrow::Cow::Owned(
                crate::store::MIGRATOR.iter().take(4).cloned().collect(),
            ),
            ignore_missing: false,
            locking: true,
            no_tx: false,
        };
        prefix.run(&pool).await.unwrap();
        let (user, ws, key) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        sqlx::query("INSERT INTO users(id,email) VALUES($1,'migrate@test.invalid')")
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Team','team')")
            .bind(ws)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'k',decode(repeat('00',32),'hex'))").bind(key).bind(ws).bind(user).execute(&pool).await.unwrap();
        sqlx::query(
            "WITH i AS (INSERT INTO installation_policy(singleton,requests_per_minute,monthly_budget_microusd,budget_period) VALUES(true,7,1,'day') RETURNING 1),
             t AS (INSERT INTO workspace_type_policies(kind,monthly_budget_microusd,budget_period) VALUES('team',2,'week'),('project',NULL,'day') RETURNING 1),
             o AS (INSERT INTO workspace_platform_policy_overrides(workspace_id,monthly_budget_microusd) VALUES($1,3) RETURNING 1),
             l AS (INSERT INTO workspace_local_policies(workspace_id,monthly_budget_microusd,budget_period) VALUES($1,4,'day') RETURNING 1)
             INSERT INTO key_policies(workspace_id,governance_key_id,monthly_budget_microusd,budget_period) VALUES($1,$2,5,'week')",
        )
        .bind(ws)
        .bind(key)
        .execute(&pool)
        .await
        .unwrap();
        crate::store::MIGRATOR.run(&pool).await.unwrap();
        let rows: Vec<BudgetRow> = sqlx::query_as(
            "SELECT layer,kind,workspace_id,governance_key_id,period,amount_microusd FROM policy_budgets ORDER BY amount_microusd",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        // The installation budget moved in 0005 and was removed with the
        // installation layer in 0026 (recorded in the audit log).
        assert_eq!(
            rows,
            vec![
                (
                    "type".into(),
                    Some("team".into()),
                    None,
                    None,
                    "week".into(),
                    2
                ),
                ("override".into(), None, Some(ws), None, "month".into(), 3),
                ("local".into(), None, Some(ws), None, "day".into(), 4),
                ("key".into(), None, Some(ws), Some(key), "week".into(), 5),
            ]
        );
        // Installation rate limits are gone with their table, recorded once.
        let removed: serde_json::Value = sqlx::query_scalar(
            "SELECT metadata FROM audit_events WHERE action='policy.installation_removed'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(removed["requests_per_minute"], 7);
        assert_eq!(
            removed["budgets"],
            serde_json::json!([{"period":"day","amount_microusd":"1"}])
        );
        assert!(
            sqlx::query("SELECT 1 FROM installation_policy")
                .execute(&pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("SELECT monthly_budget_microusd FROM key_policies")
                .execute(&pool)
                .await
                .is_err()
        );
        for bad in [
            "INSERT INTO policy_budgets(layer,period,amount_microusd) VALUES('installation','day',9)",
            "INSERT INTO policy_budgets(layer,period,amount_microusd) VALUES('installation','year',9)",
            "INSERT INTO policy_budgets(layer,workspace_id,period,amount_microusd) VALUES('installation','00000000-0000-0000-0000-000000000001','month',9)",
            "INSERT INTO policy_budgets(layer,period,amount_microusd) VALUES('local','month',9)",
        ] {
            assert!(sqlx::query(bad).execute(&pool).await.is_err(), "{bad}");
        }
        assert_eq!(
            sqlx::query_scalar::<_, Option<chrono::DateTime<Utc>>>(
                "SELECT disabled_at FROM api_keys"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            None
        );
    }
}
