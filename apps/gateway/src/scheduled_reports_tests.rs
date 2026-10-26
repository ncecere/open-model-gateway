//! Scheduled-report contracts: run database tests with disposable PostgreSQL.
use crate::scheduled_reports::calendar::Calendar;
use chrono::{DateTime, Utc};
fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

#[test]
fn scheduled_reports_calendar_month_ends_leap_year_and_utc_windows() {
    let c = Calendar::new("monthly", "UTC", "09:00").unwrap();
    let o = c.next_after(at("2024-02-29T23:59:59Z"), None).unwrap();
    assert_eq!(o.start, at("2024-02-01T00:00:00Z"));
    assert_eq!(o.end, at("2024-03-01T00:00:00Z"));
    assert_eq!(o.due, at("2024-03-01T09:00:00Z"));
    assert_eq!(
        c.next_after(o.due, Some(o.end)).unwrap().end,
        at("2024-04-01T00:00:00Z")
    );
    let w = Calendar::new("weekly", "Pacific/Kiritimati", "09:00").unwrap();
    let o = w.next_after(at("2024-12-29T23:59:59Z"), None).unwrap();
    assert_eq!(o.start, at("2024-12-23T00:00:00Z"));
    assert_eq!(o.end, at("2024-12-30T00:00:00Z"));
    assert_eq!(o.due, at("2024-12-30T19:00:00Z"));
    assert!(Calendar::new("daily", "UTC", "09:00").is_err());
    // Latest-complete catch-up waits for its delivery preference rather than
    // emitting the prior week and then another report a few hours later.
    let c = Calendar::new("weekly", "UTC", "09:00").unwrap();
    let latest = c.latest_complete(at("2024-12-30T01:00:00Z")).unwrap();
    assert_eq!(latest.end, at("2024-12-30T00:00:00Z"));
    assert_eq!(latest.due, at("2024-12-30T09:00:00Z"));
}
#[test]
fn scheduled_reports_calendar_delivery_dst_gaps_forward_and_folds_earlier_once() {
    // Daily delivery preference is resolved separately from the UTC cadence.
    let c = Calendar::new("weekly", "America/New_York", "02:30").unwrap();
    assert_eq!(
        c.delivery_after(at("2024-03-10T00:00:00Z")).unwrap(),
        at("2024-03-10T07:30:00Z")
    );
    let c = Calendar::new("monthly", "America/New_York", "01:30").unwrap();
    assert_eq!(
        c.delivery_after(at("2024-11-03T00:00:00Z")).unwrap(),
        at("2024-11-03T05:30:00Z")
    );
    // Closure between the two fold instants does not schedule the second fold.
    assert_eq!(
        c.delivery_after(at("2024-11-03T06:00:00Z")).unwrap(),
        at("2024-11-04T06:30:00Z")
    );
    let c = Calendar::new("monthly", "Australia/Lord_Howe", "02:15").unwrap();
    assert_eq!(
        c.delivery_after(at("2024-10-05T14:00:00Z")).unwrap(),
        at("2024-10-05T15:45:00Z")
    );
}

#[cfg(feature = "integration-tests")]
mod db {
    use super::*;
    use crate::{
        leases::{Fence, Lease, Leases},
        scheduled_reports::{self, Claim, complete, snapshot},
        store::Store,
    };
    use serde_json::Value;
    use sqlx::PgPool;
    use uuid::Uuid;
    fn scheduling_now() -> DateTime<Utc> {
        // Generation fixtures require the latest complete period's delivery to be due.
        at("2026-10-27T12:00:00Z")
    }
    async fn claim(store: &Store, fence: &Fence, id: Uuid) -> Result<Option<Claim>, sqlx::Error> {
        scheduled_reports::claim_at(store, fence, id, scheduling_now()).await
    }
    async fn tick(store: &Store, fence: &Fence) -> Result<usize, sqlx::Error> {
        scheduled_reports::tick_at(store, fence, scheduling_now()).await
    }
    async fn setup(pool: &PgPool) -> (Store, Fence, Uuid, Uuid) {
        let user = Uuid::new_v4();
        sqlx::query("INSERT INTO users(id,email) VALUES($1,'report-generator@example.test')")
            .bind(user)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'admin','manual')",
        )
        .bind(user)
        .execute(pool)
        .await
        .unwrap();
        let id: Uuid = sqlx::query_scalar("INSERT INTO report_schedules(scope_kind,created_by,cadence,timezone,delivery_time,state) VALUES('installation',$1,'weekly','UTC','00:01','active') RETURNING id").bind(user).fetch_one(pool).await.unwrap();
        sqlx::query(
            "INSERT INTO report_schedule_recipients(schedule_id,slot,user_id) VALUES($1,1,$2)",
        )
        .bind(id)
        .bind(user)
        .execute(pool)
        .await
        .unwrap();
        let leases = Leases::new();
        leases.renew_once(pool).await;
        let fence = leases.held(Lease::ReportScheduler).unwrap();
        let store = Store::new(pool.clone());
        tick(&store, &fence).await.unwrap(); // initialize, never replay on creation
        assert_eq!(count(pool).await, 0);
        assert_eq!(
            cursor(pool, id).await,
            (
                Some(at("2026-11-02T00:01:00Z")),
                Some(at("2026-11-02T00:00:00Z")),
                None,
            ),
            "initialization schedules the next occurrence without replay"
        );
        sqlx::query("UPDATE report_schedules SET next_period_end=date_trunc('week',$2::timestamptz,'UTC')-interval '28 days',next_run_at=date_trunc('week',$2::timestamptz,'UTC')-interval '28 days'+interval '1 minute' WHERE id=$1").bind(id).bind(scheduling_now()).execute(pool).await.unwrap();
        (store, fence, id, user)
    }
    async fn cursor(
        pool: &PgPool,
        id: Uuid,
    ) -> (
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
    ) {
        sqlx::query_as(
            "SELECT next_run_at,next_period_end,last_period_end FROM report_schedules WHERE id=$1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }
    async fn count(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM report_runs")
            .fetch_one(pool)
            .await
            .unwrap()
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn scheduled_reports_delivery_boundary_defers_then_catches_up_once(pool: PgPool) {
        let (s, f, id, _) = setup(&pool).await;
        let before = at("2026-10-26T00:00:30Z");
        let due = at("2026-10-26T00:01:00Z");
        let original_end = Some(at("2026-09-28T00:00:00Z"));
        assert_eq!(cursor(&pool, id).await.1, original_end);
        assert!(
            scheduled_reports::claim_at(&s, &f, id, before)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(scheduled_reports::tick_at(&s, &f, before).await.unwrap(), 0);
        assert_eq!(count(&pool).await, 0);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM report_generation_claims")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            cursor(&pool, id).await,
            (Some(due), original_end, None),
            "deferral must preserve the original period cursor for skipped counting"
        );

        let (a, b) = tokio::join!(
            scheduled_reports::tick_at(&s, &f, due),
            scheduled_reports::tick_at(&s, &f, due)
        );
        assert_eq!(a.unwrap() + b.unwrap(), 1);
        assert_eq!(count(&pool).await, 1);
        let occurrence: (DateTime<Utc>, DateTime<Utc>, i64) = sqlx::query_as(
            "SELECT period_start,period_end,skipped_intervals FROM report_runs WHERE schedule_id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            occurrence,
            (at("2026-10-19T00:00:00Z"), at("2026-10-26T00:00:00Z"), 4)
        );
        let completed_cursor = (
            Some(at("2026-11-02T00:01:00Z")),
            Some(at("2026-11-02T00:00:00Z")),
            Some(at("2026-10-26T00:00:00Z")),
        );
        assert_eq!(cursor(&pool, id).await, completed_cursor);
        assert!(
            scheduled_reports::claim_at(&s, &f, id, due)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(scheduled_reports::tick_at(&s, &f, due).await.unwrap(), 0);
        let (a, b) = tokio::join!(
            scheduled_reports::tick_at(&s, &f, due),
            scheduled_reports::tick_at(&s, &f, due)
        );
        assert_eq!((a.unwrap(), b.unwrap()), (0, 0));
        assert_eq!(count(&pool).await, 1);
        assert_eq!(cursor(&pool, id).await, completed_cursor);
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn scheduled_reports_frozen_scheduling_preserves_claim_expiry_and_lease_fences(
        pool: PgPool,
    ) {
        let (s, f, id, _) = setup(&pool).await;
        let _claim = claim(&s, &f, id).await.unwrap().unwrap();
        let (token, expires, live): (Uuid, DateTime<Utc>, bool) = sqlx::query_as(
            "SELECT token,expires_at,expires_at>clock_timestamp() AND expires_at<=clock_timestamp()+interval '30 seconds' FROM report_generation_claims WHERE schedule_id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(live, "claim TTL is anchored to real database time");
        let advanced = expires.max(scheduling_now()) + chrono::TimeDelta::days(14);
        assert!(advanced > expires);
        assert!(
            scheduled_reports::claim_at(&s, &f, id, advanced)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            scheduled_reports::tick_at(&s, &f, advanced).await.unwrap(),
            0
        );
        let unchanged: (Uuid, DateTime<Utc>) = sqlx::query_as(
            "SELECT token,expires_at FROM report_generation_claims WHERE schedule_id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            unchanged,
            (token, expires),
            "scheduling cannot steal a live claim"
        );
        assert_eq!(count(&pool).await, 0);

        // Expire ownership using database time, not the frozen scheduling instant.
        sqlx::query("UPDATE report_generation_claims SET expires_at=clock_timestamp()-interval '1 second' WHERE schedule_id=$1")
            .bind(id).execute(&pool).await.unwrap();
        let _replacement = scheduled_reports::claim_at(&s, &f, id, advanced)
            .await
            .unwrap()
            .unwrap();
        let replacement_token: Uuid =
            sqlx::query_scalar("SELECT token FROM report_generation_claims WHERE schedule_id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_ne!(replacement_token, token);
        sqlx::query("SELECT omg_lease_release('report_scheduler',$1,$2)")
            .bind(f.holder)
            .bind(f.epoch)
            .execute(&pool)
            .await
            .unwrap();
        let error = match scheduled_reports::claim_at(&s, &f, id, advanced).await {
            Err(error) => error,
            Ok(_) => panic!("frozen scheduling must not bypass a released lease"),
        };
        assert!(crate::leases::is_fenced(&error));
        assert!(crate::leases::is_fenced(
            &scheduled_reports::tick_at(&s, &f, advanced)
                .await
                .unwrap_err()
        ));
        assert_eq!(count(&pool).await, 0);
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn scheduled_reports_generation_two_replicas_idempotent_catchup_lineage(pool: PgPool) {
        let (s, f, id, _) = setup(&pool).await;
        let (a, b) = tokio::join!(tick(&s, &f), tick(&s, &f));
        a.unwrap();
        b.unwrap();
        assert_eq!(count(&pool).await, 1);
        let replica = Leases::new();
        replica.renew_once(&pool).await;
        assert!(replica.held(Lease::ReportScheduler).is_none());
        let stale = Fence {
            holder: replica.holder(),
            ..f
        };
        assert!(crate::leases::is_fenced(
            &tick(&s, &stale).await.unwrap_err()
        ));
        tick(&s, &f).await.unwrap();
        assert_eq!(count(&pool).await, 1);
        let (start,end,revision,skipped,epoch,expires):(DateTime<Utc>,DateTime<Utc>,i64,i64,i64,bool) = sqlx::query_as("SELECT period_start,period_end,configuration_revision,skipped_intervals,lease_epoch,expires_at=created_at+interval '90 days' FROM report_runs WHERE schedule_id=$1").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(end - start, chrono::TimeDelta::days(7));
        assert!(skipped >= 3);
        assert_eq!(revision, 1);
        assert_eq!(epoch, f.epoch);
        assert!(expires);
        let cursor: DateTime<Utc> =
            sqlx::query_scalar("SELECT last_period_end FROM report_schedules WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(cursor, end);
        assert!(
            crate::store::MIGRATOR
                .iter()
                .any(|m| m.version == 63 && m.description == "report generation")
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn scheduled_reports_generation_revision_token_expiry_and_lease_fences(pool: PgPool) {
        let (s, f, id, _) = setup(&pool).await;
        let c = claim(&s, &f, id).await.unwrap().unwrap();
        let body = snapshot(&s, &c).await.unwrap();
        sqlx::query("UPDATE report_schedules SET delivery_time='00:02' WHERE id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(!complete(&s, &f, &c, &body).await.unwrap());
        assert_eq!(count(&pool).await, 0);
        assert_eq!(cursor(&pool, id).await, (None, None, None));
        assert!(claim(&s, &f, id).await.unwrap().is_none());
        assert_eq!(count(&pool).await, 0);
        assert_eq!(
            cursor(&pool, id).await,
            (
                Some(at("2026-11-02T00:02:00Z")),
                Some(at("2026-11-02T00:00:00Z")),
                None,
            ),
            "revision reset initializes at the same scheduling instant without replay"
        );
        sqlx::query("UPDATE report_schedules SET next_period_end=date_trunc('week',$2::timestamptz,'UTC')-interval '7 days',next_run_at=$2::timestamptz-interval '1 day' WHERE id=$1").bind(id).bind(scheduling_now()).execute(&pool).await.unwrap();
        sqlx::query("UPDATE report_generation_claims SET expires_at=clock_timestamp()-interval '1 second' WHERE schedule_id=$1").bind(id).execute(&pool).await.unwrap();
        let c = claim(&s, &f, id).await.unwrap().unwrap();
        let body = snapshot(&s, &c).await.unwrap();
        sqlx::query(
            "UPDATE report_generation_claims SET token=gen_random_uuid() WHERE schedule_id=$1",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        assert!(!complete(&s, &f, &c, &body).await.unwrap());
        sqlx::query("UPDATE report_generation_claims SET expires_at=clock_timestamp()-interval '1 second' WHERE schedule_id=$1").bind(id).execute(&pool).await.unwrap();
        let c = claim(&s, &f, id).await.unwrap().unwrap();
        sqlx::query("SELECT omg_lease_release('report_scheduler',$1,$2)")
            .bind(f.holder)
            .bind(f.epoch)
            .execute(&pool)
            .await
            .unwrap();
        assert!(crate::leases::is_fenced(
            &complete(&s, &f, &c, &body).await.unwrap_err()
        ));
        assert_eq!(count(&pool).await, 0);
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn scheduled_reports_occurrence_failure_categories_are_immutable_and_bounded(
        pool: PgPool,
    ) {
        let (s, f, id, _) = setup(&pool).await;
        let c = claim(&s, &f, id).await.unwrap().unwrap();
        for (error, category) in [
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
                sqlx::Error::Protocol("DO NOT PERSIST RAW DETAILS".into()),
                "other",
            ),
        ] {
            crate::scheduled_reports::failed(&s, &f, id, Some(&c), &error)
                .await
                .unwrap();
            let recorded:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM report_generation_outcomes WHERE schedule_id=$1 AND category=$2 AND period_start IS NOT NULL AND period_end IS NOT NULL AND claim_token IS NOT NULL AND run_id IS NULL)").bind(id).bind(category).fetch_one(&pool).await.unwrap();
            assert!(recorded);
        }
        for statement in [
            "UPDATE report_generation_outcomes SET category='other'",
            "DELETE FROM report_generation_outcomes",
            "TRUNCATE report_generation_outcomes",
        ] {
            assert!(
                sqlx::query(statement).execute(&pool).await.is_err(),
                "{statement}"
            );
        }
        assert!(sqlx::query("INSERT INTO report_generation_outcomes(schedule_id,configuration_revision,category) VALUES($1,1,'unbounded attacker detail')").bind(id).execute(&pool).await.is_err());
        assert_eq!(
            count(&pool).await,
            0,
            "failure records are not successful snapshots"
        );
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT last_period_end IS NULL FROM report_schedules WHERE id=$1"
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn scheduled_reports_generation_snapshot_cap_rolls_back_cursor_and_occurrence(
        pool: PgPool,
    ) {
        let (s, f, id, _) = setup(&pool).await;
        let c = claim(&s, &f, id).await.unwrap().unwrap();
        let body = snapshot(&s, &c).await.unwrap();
        let mut oversized = body.clone();
        oversized["oversized"] = serde_json::json!("x".repeat(1_048_576));
        let error = complete(&s, &f, &c, &oversized).await.unwrap_err();
        assert_eq!(
            crate::scheduled_reports::Outcome::classify(&error),
            crate::scheduled_reports::Outcome::SnapshotTooLarge
        );
        assert_eq!(count(&pool).await, 0);
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT last_period_end IS NULL FROM report_schedules WHERE id=$1"
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        );
        assert!(complete(&s, &f, &c, &body).await.unwrap());
        assert_eq!(count(&pool).await, 1);
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn scheduled_reports_generation_authority_loss_pauses_audited(pool: PgPool) {
        let (s, f, id, user) = setup(&pool).await;
        let c = claim(&s, &f, id).await.unwrap().unwrap();
        let body = snapshot(&s, &c).await.unwrap();
        sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1")
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        assert!(!complete(&s, &f, &c, &body).await.unwrap());
        assert_eq!(count(&pool).await, 0);
        let state: String = sqlx::query_scalar("SELECT state FROM report_schedules WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state, "paused");
        let audit:Value=sqlx::query_scalar("SELECT metadata FROM audit_events WHERE resource_id=$1 AND action='report_schedule.authority_paused'").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(audit["category"], "creator_authority_lost");
        assert!(sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT FROM report_generation_outcomes WHERE schedule_id=$1 AND category='authority_lost' AND period_start IS NOT NULL)").bind(id).fetch_one(&pool).await.unwrap());
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn scheduled_reports_generation_workspace_and_personal_live_authority(pool: PgPool) {
        let (s, f, _, user) = setup(&pool).await;
        for kind in ["team", "personal"] {
            let ws = Uuid::new_v4();
            sqlx::query("INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES($1,'Report scope',$2,$3)")
                .bind(ws).bind(kind).bind((kind=="personal").then_some(user)).execute(&pool).await.unwrap();
            if kind == "team" {
                sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,'admin','manual')").bind(ws).bind(user).execute(&pool).await.unwrap();
            }
            let id:Uuid=sqlx::query_scalar("INSERT INTO report_schedules(scope_kind,workspace_id,created_by,cadence,timezone,delivery_time,state) VALUES($1,$2,$3,'weekly','UTC','00:01','active') RETURNING id")
                .bind(if kind=="personal" {"personal"} else {"workspace"}).bind(ws).bind(user).fetch_one(&pool).await.unwrap();
            sqlx::query("UPDATE report_schedules SET next_period_end=date_trunc('week',$2::timestamptz,'UTC')-interval '7 days',next_run_at=$2::timestamptz-interval '1 day' WHERE id=$1").bind(id).bind(scheduling_now()).execute(&pool).await.unwrap();
            let c = claim(&s, &f, id).await.unwrap().unwrap();
            let body = snapshot(&s, &c).await.unwrap();
            if kind == "team" {
                // Platform Admin does not substitute for live human membership.
                sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id=$2").bind(ws).bind(user).execute(&pool).await.unwrap();
            } else {
                sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1")
                    .bind(ws)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            assert!(!complete(&s, &f, &c, &body).await.unwrap());
            assert_eq!(
                sqlx::query_scalar::<_, String>("SELECT state FROM report_schedules WHERE id=$1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .unwrap(),
                "paused"
            );
        }
        assert_eq!(count(&pool).await, 0);
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn scheduled_reports_generation_immutable_snapshot_retention_and_dedup(pool: PgPool) {
        let (s, f, _, _) = setup(&pool).await;
        tick(&s, &f).await.unwrap();
        for sql in [
            "UPDATE report_snapshots SET body='{}'",
            "DELETE FROM report_snapshots",
            "TRUNCATE report_snapshots",
            "UPDATE report_runs SET configuration_revision=2",
            "DELETE FROM report_runs",
            "TRUNCATE report_runs",
        ] {
            assert!(sqlx::query(sql).execute(&pool).await.is_err(), "{sql}");
        }
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT omg_report_prune_snapshots(100)")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        let body: Value = sqlx::query_scalar("SELECT body FROM report_snapshots")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(body["totals"]["known_cost_microusd"], "0");
        assert!(body["totals"]["root_requests"].is_null());
        // Migrator-only fixture aging. Runtime never receives these privileges;
        // the immutable UPDATE refusal was proved above before changing age.
        sqlx::raw_sql("ALTER TABLE report_runs DISABLE TRIGGER report_runs_immutable; UPDATE report_runs SET created_at=clock_timestamp()-interval '91 days',expires_at=clock_timestamp()-interval '1 day'; ALTER TABLE report_runs ENABLE TRIGGER report_runs_immutable; ALTER TABLE report_snapshots DISABLE TRIGGER report_snapshots_immutable; UPDATE report_snapshots SET expires_at=clock_timestamp()-interval '1 day'; ALTER TABLE report_snapshots ENABLE TRIGGER report_snapshots_immutable;")
            .execute(&pool).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT omg_report_prune_snapshots(100)")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            count(&pool).await,
            1,
            "deduplication survives payload expiry"
        );
        assert!(
            sqlx::query("INSERT INTO report_snapshots(run_id,body) SELECT id,$1 FROM report_runs")
                .bind(&body)
                .execute(&pool)
                .await
                .is_err(),
            "expired payload cannot be resurrected"
        );
        tick(&s, &f).await.unwrap();
        assert_eq!(
            count(&pool).await,
            1,
            "expiry must not replay an occurrence"
        );
    }
}
