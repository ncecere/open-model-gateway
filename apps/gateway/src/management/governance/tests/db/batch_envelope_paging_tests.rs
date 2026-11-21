use super::batch_envelope_tests::envelope;
use super::*;

const PATH: &str = "/api/v1/platform/reservations/resolve-batch-envelopes";

async fn envelopes(f: &Fixture, count: i32) -> Vec<Uuid> {
    let seed = envelope(f, "gateway", true).await;
    let mut ids: Vec<Uuid> = sqlx::query_scalar("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,workload_kind) SELECT gen_random_uuid(),$1,$2,$3,'company/smart','openai',false,'failed',gen_random_uuid(),'batches' FROM generate_series(1,$4) RETURNING id")
        .bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).bind(count - 1).fetch_all(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,poll_deadline_at,batch_endpoint,batch_mode,input_file_id,work_file_id) SELECT id,'batch',workspace_id,api_key_id,deployment_id,id,public_model,provider,now()+interval '1 hour','/v1/chat/completions','gateway',(SELECT input_file_id FROM async_jobs WHERE id=$2),(SELECT work_file_id FROM async_jobs WHERE id=$2) FROM inference_executions WHERE id=ANY($1)")
        .bind(&ids).bind(seed).execute(&f.store.pool).await.unwrap();
    sqlx::query("UPDATE async_jobs SET state='failed',completed_at=now() WHERE id=ANY($1)")
        .bind(&ids)
        .execute(&f.store.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd) SELECT id,workspace_id,api_key_id,deployment_id,started_at,date_trunc('minute',started_at,'UTC'),date_trunc('month',started_at,'UTC'),started_at,'unknown',1 FROM inference_executions WHERE id=ANY($1)")
        .bind(&ids).execute(&f.store.pool).await.unwrap();
    ids.push(seed);
    ids
}

async fn observe_writes(f: &Fixture) {
    sqlx::raw_sql("CREATE TABLE envelope_write_sizes(n bigint NOT NULL);
        CREATE SEQUENCE envelope_write_progress;
        CREATE FUNCTION observe_envelope_writes() RETURNS trigger LANGUAGE plpgsql AS $$
        DECLARE n bigint; BEGIN
          SELECT count(*) INTO n FROM envelope_write_rows WHERE batch_envelope_resolved;
          IF n>512 THEN RAISE EXCEPTION 'too many envelope rows in one statement'; END IF;
          IF n>0 THEN INSERT INTO envelope_write_sizes VALUES(n); PERFORM nextval('envelope_write_progress'); END IF;
          RETURN NULL;
        END $$;
        CREATE TRIGGER a_observe_envelope_writes AFTER UPDATE ON governance_reservations
        REFERENCING NEW TABLE AS envelope_write_rows FOR EACH STATEMENT EXECUTE FUNCTION observe_envelope_writes();")
        .execute(&f.store.pool).await.unwrap();
}

#[dbtest::test]
async fn batch_envelope_write_statements_are_bounded(pool: PgPool) {
    let f = fixture(pool).await;
    let ids = envelopes(&f, 1_025).await;
    observe_writes(&f).await;
    let response = call(
        &f,
        &user(f.owner, true),
        "POST",
        PATH,
        json!({"apply":true,"expected_count":"1025","reason":"bounded accounting statements"}),
    )
    .await;
    assert_eq!(response.0, StatusCode::OK, "{}", response.1);
    assert_eq!(response.1["resolved_count"], "1025");
    let sizes: Vec<i64> = sqlx::query_scalar("SELECT n FROM envelope_write_sizes")
        .fetch_all(&f.store.pool)
        .await
        .unwrap();
    assert!(sizes.iter().all(|n| (1..=512).contains(n)));
    assert_eq!(sizes.iter().sum::<i64>(), 1_025);
    let resolved: i64 = sqlx::query_scalar("SELECT count(*) FROM governance_reservations WHERE execution_id=ANY($1) AND state='settled' AND actual_microusd=0 AND batch_envelope_resolved AND NOT free_price_resolved AND input_tokens IS NULL AND output_tokens IS NULL")
        .bind(&ids).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(resolved, 1_025);
    let ledger: (i64, i64) = sqlx::query_as("SELECT count(*),sum(amount_microusd)::bigint FROM monetary_ledger WHERE execution_id=ANY($1) AND kind='reconciliation'")
        .bind(&ids).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(ledger, (1_025, 0));
    assert!(
        crate::governance::totals::verify(&f.store)
            .await
            .unwrap()
            .consistent()
    );
}

#[dbtest::test]
async fn batch_envelope_write_failure_rolls_back_prior_statements(pool: PgPool) {
    let f = fixture(pool).await;
    let ids = envelopes(&f, 1_025).await;
    let tail: Uuid = sqlx::query_scalar("SELECT execution_id FROM governance_reservations WHERE execution_id=ANY($1) ORDER BY admitted_at DESC,execution_id DESC LIMIT 1")
        .bind(&ids).fetch_one(&f.store.pool).await.unwrap();
    observe_writes(&f).await;
    sqlx::raw_sql("CREATE TABLE envelope_write_failure(execution_id uuid PRIMARY KEY);
        CREATE SEQUENCE envelope_write_failure_fired;
        CREATE FUNCTION reject_envelope_tail() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
          IF NEW.batch_envelope_resolved AND EXISTS(SELECT FROM envelope_write_failure WHERE execution_id=NEW.execution_id) THEN
            PERFORM nextval('envelope_write_failure_fired');
            RAISE EXCEPTION 'injected envelope write failure';
          END IF; RETURN NEW;
        END $$;
        CREATE TRIGGER reject_envelope_tail BEFORE UPDATE OF batch_envelope_resolved ON governance_reservations
        FOR EACH ROW EXECUTE FUNCTION reject_envelope_tail();")
        .execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO envelope_write_failure VALUES($1)")
        .bind(tail)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let before: Value = sqlx::query_scalar(
        "SELECT jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text) FROM budget_totals t",
    )
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    let reservations: Value = sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(r) ORDER BY r.admitted_at,r.execution_id) FROM governance_reservations r WHERE execution_id=ANY($1)")
        .bind(&ids).fetch_one(&f.store.pool).await.unwrap();
    let admin = user(f.owner, true);
    let body = json!({"apply":true,"expected_count":"1025","reason":"atomic page accounting"});
    let response = call(&f, &admin, "POST", PATH, body.clone()).await;
    assert_eq!(
        response.0,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        response.1
    );
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT is_called FROM envelope_write_failure_fired")
            .fetch_one(&f.store.pool)
            .await
            .unwrap()
    );
    // Sequence increments survive rollback and establish that an earlier
    // statement completed before the injected failure in the last group.
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT is_called FROM envelope_write_progress")
            .fetch_one(&f.store.pool)
            .await
            .unwrap()
    );
    let after: Value = sqlx::query_scalar(
        "SELECT jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text) FROM budget_totals t",
    )
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(before, after);
    let retained: Value = sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(r) ORDER BY r.admitted_at,r.execution_id) FROM governance_reservations r WHERE execution_id=ANY($1)")
        .bind(&ids).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(reservations, retained);
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM monetary_ledger WHERE execution_id=ANY($1) AND kind='reconciliation'")
        .bind(&ids).fetch_one(&f.store.pool).await.unwrap(), 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM audit_events WHERE action='usage.batch_envelope_resolved'"
        )
        .fetch_one(&f.store.pool)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM async_jobs WHERE id=ANY($1) AND settled_at IS NOT NULL"
        )
        .bind(&ids)
        .fetch_one(&f.store.pool)
        .await
        .unwrap(),
        0
    );
    sqlx::query("DROP TRIGGER reject_envelope_tail ON governance_reservations")
        .execute(&f.store.pool)
        .await
        .unwrap();
    let applied = call(&f, &admin, "POST", PATH, body).await;
    assert_eq!(applied.0, StatusCode::OK, "{}", applied.1);
    assert_eq!(applied.1["resolved_count"], "1025");
}

async fn lines(f: &Fixture, job: Uuid, count: i32) {
    sqlx::query("WITH inserted AS (INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,batch_job_id) SELECT gen_random_uuid(),$1,$2,$3,'company/smart','openai',false,'failed',gen_random_uuid(),$4 FROM generate_series(1,$5) RETURNING *) INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd) SELECT id,workspace_id,api_key_id,deployment_id,started_at,date_trunc('minute',started_at,'UTC'),date_trunc('month',started_at,'UTC'),started_at,'unknown',1 FROM inserted")
        .bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).bind(job).bind(count).execute(&f.store.pool).await.unwrap();
}

#[dbtest::test]
async fn batch_envelope_pages_bound_line_volume_without_splitting_jobs(pool: PgPool) {
    let f = fixture(pool).await;
    let first = envelope(&f, "gateway", true).await;
    let second = envelope(&f, "gateway", true).await;
    lines(&f, first, 75_001).await;
    lines(&f, second, 75_001).await;
    let admin = user(f.owner, true);
    let preview = call(&f, &admin, "POST", PATH, json!({})).await;
    assert_eq!(preview.0, StatusCode::OK, "{}", preview.1);
    assert_eq!(preview.1["eligible_count"], "1");
    assert_eq!(preview.1["line_unknown_count"], "75001");
    assert_eq!(preview.1["line_held_microusd"], "75001");
    assert_eq!(preview.1["next_offset"], "1");
    let apply = json!({"apply":true,"expected_count":"1","reason":"whole batch accounting"});
    let first_page = call(&f, &admin, "POST", PATH, apply.clone()).await;
    assert_eq!(first_page.0, StatusCode::OK, "{}", first_page.1);
    assert_eq!(first_page.1["next_offset"], "0");
    let second_page = call(&f, &admin, "POST", PATH, apply).await;
    assert_eq!(second_page.0, StatusCode::OK, "{}", second_page.1);
    assert_eq!(second_page.1["line_unknown_count"], "75001");
    assert_eq!(second_page.1["next_offset"], Value::Null);
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id AND e.started_at=r.admitted_at WHERE e.batch_job_id=ANY($1) AND r.state='unknown' AND r.held_microusd=1").bind([first,second].as_slice()).fetch_one(&f.store.pool).await.unwrap(),150_002);
}

async fn duplicate_envelope(f: &Fixture, job: Uuid) {
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,workload_kind,started_at) SELECT id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,workload_kind,started_at+interval '1 microsecond' FROM inference_executions WHERE id=$1").bind(job).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd) SELECT e.id,e.workspace_id,e.api_key_id,e.deployment_id,e.started_at,date_trunc('minute',e.started_at,'UTC'),date_trunc('month',e.started_at,'UTC'),e.started_at,'unknown',200 FROM inference_executions e WHERE e.id=$1 AND NOT EXISTS(SELECT FROM governance_reservations r WHERE r.execution_id=e.id AND r.admitted_at=e.started_at)").bind(job).execute(&f.store.pool).await.unwrap();
}

#[dbtest::test]
async fn batch_envelope_oversized_history_does_not_block_later_jobs(pool: PgPool) {
    let f = fixture(pool).await;
    let oversized = envelope(&f, "gateway", true).await;
    lines(&f, oversized, 150_001).await;
    let eligible = envelope(&f, "gateway", true).await;
    let admin = user(f.owner, true);
    let preview = call(&f, &admin, "POST", PATH, json!({})).await;
    assert_eq!(preview.0, StatusCode::OK, "{}", preview.1);
    assert_eq!(preview.1["eligible_count"], "0");
    assert_eq!(preview.1["next_offset"], "1");
    let page = call(
        &f,
        &admin,
        "POST",
        PATH,
        json!({"offset":"1","apply":true,"expected_count":"1","reason":"bounded retained history"}),
    )
    .await;
    assert_eq!(page.0, StatusCode::OK, "{}", page.1);
    assert_eq!(page.1["next_offset"], Value::Null);
    let states:Vec<(Uuid,String)>=sqlx::query_as("SELECT execution_id,state FROM governance_reservations WHERE execution_id=ANY($1) ORDER BY execution_id").bind([oversized,eligible].as_slice()).fetch_all(&f.store.pool).await.unwrap();
    assert!(states.contains(&(oversized, "unknown".into())));
    assert!(states.contains(&(eligible, "settled".into())));
}

#[dbtest::test]
async fn batch_envelope_lock_wait_is_bounded_and_rolls_back(pool: PgPool) {
    let f = fixture(pool).await;
    let job = envelope(&f, "gateway", true).await;
    let mut tx = crate::db::begin(&f.store.pool).await.unwrap();
    sqlx::query("SELECT id FROM async_jobs WHERE id=$1 FOR UPDATE")
        .bind(job)
        .execute(&mut *tx)
        .await
        .unwrap();
    let admin = user(f.owner, true);
    let body = json!({"apply":true,"expected_count":"1","reason":"lock wait bound"});
    let blocked = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        call(&f, &admin, "POST", PATH, body.clone()),
    )
    .await
    .expect("the job lock wait must be bounded");
    assert_eq!(blocked.0, StatusCode::SERVICE_UNAVAILABLE, "{}", blocked.1);
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT state FROM governance_reservations WHERE execution_id=$1"
        )
        .bind(job)
        .fetch_one(&f.store.pool)
        .await
        .unwrap(),
        "unknown"
    );
    tx.rollback().await.unwrap();
    let retry = call(&f, &admin, "POST", PATH, body).await;
    assert_eq!(retry.0, StatusCode::OK, "{}", retry.1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM monetary_ledger WHERE execution_id=$1 AND kind='reconciliation'"
        )
        .bind(job)
        .fetch_one(&f.store.pool)
        .await
        .unwrap(),
        1
    );
}

#[dbtest::test]
async fn batch_envelope_duplicate_uuid_history_is_refused(pool: PgPool) {
    let f = fixture(pool).await;
    let job = envelope(&f, "gateway", true).await;
    duplicate_envelope(&f, job).await;
    let admin = user(f.owner, true);
    let preview = call(&f, &admin, "POST", PATH, json!({})).await;
    assert_eq!(preview.0, StatusCode::OK, "{}", preview.1);
    assert_eq!(preview.1["eligible_count"], "0");
    assert_eq!(preview.1["envelope_held_microusd"], "0");
    assert_eq!(
        call(
            &f,
            &admin,
            "POST",
            PATH,
            json!({"apply":true,"expected_count":"2","reason":"ambiguous history"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM governance_reservations WHERE execution_id=$1 AND state='unknown'"
        )
        .bind(job)
        .fetch_one(&f.store.pool)
        .await
        .unwrap(),
        2
    );
}

#[dbtest::test]
async fn batch_envelope_marker_refuses_duplicate_uuid_history(pool: PgPool) {
    let f = fixture(pool).await;
    let job = envelope(&f, "gateway", true).await;
    duplicate_envelope(&f, job).await;
    let error=sqlx::query("UPDATE governance_reservations SET state='settled',actual_microusd=0,batch_envelope_resolved=true WHERE execution_id=$1").bind(job).execute(&f.store.pool).await.unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23514")
    );
}

#[dbtest::test]
async fn batch_envelope_recheck_does_not_expand_the_locked_full_keys(pool: PgPool) {
    let f = fixture(pool).await;
    let job = envelope(&f, "gateway", true).await;
    let mut tx = crate::db::begin(&f.store.pool).await.unwrap();
    sqlx::query(
        "SELECT execution_id FROM governance_reservations WHERE execution_id=$1 FOR UPDATE",
    )
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
        json!({"apply":true,"expected_count":"2","reason":"fixed full reservation keys"}),
    );
    tokio::pin!(request);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut request)
            .await
            .is_err()
    );
    // A second admission timestamp appears after the reservation-lock query
    // took its snapshot. It must not enter the selected set on the recheck.
    duplicate_envelope(&f, job).await;
    tx.commit().await.unwrap();
    let response = request.await;
    assert_eq!(response.0, StatusCode::CONFLICT, "{}", response.1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM governance_reservations WHERE execution_id=$1 AND state='unknown'"
        )
        .bind(job)
        .fetch_one(&f.store.pool)
        .await
        .unwrap(),
        2
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM monetary_ledger WHERE execution_id=$1 AND kind='reconciliation'"
        )
        .bind(job)
        .fetch_one(&f.store.pool)
        .await
        .unwrap(),
        0
    );
}

#[dbtest::test]
async fn batch_envelope_lost_commit_replay_can_apply_another_page(pool: PgPool) {
    let f = fixture(pool).await;
    let seed = envelope(&f, "gateway", true).await;
    let ids:Vec<Uuid>=sqlx::query_scalar("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,workload_kind) SELECT gen_random_uuid(),$1,$2,$3,'company/smart','openai',false,'failed',gen_random_uuid(),'batches' FROM generate_series(1,19999) RETURNING id").bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).fetch_all(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,poll_deadline_at,batch_endpoint,batch_mode,input_file_id,work_file_id) SELECT id,'batch',workspace_id,api_key_id,deployment_id,id,public_model,provider,now()+interval '1 hour','/v1/chat/completions','gateway',(SELECT input_file_id FROM async_jobs WHERE id=$2),(SELECT work_file_id FROM async_jobs WHERE id=$2) FROM inference_executions WHERE id=ANY($1)").bind(&ids).bind(seed).execute(&f.store.pool).await.unwrap();
    sqlx::query("UPDATE async_jobs SET state='failed',completed_at=now() WHERE id=ANY($1)")
        .bind(&ids)
        .execute(&f.store.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd) SELECT id,workspace_id,api_key_id,deployment_id,started_at,date_trunc('minute',started_at,'UTC'),date_trunc('month',started_at,'UTC'),started_at,'unknown',1 FROM inference_executions WHERE id=ANY($1)").bind(&ids).execute(&f.store.pool).await.unwrap();
    let admin = user(f.owner, true);
    let body = json!({"apply":true,"expected_count":"10000","reason":"lost acknowledgement"});
    let lost = super::super::super::batch_envelopes::LOSE_COMMIT_ACK
        .scope(true, call(&f, &admin, "POST", PATH, body.clone()))
        .await;
    assert_eq!(lost.0, StatusCode::SERVICE_UNAVAILABLE);
    let replay = call(&f, &admin, "POST", PATH, body.clone()).await;
    assert_eq!(replay.0, StatusCode::OK, "{}", replay.1);
    assert_eq!(replay.1["resolved_count"], "10000");
    assert_eq!(
        call(&f, &admin, "POST", PATH, body).await.0,
        StatusCode::CONFLICT
    );
    let rows:(i64,i64)=sqlx::query_as("SELECT count(*),count(DISTINCT execution_id) FROM monetary_ledger WHERE kind='reconciliation'").fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(rows, (20000, 20000));
}
