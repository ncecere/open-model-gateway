fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
    s.parse().unwrap()
}
use crate::scheduled_reports::calendar::Calendar;
#[test]
fn scheduled_reports_exact_closure_and_earlier_fold_equality() {
    let c = Calendar::new("weekly", "UTC", "00:00").unwrap();
    assert_eq!(
        c.delivery_after(at("2024-12-30T00:00:00Z")).unwrap(),
        at("2024-12-31T00:00:00Z")
    );
    let c = Calendar::new("weekly", "America/New_York", "01:30").unwrap();
    assert_eq!(
        c.delivery_after(at("2024-11-03T05:30:00Z")).unwrap(),
        at("2024-11-04T06:30:00Z")
    );
}
#[test]
fn scheduled_reports_failure_categories_are_bounded() {
    use crate::scheduled_reports::Outcome;
    for (error, expected) in [
        (sqlx::Error::PoolTimedOut, "timeout"),
        (
            sqlx::Error::Protocol("report snapshot exceeds limit".into()),
            "snapshot_too_large",
        ),
        (
            sqlx::Error::Protocol("report additive history unavailable".into()),
            "history_unavailable",
        ),
        (
            sqlx::Error::Protocol("report creator authority lost".into()),
            "authority_lost",
        ),
        (
            sqlx::Error::Protocol("arbitrary untrusted details".into()),
            "other",
        ),
    ] {
        assert_eq!(Outcome::classify(&error).as_str(), expected);
    }
}
#[cfg(feature = "integration-tests")]
mod db {
    use crate::scheduled_reports::{
        CANDIDATE_BATCH, SELECT_CANDIDATES_AFTER, SELECT_CANDIDATES_FIRST, SELECT_ELIGIBLE,
        begin_dispatch,
    };
    use chrono::{DateTime, Utc};
    use serde_json::Value;
    use sqlx::PgPool;
    use uuid::Uuid;
    fn inspect(v: &Value, found: &mut bool, work: &mut f64) {
        if let Some(name) = v["Index Name"].as_str() {
            if name == "report_schedules_dispatch_due" {
                *found = true;
            }
        }
        if v["Node Type"].as_str().is_some_and(|s| s.contains("Scan")) {
            *work += (v["Actual Rows"].as_f64().unwrap_or(0.)
                + v["Rows Removed by Filter"].as_f64().unwrap_or(0.))
                * v["Actual Loops"].as_f64().unwrap_or(1.);
        }
        if let Some(plans) = v["Plans"].as_array() {
            for p in plans {
                inspect(p, found, work);
            }
        }
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn scheduled_reports_review_due_index_plan_bounded_before_claims(pool: PgPool) {
        let user = Uuid::new_v4();
        sqlx::query("INSERT INTO users(id,email) VALUES($1,'report-plan@example.test')")
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO report_schedules(scope_kind,created_by,cadence,timezone,delivery_time,state,next_run_at) SELECT 'installation',$1,'weekly','UTC','09:00','active',CASE WHEN g<=5000 THEN now()-interval '1 day' ELSE now()+interval '1 year' END FROM generate_series(1,25000) g").bind(user).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO report_generation_claims(schedule_id,token,configuration_revision,period_start,period_end,skipped_intervals,expires_at) SELECT id,gen_random_uuid(),1,now()-interval '14 days',now()-interval '7 days',0,now()-interval '1 day' FROM report_schedules").execute(&pool).await.unwrap();
        sqlx::query("ANALYZE report_schedules")
            .execute(&pool)
            .await
            .unwrap();
        let mut tx = begin_dispatch(&pool).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>("SHOW statement_timeout")
                .fetch_one(&mut *tx)
                .await
                .unwrap(),
            "10s"
        );
        let cutoff: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        let page: Vec<(DateTime<Utc>, Uuid)> = sqlx::query_as(SELECT_CANDIDATES_FIRST)
            .bind(cutoff)
            .bind(CANDIDATE_BATCH)
            .fetch_all(&mut *tx)
            .await
            .unwrap();
        let ids = page.iter().map(|(_, id)| *id).collect::<Vec<_>>();
        let join_explain = format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {SELECT_ELIGIBLE}");
        let join: Value = sqlx::query_scalar(&join_explain)
            .bind(ids)
            .bind(cutoff)
            .bind(8i64)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        let (mut ignored, mut join_work) = (false, 0.);
        inspect(&join[0]["Plan"], &mut ignored, &mut join_work);
        assert!(
            join_work <= CANDIDATE_BATCH as f64 * 4.,
            "unbounded claim join: {join}"
        );
        for after in [false, true] {
            let sql = if after {
                SELECT_CANDIDATES_AFTER
            } else {
                SELECT_CANDIDATES_FIRST
            };
            let explain = format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {sql}");
            let query = sqlx::query_scalar::<_, Value>(&explain)
                .bind(cutoff)
                .bind(CANDIDATE_BATCH);
            let plan = if after {
                query
                    .bind(cutoff - chrono::TimeDelta::days(2))
                    .bind(Uuid::nil())
                    .fetch_one(&mut *tx)
                    .await
                    .unwrap()
            } else {
                query.fetch_one(&mut *tx).await.unwrap()
            };
            let (mut found, mut work) = (false, 0.);
            inspect(&plan[0]["Plan"], &mut found, &mut work);
            assert!(found, "{plan}");
            assert!(
                work <= CANDIDATE_BATCH as f64 * 4.,
                "unbounded scan work {work}: {plan}"
            );
        }
        sqlx::query("SET LOCAL statement_timeout='1ms'")
            .execute(&mut *tx)
            .await
            .unwrap();
        let error = sqlx::query("SELECT pg_sleep(0.05)")
            .execute(&mut *tx)
            .await
            .unwrap_err();
        assert_eq!(
            crate::scheduled_reports::Outcome::classify(&error),
            crate::scheduled_reports::Outcome::Timeout
        );
    }
}
