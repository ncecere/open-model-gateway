use super::*;
const PATH: &str = "/api/v1/platform/reservations/resolve-batch-envelopes";

pub(crate) async fn envelope(f: &Fixture, mode: &str, terminal: bool) -> Uuid {
    let id = execution(
        f,
        f.principal.key_id,
        f.principal.workspace_id,
        "company/smart",
        "unknown",
        None,
        Some(100),
    )
    .await;
    sqlx::query("UPDATE inference_executions SET workload_kind='batches' WHERE id=$1")
        .bind(id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let file = Uuid::new_v4();
    sqlx::query("INSERT INTO stored_files(id,object_key,purpose,workspace_id,created_by_api_key_id,backend,encryption_key_id) VALUES($1,'batch_input/'||$2::text||'/'||$1::text,'batch_input',$2,$3,'memory','memory')").bind(file).bind(f.principal.workspace_id).bind(f.principal.key_id).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,poll_deadline_at,batch_endpoint,batch_mode,input_file_id,work_file_id) VALUES($1,'batch',$2,$3,$4,$1,'company/smart','openai',now()+interval '1 hour','/v1/chat/completions',$5,$6,$6)").bind(id).bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).bind(mode).bind(file).execute(&f.store.pool).await.unwrap();
    if terminal {
        sqlx::query("UPDATE async_jobs SET state='failed',completed_at=now() WHERE id=$1")
            .bind(id)
            .execute(&f.store.pool)
            .await
            .unwrap();
    }
    id
}

#[dbtest::test]
async fn batch_envelope_no_lines_releases_only_envelope(pool: PgPool) {
    let f = fixture(pool).await;
    let admin = user(f.owner, true);
    assert_eq!(
        call(&f, &admin, "POST", PATH, json!({})).await.0,
        StatusCode::OK,
        "resolution preview route is required"
    );
    let id = envelope(&f, "gateway", true).await;
    let (status, preview) = call(&f, &admin, "POST", PATH, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["eligible_count"], "1");
    assert_eq!(preview["envelope_held_microusd"], "100");
    let (status, result) = call(
        &f,
        &admin,
        "POST",
        PATH,
        json!({"apply":true,"expected_count":"1","reason":"terminal batch evidence"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let row: (String, Option<i64>, Option<i64>, Option<i64>, bool) = sqlx::query_as("SELECT state,actual_microusd,input_tokens,output_tokens,free_price_resolved FROM governance_reservations WHERE execution_id=$1").bind(id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(row, ("settled".into(), Some(0), None, None, false));
    let ledger: (i64, Option<i64>, Option<i64>) = sqlx::query_as("SELECT amount_microusd,input_tokens,output_tokens FROM monetary_ledger WHERE execution_id=$1 AND kind='reconciliation'").bind(id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(ledger, (0, None, None));
}

#[dbtest::test]
async fn batch_envelope_conserves_line_costs_holds_observations_and_history(pool: PgPool) {
    let f = fixture(pool).await;
    let job = envelope(&f, "gateway", true).await;
    let mut lines = Vec::new();
    for (state, actual, held) in [
        ("settled", Some(17), Some(30)),
        ("unknown", None, Some(40)),
        ("pending", None, Some(50)),
        ("unknown", None, None),
    ] {
        let id = execution(
            &f,
            f.principal.key_id,
            f.principal.workspace_id,
            "company/smart",
            state,
            actual,
            held,
        )
        .await;
        sqlx::query("UPDATE inference_executions SET batch_job_id=$2 WHERE id=$1")
            .bind(id)
            .bind(job)
            .execute(&f.store.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO monetary_ledger(id,execution_id,admitted_at,kind,amount_microusd) SELECT gen_random_uuid(),execution_id,admitted_at,'hold',held_microusd FROM governance_reservations WHERE execution_id=$1").bind(id).execute(&f.store.pool).await.unwrap();
        if state != "pending" {
            sqlx::query("INSERT INTO monetary_ledger(id,execution_id,admitted_at,kind,amount_microusd,input_tokens,output_tokens) SELECT gen_random_uuid(),execution_id,admitted_at,CASE WHEN state='settled' THEN 'settlement' ELSE 'unknown' END,actual_microusd,input_tokens,output_tokens FROM governance_reservations WHERE execution_id=$1").bind(id).execute(&f.store.pool).await.unwrap();
        }
        lines.push(id);
    }
    let observed = serde_json::to_value(crate::billing::MeterUsage {
        requests: Some(2),
        ..Default::default()
    })
    .unwrap();
    sqlx::query(
        "UPDATE governance_reservations SET input_tokens=7,meter_usage=$2 WHERE execution_id=$1",
    )
    .bind(job)
    .bind(&observed)
    .execute(&f.store.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO monetary_ledger(id,execution_id,admitted_at,kind,amount_microusd) SELECT gen_random_uuid(),execution_id,admitted_at,'hold',220 FROM governance_reservations WHERE execution_id=$1 UNION ALL SELECT gen_random_uuid(),execution_id,admitted_at,'unknown',NULL FROM governance_reservations WHERE execution_id=$1").bind(job).execute(&f.store.pool).await.unwrap();
    let old: Value = sqlx::query_scalar(
        "SELECT jsonb_agg(to_jsonb(l) ORDER BY kind) FROM monetary_ledger l WHERE execution_id=$1",
    )
    .bind(job)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    let line_before: Value = sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(r) ORDER BY execution_id) FROM governance_reservations r WHERE execution_id=ANY($1)").bind(&lines).fetch_one(&f.store.pool).await.unwrap();
    let line_history:Value=sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(l) ORDER BY execution_id,kind) FROM monetary_ledger l WHERE execution_id=ANY($1)").bind(&lines).fetch_one(&f.store.pool).await.unwrap();
    let execution_before: Value =
        sqlx::query_scalar("SELECT to_jsonb(e) FROM inference_executions e WHERE id=$1")
            .bind(job)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    let before: (String,String) = sqlx::query_as("SELECT settled_microusd::text,held_microusd::text FROM budget_totals WHERE scope_kind='workspace' AND scope_id=$1 AND period='lifetime'").bind(f.principal.workspace_id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(before, ("17".into(), "190".into()));
    let admin = user(f.owner, true);
    let (status, result) = call(
        &f,
        &admin,
        "POST",
        PATH,
        json!({"apply":true,"expected_count":"1","reason":"conservation"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(
        result,
        json!({"eligible_count":"1","resolved_count":"1","next_offset":null,"envelope_held_microusd":"100","line_settled_microusd":"17","line_held_microusd":"90","line_pending_count":"1","line_unknown_count":"2","line_unbounded_count":"1"})
    );
    let after: Value = sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(r) ORDER BY execution_id) FROM governance_reservations r WHERE execution_id=ANY($1)").bind(&lines).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(line_before, after);
    let retained: Value = sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(l) ORDER BY kind) FROM monetary_ledger l WHERE execution_id=$1 AND kind<>'reconciliation'").bind(job).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(old, retained);
    let after: Value =
        sqlx::query_scalar("SELECT to_jsonb(e) FROM inference_executions e WHERE id=$1")
            .bind(job)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(execution_before, after);
    let row:(i64,Option<i64>,Option<i64>,Value,String)=sqlx::query_as("SELECT amount_microusd,input_tokens,output_tokens,meter_usage,evidence FROM monetary_ledger WHERE execution_id=$1 AND kind='reconciliation'").bind(job).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(
        row,
        (
            0,
            Some(7),
            None,
            observed,
            "batch-lines=17;conservation".into()
        )
    );
    assert_eq!(
        crate::governance::batch::batch_cost(&f.store, job)
            .await
            .unwrap(),
        (17, 90, true)
    );
    let totals:(String,String,i64)=sqlx::query_as("SELECT settled_microusd::text,held_microusd::text,unknown FROM budget_totals WHERE scope_kind='workspace' AND scope_id=$1 AND period='lifetime'").bind(f.principal.workspace_id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(totals, ("17".into(), "90".into(), 2));
    assert!(
        crate::governance::totals::verify(&f.store)
            .await
            .unwrap()
            .consistent()
    );
    let retained_lines:Value=sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(l) ORDER BY execution_id,kind) FROM monetary_ledger l WHERE execution_id=ANY($1)").bind(&lines).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(line_history, retained_lines);
    let known:i64=sqlx::query_scalar("SELECT sum(amount_microusd)::bigint FROM monetary_ledger WHERE (execution_id=ANY($1) OR execution_id=$2) AND kind IN ('settlement','reconciliation')").bind(&lines).bind(job).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(known, 17);
    let report_path = format!(
        "/api/v1/workspaces/{}/cost-report?{}",
        f.principal.workspace_id,
        range()
    );
    let (status, report) = call(&f, &admin, "GET", &report_path, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["totals"]["known_cost_microusd"], "17");
    assert_eq!(report["totals"]["held_microusd"], "90");
    let audit: Value = sqlx::query_scalar(
        "SELECT metadata FROM audit_events WHERE action='usage.batch_envelope_resolved'",
    )
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(audit["line_settled_microusd"], "17");
    for id in lines.into_iter().chain([job, f.principal.key_id]) {
        assert!(!audit.to_string().contains(&id.to_string()));
    }
}

#[dbtest::test]
async fn batch_envelope_refusals_concurrent_apply_and_lost_commit(pool: PgPool) {
    let f = fixture(pool).await;
    let native = envelope(&f, "native", true).await;
    let running = envelope(&f, "gateway", false).await;
    let priced = envelope(&f, "gateway", true).await;
    let price = f.price(0).await;
    sqlx::query("UPDATE governance_reservations SET price_id=$2 WHERE execution_id=$1")
        .bind(priced)
        .bind(price)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let ambiguous = envelope(&f, "gateway", true).await;
    sqlx::query("UPDATE async_jobs SET submit_started_at=now() WHERE id=$1")
        .bind(ambiguous)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let missing = envelope(&f, "gateway", true).await;
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,batch_job_id) VALUES(gen_random_uuid(),$1,$2,$3,'company/smart','openai',false,'failed',gen_random_uuid(),$4)").bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).bind(missing).execute(&f.store.pool).await.unwrap();
    let eligible = envelope(&f, "gateway", true).await;
    let admin = user(f.owner, true);
    assert_eq!(
        call(&f, &user(f.other, true), "POST", PATH, json!({}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    for body in [
        json!({"apply":true}),
        json!({"month":"2026-13"}),
        json!({"expected_count":"01"}),
        json!({"offset":"-1"}),
        json!({"reason":"\n"}),
    ] {
        assert_eq!(
            call(&f, &admin, "POST", PATH, body).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        call(&f, &admin, "POST", PATH, json!({"typo":true})).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let body = json!({"apply":true,"expected_count":"1","reason":"terminal evidence"});
    let (a, b) = tokio::join!(
        call(&f, &admin, "POST", PATH, body.clone()),
        call(&f, &admin, "POST", PATH, body.clone())
    );
    assert!(
        (a.0 == StatusCode::OK && b.0 == StatusCode::CONFLICT)
            || (b.0 == StatusCode::OK && a.0 == StatusCode::CONFLICT)
    );
    assert_eq!(
        call(&f, &admin, "POST", PATH, body.clone()).await.0,
        StatusCode::CONFLICT
    );
    for id in [native, running, priced, ambiguous, missing] {
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT state FROM governance_reservations WHERE execution_id=$1"
            )
            .bind(id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap(),
            "unknown"
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM monetary_ledger WHERE execution_id=$1 AND kind='reconciliation'"
        )
        .bind(eligible)
        .fetch_one(&f.store.pool)
        .await
        .unwrap(),
        1
    );
    let lost = envelope(&f, "gateway", true).await;
    let response = super::super::super::batch_envelopes::LOSE_COMMIT_ACK
        .scope(true, call(&f, &admin, "POST", PATH, body.clone()))
        .await;
    assert_eq!(response.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        call(&f, &admin, "POST", PATH, body).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM monetary_ledger WHERE execution_id=$1 AND kind='reconciliation'"
        )
        .bind(lost)
        .fetch_one(&f.store.pool)
        .await
        .unwrap(),
        1
    );
}

#[dbtest::test]
async fn batch_envelope_rechecks_admin_after_authority_wait(pool: PgPool) {
    use crate::governance::locks::{self, Scope};
    let f = fixture(pool).await;
    let id = envelope(&f, "gateway", true).await;
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
        json!({"apply":true,"expected_count":"1","reason":"authority wait"}),
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
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT state FROM governance_reservations WHERE execution_id=$1"
        )
        .bind(id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap(),
        "unknown"
    );
}

#[dbtest::test]
async fn batch_envelope_serializes_job_and_line_settlement(pool: PgPool) {
    let f = fixture(pool).await;
    let job = envelope(&f, "gateway", true).await;
    let line = execution(
        &f,
        f.principal.key_id,
        f.principal.workspace_id,
        "company/smart",
        "unknown",
        None,
        Some(40),
    )
    .await;
    sqlx::query("UPDATE inference_executions SET batch_job_id=$2 WHERE id=$1")
        .bind(line)
        .bind(job)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let mut tx = crate::db::begin(&f.store.pool).await.unwrap();
    sqlx::query("SELECT id FROM async_jobs WHERE id=$1 FOR UPDATE")
        .bind(job)
        .execute(&mut *tx)
        .await
        .unwrap();
    let admin = user(f.owner, true);
    let request = call(
        &f,
        &admin,
        "POST",
        PATH,
        json!({"apply":true,"expected_count":"1","reason":"settlement wait"}),
    );
    tokio::pin!(request);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut request)
            .await
            .is_err()
    );
    // The operator cannot read an envelope/line snapshot before the job lock.
    sqlx::query("UPDATE governance_reservations SET state='settled',actual_microusd=19,input_tokens=19,output_tokens=0 WHERE execution_id=$1").bind(line).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let (status, result) = request.await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["line_settled_microusd"], "19");
    assert_eq!(result["line_unknown_count"], "0");
    assert_eq!(
        crate::governance::batch::batch_cost(&f.store, job)
            .await
            .unwrap(),
        (19, 0, false)
    );
}

#[dbtest::test]
async fn batch_envelope_large_bucket_and_ineligible_prefix_make_progress(pool: PgPool) {
    let f = fixture(pool).await;
    let seed = envelope(&f, "gateway", true).await;
    // Missing line reservations remain ineligible even across a full page.
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,batch_job_id) VALUES(gen_random_uuid(),$1,$2,$3,'company/smart','openai',false,'failed',gen_random_uuid(),$4)").bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).bind(seed).execute(&f.store.pool).await.unwrap();
    for ambiguous in [true, false] {
        let ids:Vec<Uuid>=sqlx::query_scalar("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,workload_kind) SELECT gen_random_uuid(),$1,$2,$3,'company/smart','openai',false,'failed',gen_random_uuid(),'batches' FROM generate_series(1,$4) RETURNING id").bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).bind(if ambiguous {10000_i32} else {10001_i32}).fetch_all(&f.store.pool).await.unwrap();
        sqlx::query("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,poll_deadline_at,batch_endpoint,batch_mode,input_file_id,work_file_id) SELECT id,'batch',workspace_id,api_key_id,deployment_id,id,public_model,provider,now()+interval '1 hour','/v1/chat/completions',$2,(SELECT input_file_id FROM async_jobs WHERE id=$3),(SELECT work_file_id FROM async_jobs WHERE id=$3) FROM inference_executions WHERE id=ANY($1)").bind(&ids).bind("gateway").bind(seed).execute(&f.store.pool).await.unwrap();
        sqlx::query("UPDATE async_jobs SET state='failed',completed_at=now() WHERE id=ANY($1)")
            .bind(&ids)
            .execute(&f.store.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd) SELECT id,workspace_id,api_key_id,deployment_id,started_at,date_trunc('minute',started_at,'UTC'),date_trunc('month',started_at,'UTC'),started_at,'unknown',1 FROM inference_executions WHERE id=ANY($1)").bind(&ids).execute(&f.store.pool).await.unwrap();
        if ambiguous {
            sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,batch_job_id) SELECT gen_random_uuid(),workspace_id,api_key_id,deployment_id,'company/smart','openai',false,'failed',gen_random_uuid(),id FROM async_jobs WHERE id=ANY($1)").bind(&ids).execute(&f.store.pool).await.unwrap();
        }
    }
    let admin = user(f.owner, true);
    let preview = call(&f, &admin, "POST", PATH, json!({})).await;
    assert_eq!(preview.0, StatusCode::OK, "{}", preview.1);
    assert_eq!(preview.1["eligible_count"], "0");
    assert_eq!(preview.1["next_offset"], "10000");
    let empty = call(
        &f,
        &admin,
        "POST",
        PATH,
        json!({"apply":true,"expected_count":"0","reason":"ineligible prefix"}),
    )
    .await;
    assert_eq!(empty.0, StatusCode::OK, "{}", empty.1);
    assert_eq!(empty.1["next_offset"], "10000");
    let page = call(
        &f,
        &admin,
        "POST",
        PATH,
        json!({"apply":true,"offset":"10000","expected_count":"9999","reason":"bounded page"}),
    )
    .await;
    assert_eq!(page.0, StatusCode::OK, "{}", page.1);
    assert_eq!(page.1["next_offset"], "10001");
    let last = call(
        &f,
        &admin,
        "POST",
        PATH,
        json!({"apply":true,"offset":"10001","expected_count":"2","reason":"last page"}),
    )
    .await;
    assert_eq!(last.0, StatusCode::OK, "{}", last.1);
    assert_eq!(last.1["next_offset"], Value::Null);
    let remaining = call(&f, &admin, "POST", PATH, json!({"offset":"10000"})).await;
    assert_eq!(remaining.0, StatusCode::OK, "{}", remaining.1);
    assert_eq!(remaining.1["eligible_count"], "0");
    assert_eq!(remaining.1["next_offset"], Value::Null);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM governance_reservations WHERE state='unknown'"
        )
        .fetch_one(&f.store.pool)
        .await
        .unwrap(),
        10001
    );
}
