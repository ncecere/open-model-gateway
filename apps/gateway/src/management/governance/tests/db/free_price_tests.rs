use super::*;

const PATH: &str = "/api/v1/platform/reservations/resolve-free-price";

async fn unknown(f: &Fixture, price: Option<Uuid>, held: Option<i64>) -> Uuid {
    let id = execution(
        f,
        f.principal.key_id,
        f.principal.workspace_id,
        "company/smart",
        "unknown",
        None,
        held,
    )
    .await;
    sqlx::query("UPDATE governance_reservations SET price_id=$2 WHERE execution_id=$1")
        .bind(id)
        .bind(price)
        .execute(&f.store.pool)
        .await
        .unwrap();
    id
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn free_price_preview_apply_preserves_null_and_audits(pool: PgPool) {
    let f = fixture(pool).await;
    let free = f.price(0).await;
    let id = unknown(&f, Some(free), Some(0)).await;
    f.price(1_000_000).await; // Current rates do not change the pinned free version.
    sqlx::query("INSERT INTO monetary_ledger(id,execution_id,admitted_at,kind,amount_microusd) SELECT gen_random_uuid(),execution_id,admitted_at,'hold',0 FROM governance_reservations WHERE execution_id=$1 UNION ALL SELECT gen_random_uuid(),execution_id,admitted_at,'unknown',NULL FROM governance_reservations WHERE execution_id=$1").bind(id).execute(&f.store.pool).await.unwrap();
    let old: Value = sqlx::query_scalar(
        "SELECT jsonb_agg(to_jsonb(l) ORDER BY kind) FROM monetary_ledger l WHERE execution_id=$1",
    )
    .bind(id)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    let before: Value = sqlx::query_scalar("SELECT to_jsonb(r)-'state'-'actual_microusd'-'free_price_resolved' FROM governance_reservations r WHERE execution_id=$1").bind(id).fetch_one(&f.store.pool).await.unwrap();
    let admin = user(f.owner, true);
    let (status, preview) = call(&f, &admin, "POST", PATH, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(preview, json!({"eligible_count":"1","resolved_count":"0"}));
    let (status, result) = call(
        &f,
        &admin,
        "POST",
        PATH,
        json!({"apply":true,"expected_count":"1","reason":"free failed stream"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result, json!({"eligible_count":"1","resolved_count":"1"}));
    let row: (String, Option<i64>, Option<i64>, Option<i64>) = sqlx::query_as("SELECT state,actual_microusd,input_tokens,output_tokens FROM governance_reservations WHERE execution_id=$1").bind(id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(row, ("settled".into(), Some(0), None, None));
    let ledger: (i64, Option<i64>, Option<i64>, String) = sqlx::query_as("SELECT amount_microusd,input_tokens,output_tokens,evidence FROM monetary_ledger WHERE execution_id=$1 AND kind='reconciliation'").bind(id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!((ledger.0, ledger.1, ledger.2), (0, None, None));
    assert!(ledger.3.contains(&f.owner.to_string()));
    assert!(ledger.3.contains("free failed stream"));
    let after: Value = sqlx::query_scalar("SELECT to_jsonb(r)-'state'-'actual_microusd'-'free_price_resolved' FROM governance_reservations r WHERE execution_id=$1").bind(id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(before, after);
    let retained: Value = sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(l) ORDER BY kind) FROM monetary_ledger l WHERE execution_id=$1 AND kind<>'reconciliation'").bind(id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(retained, old);
    let actor: Uuid = sqlx::query_scalar("SELECT actor_user_id FROM audit_events WHERE action='usage.free_price_resolved' AND resource_type='workspace' AND resource_id=$1").bind(f.principal.workspace_id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(actor, f.owner);
    let audit: Value = sqlx::query_scalar(
        "SELECT metadata FROM audit_events WHERE action='usage.free_price_resolved'",
    )
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(
        audit,
        json!({"reason":"free failed stream","resolved_count":"1"})
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn free_price_selection_refusals_and_concurrency(pool: PgPool) {
    let f = fixture(pool).await;
    let priced = f.price(1_000_000).await;
    let inverse = unknown(&f, Some(priced), Some(0)).await;
    let free = f.price(0).await;
    let eligible = unknown(&f, Some(free), Some(0)).await;
    let held = unknown(&f, Some(free), Some(1)).await;
    let missing = unknown(&f, None, Some(0)).await;
    let unbounded = unknown(&f, None, None).await;
    // Supplied observations remain byte-for-byte on both history surfaces.
    let observation = serde_json::to_value(crate::billing::MeterUsage {
        requests: Some(2),
        ..Default::default()
    })
    .unwrap();
    sqlx::query(
        "UPDATE governance_reservations SET input_tokens=7,meter_usage=$2 WHERE execution_id=$1",
    )
    .bind(eligible)
    .bind(&observation)
    .execute(&f.store.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE inference_executions SET input_tokens=7,meter_usage=$2 WHERE id=$1")
        .bind(eligible)
        .bind(&observation)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let admin = user(f.owner, true);
    let member = user(f.other, true); // Cached/browser claims cannot elevate a live User.
    assert_eq!(
        call(&f, &member, "POST", PATH, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    for (body, status) in [
        (json!({"apply":true}), StatusCode::BAD_REQUEST),
        (
            json!({"apply":true,"expected_count":"1","reason":""}),
            StatusCode::BAD_REQUEST,
        ),
        (json!({"month":"2026-13"}), StatusCode::BAD_REQUEST),
        (json!({"typo":true}), StatusCode::UNPROCESSABLE_ENTITY),
    ] {
        assert_eq!(call(&f, &admin, "POST", PATH, body).await.0, status);
    }
    assert_eq!(
        call(
            &f,
            &admin,
            "POST",
            PATH,
            json!({"apply":true,"expected_count":"2","reason":"stale"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let request = json!({"apply":true,"expected_count":"1","reason":"confirmed free price"});
    let (a, b) = tokio::join!(
        call(&f, &admin, "POST", PATH, request.clone()),
        call(&f, &admin, "POST", PATH, request)
    );
    assert!(
        (a.0 == StatusCode::OK && b.0 == StatusCode::CONFLICT)
            || (b.0 == StatusCode::OK && a.0 == StatusCode::CONFLICT)
    );
    for id in [inverse, held, missing, unbounded] {
        let state: String =
            sqlx::query_scalar("SELECT state FROM governance_reservations WHERE execution_id=$1")
                .bind(id)
                .fetch_one(&f.store.pool)
                .await
                .unwrap();
        assert_eq!(state, "unknown");
    }
    let observed: (Option<i64>,Option<i64>,Option<Value>)=sqlx::query_as("SELECT input_tokens,output_tokens,meter_usage FROM monetary_ledger WHERE execution_id=$1 AND kind='reconciliation'").bind(eligible).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(observed, (Some(7), None, Some(observation)));
    assert_eq!(
        call(&f, &admin, "POST", PATH, json!({})).await.1,
        json!({"eligible_count":"0","resolved_count":"0"})
    );
    assert_eq!(
        call(
            &f,
            &admin,
            "POST",
            PATH,
            json!({"apply":true,"expected_count":"0","reason":"repeat"})
        )
        .await
        .0,
        StatusCode::OK
    );
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM monetary_ledger WHERE kind='reconciliation'")
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(rows, 1);
}

async fn v3(f: &Fixture, mutate: impl FnOnce(&mut Vec<Value>)) -> Uuid {
    use crate::billing::v3::Meter;
    let mut lines: Vec<Value> = Meter::ALL
        .into_iter()
        .map(|m| json!({"meter":m,"not_applicable":true}))
        .collect();
    // Explicit free request meter; all other meters explicitly do not apply.
    *lines.last_mut().unwrap() = json!({"meter":"requests","microusd_per_batch":"0","batch":1,"unit_label":"/request","sku_label":"Request"});
    mutate(&mut lines);
    // Publication stores explicit unknown rates as omitted canonical lines.
    let lines: crate::billing::v3::PriceLines = serde_json::from_value(json!(lines)).unwrap();
    let lines = serde_json::to_value(lines).unwrap();
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,pricing_version,input_token_limit,output_token_limit,price_lines,max_units) VALUES($1,$2,3,100,50,$3,'{}')").bind(id).bind(f.deployment).bind(lines).execute(&f.store.pool).await.unwrap();
    id
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn free_price_v3_missing_unknown_priced_and_selectors(pool: PgPool) {
    let f = fixture(pool).await;
    let free = v3(&f, |_| {}).await;
    let eligible = unknown(&f, Some(free), Some(0)).await;
    for price in [
        v3(&f,|v|{v.remove(0);}).await,
        v3(&f,|v|{v[0]=json!({"meter":"input_tokens","unknown":true});}).await,
        v3(&f,|v|{v[0]=json!({"meter":"input_tokens","microusd_per_batch":"1","batch":1000000,"unit_label":"/M tokens","sku_label":"Input"});}).await,
    ] { unknown(&f,Some(price),Some(0)).await; }
    let admin = user(f.owner, true);
    let month = Utc::now().format("%Y-%m").to_string();
    let selectors =
        json!({"month":month,"workspace_id":f.principal.workspace_id,"deployment_id":f.deployment});
    assert_eq!(
        call(&f, &admin, "POST", PATH, selectors.clone()).await.1["eligible_count"],
        "1"
    );
    for body in [
        json!({"workspace_id":f.team.workspace_id}),
        json!({"deployment_id":Uuid::new_v4()}),
        json!({"month":"2000-01"}),
    ] {
        assert_eq!(
            call(&f, &admin, "POST", PATH, body).await.1["eligible_count"],
            "0"
        );
    }
    let mut body = selectors;
    body["apply"] = json!(true);
    body["expected_count"] = json!("1");
    body["reason"] = json!("pinned free meters");
    assert_eq!(call(&f, &admin, "POST", PATH, body).await.0, StatusCode::OK);
    let settled: Vec<Uuid> = sqlx::query_scalar(
        "SELECT execution_id FROM governance_reservations WHERE state='settled'",
    )
    .fetch_all(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(settled, vec![eligible]);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn free_price_resolution_unblocks_history_archive(pool: PgPool) {
    use crate::archive::{Group, Month, Refusal, Request, archive_as_of};
    use chrono::{DateTime, Months};
    let f = fixture(pool).await;
    let upper: DateTime<Utc> = sqlx::query_scalar(
        "SELECT legacy_upper FROM history_partitions WHERE parent='inference_executions'",
    )
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    crate::partitions::ensure_at(&f.store.pool, 6, upper)
        .await
        .unwrap();
    let month = upper + Months::new(1);
    let free = f.price(0).await;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,attempt_number,started_at) VALUES($1,$2,$3,$4,'company/smart','openai',true,'failed',$1,1,$5)").bind(id).bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).bind(month).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd) VALUES($1,$2,$3,$4,$5,$6,$6,$6,$6,'unknown',0)").bind(id).bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).bind(free).bind(month).execute(&f.store.pool).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let label = month.format("%Y-%m").to_string();
    let req = Request {
        group: Group::History,
        month: Month::parse(&label).unwrap(),
        to: dir.path().into(),
        drop: false,
        retention_months: 1,
    };
    let later = month + Months::new(3);
    assert!(matches!(
        archive_as_of(&f.store.pool, &req, later)
            .await
            .unwrap_err()
            .downcast::<Refusal>()
            .unwrap(),
        Refusal::OpenReservations(1)
    ));
    assert_eq!(call(&f,&user(f.owner,true),"POST",PATH,json!({"month":label,"apply":true,"expected_count":"1","reason":"archive free-price unknown"})).await.0,StatusCode::OK);
    crate::rollups::roll_hour(&f.store, month, None)
        .await
        .unwrap();
    archive_as_of(&f.store.pool, &req, later).await.unwrap();
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM governance_reservations WHERE execution_id=$1")
            .bind(id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(
        remaining, 0,
        "month detached only by the explicit archive action"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn free_price_rechecks_role_after_authority_lock_wait(pool: PgPool) {
    use crate::governance::locks::{self, Scope};
    let f = fixture(pool).await;
    let free = f.price(0).await;
    let id = unknown(&f, Some(free), Some(0)).await;
    sqlx::query(
        "INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'admin','manual')",
    )
    .bind(f.other)
    .execute(&f.store.pool)
    .await
    .unwrap();
    let mut tx = crate::db::begin(&f.store.pool).await.unwrap();
    locks::exclusive(
        &mut tx,
        [
            Scope::Workspace(f.principal.workspace_id),
            Scope::User(f.owner),
        ],
    )
    .await
    .unwrap();
    let actor = user(f.owner, true);
    let request = call(
        &f,
        &actor,
        "POST",
        PATH,
        json!({"apply":true,"expected_count":"1","reason":"must refuse revoked authority"}),
    );
    tokio::pin!(request);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut request)
            .await
            .is_err()
    );
    sqlx::query("DELETE FROM platform_role_grants WHERE user_id=$1 AND role='admin'")
        .bind(f.owner)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'user','manual')")
        .bind(f.owner)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(request.await.0, StatusCode::FORBIDDEN);
    let state: String =
        sqlx::query_scalar("SELECT state FROM governance_reservations WHERE execution_id=$1")
            .bind(id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(state, "unknown");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn free_price_v2_cache_and_pinned_batch_tier(pool: PgPool) {
    let f = fixture(pool).await;
    let na = json!({"status":"not_applicable"});
    let cache = json!({"read":na,"write":na,"write_5m":na,"write_1h":na});
    for (read, expected) in [
        (json!({"status":"priced","microusd_per_million":"0"}), true),
        (json!({"status":"priced","microusd_per_million":"1"}), false),
        (json!({"status":"unknown"}), false),
    ] {
        let mut rates = cache.clone();
        rates["read"] = read;
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,pricing_version,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,cache_pricing) VALUES($1,$2,2,0,0,100,50,$3)").bind(id).bind(f.deployment).bind(rates).execute(&f.store.pool).await.unwrap();
        let execution = unknown(&f, Some(id), Some(0)).await;
        let count = call(&f, &user(f.owner, true), "POST", PATH, json!({}))
            .await
            .1["eligible_count"]
            .clone();
        assert_eq!(count, if expected { "1" } else { "0" });
        if expected {
            assert_eq!(
                call(
                    &f,
                    &user(f.owner, true),
                    "POST",
                    PATH,
                    json!({"apply":true,"expected_count":"1","reason":"free cache rates"})
                )
                .await
                .0,
                StatusCode::OK
            );
            let state: String = sqlx::query_scalar(
                "SELECT state FROM governance_reservations WHERE execution_id=$1",
            )
            .bind(execution)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
            assert_eq!(state, "settled");
        }
    }
    // A pinned native batch tier is independent of the standard tier.
    let free = v3(&f, |_| {}).await;
    let lines: Value = sqlx::query_scalar("SELECT price_lines FROM deployment_prices WHERE id=$1")
        .bind(free)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    let mut priced = lines.clone();
    priced[0] = json!({"meter":"input_tokens","microusd_per_batch":"1","batch":1000000,"unit_label":"/M tokens","sku_label":"Input"});
    for (standard, batch, expected) in [
        (priced.clone(), lines.clone(), true),
        (lines, priced, false),
    ] {
        let price = Uuid::new_v4();
        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,pricing_version,input_token_limit,output_token_limit,price_lines,max_units,batch_price_lines) VALUES($1,$2,3,100,50,$3,'{}',$4)").bind(price).bind(f.deployment).bind(standard).bind(batch).execute(&f.store.pool).await.unwrap();
        let execution = unknown(&f, Some(price), Some(0)).await;
        sqlx::query("UPDATE governance_reservations SET price_tier='batch' WHERE execution_id=$1")
            .bind(execution)
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(
            call(&f, &user(f.owner, true), "POST", PATH, json!({}))
                .await
                .1["eligible_count"],
            if expected { "1" } else { "0" }
        );
        if expected {
            assert_eq!(
                call(
                    &f,
                    &user(f.owner, true),
                    "POST",
                    PATH,
                    json!({"apply":true,"expected_count":"1","reason":"free pinned batch tier"})
                )
                .await
                .0,
                StatusCode::OK
            );
        }
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn free_price_native_job_requires_terminal_gateway_record(pool: PgPool) {
    let f = fixture(pool).await;
    let free = v3(&f, |_| {}).await;
    let id = unknown(&f, Some(free), Some(0)).await;
    sqlx::query("UPDATE inference_executions SET workload_kind='batches' WHERE id=$1")
        .bind(id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let admin = user(f.owner, true);
    assert_eq!(
        call(&f, &admin, "POST", PATH, json!({})).await.1["eligible_count"],
        "0",
        "missing gateway job is ambiguous"
    );
    sqlx::query("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,upstream_id,poll_deadline_at,batch_endpoint) VALUES($1,'batch',$2,$3,$4,$1,'company/smart','openai','batch_free',now()+interval '1 hour','/v1/chat/completions')").bind(id).bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).execute(&f.store.pool).await.unwrap();
    assert_eq!(
        call(&f, &admin, "POST", PATH, json!({})).await.1["eligible_count"],
        "0",
        "running job is ineligible"
    );
    sqlx::query("UPDATE async_jobs SET state='failed',completed_at=now() WHERE id=$1")
        .bind(id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &f,
            &admin,
            "POST",
            PATH,
            json!({"apply":true,"expected_count":"1","reason":"terminal accepted free job"})
        )
        .await
        .0,
        StatusCode::OK
    );
}
