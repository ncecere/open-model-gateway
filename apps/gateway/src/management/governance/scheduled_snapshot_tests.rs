use super::*;
use crate::governance::tests::db::fixture;
use sqlx::PgPool;
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_snapshot_exact_live_parity_rollups_privacy_and_attribution(
    pool: PgPool,
) {
    let f = fixture(pool.clone()).await;
    let now: chrono::DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let start = now.date_naive().with_day(1).unwrap();
    let end = now.date_naive().succ_opt().unwrap();
    let lo = start.and_hms_opt(0, 0, 0).unwrap().and_utc();
    let hi = end.and_hms_opt(0, 0, 0).unwrap().and_utc();
    let cc = Uuid::new_v4();
    sqlx::query("INSERT INTO cost_centers(id,name,code) VALUES($1,'Historical allocation','HIST')")
        .bind(cc)
        .execute(&pool)
        .await
        .unwrap();
    for (p, model, state, actual, held) in [
        (
            &f.principal,
            "PRIVATE_PERSONAL_MODEL",
            "settled",
            Some(9_007_199_254_740_993i64),
            None,
        ),
        (&f.team, "Shared", "unknown", None, Some(17)),
        (&f.team, "Shared", "pending", None, Some(23)),
    ] {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at,cost_center_id,cost_center_name) VALUES($1,$2,$3,$4,$5,'openai',false,'succeeded',$1,$6,$7,'Historical allocation')").bind(id).bind(p.workspace_id).bind(p.api_key_id).bind(f.deployment).bind(model).bind(lo+TimeDelta::hours(1)).bind(cc).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,actual_microusd,held_microusd,unbounded_cost) VALUES($1,$2,$3,$4,$5,$5,$6,$5,$7,$8,$9,false)").bind(id).bind(p.workspace_id).bind(p.api_key_id).bind(f.deployment).bind(lo+TimeDelta::hours(1)).bind(lo).bind(state).bind(actual).bind(held).execute(&pool).await.unwrap();
    }
    sqlx::query("UPDATE cost_centers SET archived_at=now(),name='Renamed allocation' WHERE id=$1")
        .bind(cc)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = f.store.snapshot().await.unwrap();
    let filters = Filters {
        start,
        end,
        cost_center: Some(cc),
        ..Default::default()
    };
    let live = report(&mut tx, &filters, None, true).await.unwrap();
    let raw = scheduled_snapshot(&mut tx, lo, hi, None, Some(cc), true)
        .await
        .unwrap();
    for key in [
        "known_cost_microusd",
        "held_microusd",
        "attempts",
        "unresolved_attempts",
    ] {
        assert_eq!(raw["totals"][key], live["totals"][key], "{key}");
    }
    assert_eq!(raw["totals"]["known_cost_microusd"], "9007199254740993");
    assert_eq!(raw["totals"]["held_microusd"], "40");
    assert_eq!(raw["totals"]["unresolved_attempts"], "2");
    let text = raw.to_string();
    assert!(!text.contains("PRIVATE_PERSONAL_MODEL"));
    assert!(!text.contains(&f.principal.workspace_id.to_string()));
    assert!(!text.contains(&f.principal.api_key_id.to_string()));
    assert!(text.contains("Historical allocation"));
    assert!(!text.contains("Renamed allocation"));
    tx.commit().await.unwrap();
    // Build a clean rollup, then prove the scheduled additive projection is identical.
    sqlx::query("INSERT INTO usage_rollups_hourly SELECT * FROM omg_usage_aggregate($1,$2)")
        .bind(lo)
        .bind(hi)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO usage_rollup_hours(hour_start,rolled_at,groups) SELECT h,now(),0 FROM generate_series($1::timestamptz,$2::timestamptz-interval '1 hour',interval '1 hour') h").bind(lo).bind(hi).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM usage_rollup_dirty")
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = f.store.snapshot().await.unwrap();
    let rolled = scheduled_snapshot(&mut tx, lo, hi, None, Some(cc), true)
        .await
        .unwrap();
    assert_eq!(rolled["totals"], raw["totals"]);
    assert_eq!(rolled["breakdowns"], raw["breakdowns"]);
}
