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
    let end = now.date_naive().with_day(1).unwrap();
    let start = end.checked_sub_months(chrono::Months::new(1)).unwrap();
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
        sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at,cost_center_id,cost_center_name) VALUES($1,$2,$3,$4,$5,'openai',false,'succeeded',$1,$6,$7,'Historical allocation')").bind(id).bind(p.workspace_id).bind(p.key_id).bind(f.deployment).bind(model).bind(lo+TimeDelta::hours(1)).bind(cc).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,actual_microusd,held_microusd,unbounded_cost,input_tokens,output_tokens) VALUES($1,$2,$3,$4,$5,$5,$6,$5,$7,$8,$9,false,0,0)").bind(id).bind(p.workspace_id).bind(p.key_id).bind(f.deployment).bind(lo+TimeDelta::hours(1)).bind(lo).bind(state).bind(actual).bind(held).execute(&pool).await.unwrap();
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
    assert!(!text.contains(&f.principal.key_id.to_string()));
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
    let personal = scheduled_snapshot(
        &mut tx,
        lo,
        hi,
        Some(f.principal.workspace_id),
        Some(cc),
        false,
    )
    .await
    .unwrap();
    assert!(
        personal.to_string().contains("PRIVATE_PERSONAL_MODEL"),
        "owner-only projection retains its own aggregates"
    );
    tx.commit().await.unwrap();
    // Dirty-hour fallback and dimension caps preserve the full totals. These
    // additional attempts deliberately have no reservation: unknown, not zero.
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at,cost_center_id,cost_center_name) SELECT gen_random_uuid(),$1,$2,$3,'model-'||g,'openai',false,'succeeded',gen_random_uuid(),$4,$5,'Historical allocation' FROM generate_series(1,101) g")
        .bind(f.team.workspace_id).bind(f.team.key_id).bind(f.deployment).bind(lo+TimeDelta::hours(1)).bind(cc).execute(&pool).await.unwrap();
    let mut tx = f.store.snapshot().await.unwrap();
    let capped = scheduled_snapshot(&mut tx, lo, hi, None, Some(cc), true)
        .await
        .unwrap();
    assert_eq!(capped["totals"]["attempts"], "104");
    assert_eq!(capped["totals"]["unresolved_attempts"], "103");
    assert_eq!(
        capped["breakdowns"]["models"].as_array().unwrap().len(),
        100
    );
    assert_eq!(capped["breakdowns_truncated"], true);
    assert!(
        capped["truncated_dimensions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "models")
    );
    tx.commit().await.unwrap();
    // Archived raw history with a missing/dirty rollup must fail visibly;
    // neither unknown totals nor unavailable archive coverage becomes zero.
    sqlx::query("INSERT INTO archived_partitions(id,archive_group,month,lower_bound,upper_bound,partitions,manifest_sha256,disposition) VALUES(gen_random_uuid(),'history',$1,$2,$3,'[]',repeat('0',64),'dropped')")
        .bind(start.format("%Y-%m").to_string()).bind(lo).bind(hi).execute(&pool).await.unwrap();
    let mut tx = f.store.snapshot().await.unwrap();
    assert!(
        scheduled_snapshot(&mut tx, lo, hi, None, Some(cc), true)
            .await
            .is_err()
    );
}
