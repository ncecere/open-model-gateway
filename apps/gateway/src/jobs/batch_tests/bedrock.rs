use super::*;
use crate::inference::admission_validation_tests::{issue_key, serve};
use crate::providers::bedrock::BedrockAdapter;

type Captures = Arc<Mutex<Vec<(axum::http::Uri, Value)>>>;
async fn routes(
    pool: PgPool,
) -> (
    World,
    Uuid,
    Captures,
    crate::inference::admission_validation_tests::Server,
) {
    let mut w = world(pool.clone(), false, false, 1).await;
    let meta = w.claude_deployment;
    let nova = second_claude_route(&w).await;
    sqlx::query("UPDATE provider_connections SET provider='bedrock',credential_ref='aws:default',region='us-east-1'").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO routing_policies(model_id,max_attempts,allow_ambiguous_failover) VALUES($1,1,false)").bind(w.claude).execute(&pool).await.unwrap();
    for (id, model, priority) in [
        (meta, "meta.llama3-1-70b-instruct-v1:0", 0),
        (nova, "amazon.nova-pro-v1:0", 10),
    ] {
        sqlx::query("UPDATE deployments SET upstream_model=$2 WHERE id=$1")
            .bind(id)
            .bind(model)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO deployment_routing(deployment_id,priority) VALUES($1,$2)")
            .bind(id)
            .bind(priority)
            .execute(&pool)
            .await
            .unwrap();
    }
    // Meta costs 9 micro-USD per token and permits only 9 output tokens.
    // Nova costs 2 per token with a 100-input/50-output ceiling.
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES(gen_random_uuid(),$1,9000000,9000000,200,9,1)").bind(meta).execute(&pool).await.unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let capture = calls.clone();
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri, axum::Json(body): axum::Json<Value>| async move {
        capture.lock().unwrap().push((uri, body));
        axum::Json(json!({"output":{"message":{"role":"assistant","content":[{"text":"ok"}]}},"stopReason":"end_turn","usage":{"inputTokens":4,"outputTokens":1,"totalTokens":5}}))
    });
    let (base, server) = serve(app).await;
    w.jobs
        .registry
        .register(Arc::new(BedrockAdapter::for_test(&base)))
        .unwrap();
    let engine = Engine::new(
        Arc::new(w.f.store.clone()),
        w.jobs.registry.clone(),
        EngineLimits::default(),
    )
    .unwrap();
    w.jobs.engine = Some(engine);
    (w, nova, calls, server)
}

fn failed_body() -> Value {
    json!({"model":"company/claude","max_tokens":10,"messages":[
        {"role":"user","content":"ping"},
        {"role":"assistant","content":[{"type":"tool_use","id":"call","name":"ping","input":{}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":"failed","is_error":true}]}],
        "tools":[{"name":"ping","input_schema":{"type":"object"}}]})
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn failed_tool_result_sizes_and_claims_the_eligible_bedrock_deployment(pool: PgPool) {
    let (mut w, nova, calls, _server) = routes(pool.clone()).await;
    let token = issue_key(&mut w.f).await;
    let engine = w.jobs.engine.clone().unwrap();
    let body = failed_body();
    let line = json!({"custom_id":"failed-tool","method":"POST","url":"/v1/messages","body":body});
    let file = w.upload(&w.f.principal, &[line]).await;
    let job = w
        .create(w.f.principal, &file, BatchEndpoint::Messages, None)
        .await
        .unwrap();
    assert_eq!(job.deployment_id, nova);
    assert_eq!(reservation(&w.f, job.execution_id).await.1, Some(220));
    let (gateway, _gateway_server) = serve(crate::http::router_with_engine(
        w.f.store.clone(),
        None,
        engine,
    ))
    .await;
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{gateway}/v1/messages"))
        .bearer_auth(token)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap()["stop_reason"],
        "end_turn"
    );
    assert!(Runner::new(w.jobs.clone()).run_next().await.unwrap());
    let job = w.reload(&job).await;
    assert_eq!(
        (job.request_completed, job.request_failed),
        (Some(1), Some(0))
    );
    let claim: Uuid =
        sqlx::query_scalar("SELECT deployment_id FROM batch_lines WHERE job_id=$1 AND line_no=0")
            .bind(job.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(claim, nova);
    let deployments: Vec<Uuid> =
        sqlx::query_scalar("SELECT deployment_id FROM inference_executions")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(deployments, [nova; 3]);
    let child_hold: (i64, i64) = sqlx::query_as(
        "SELECT r.reserved_tokens,l.amount_microusd
        FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id
        JOIN deployment_prices p ON p.id=r.price_id AND p.deployment_id=$2
        JOIN monetary_ledger l ON l.execution_id=e.id AND l.kind='hold'
        WHERE e.batch_job_id=$1",
    )
    .bind(job.id)
    .bind(nova)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(child_hold, (110, 220));
    let captured = calls.lock().unwrap();
    assert_eq!(captured.len(), 2);
    for (uri, body) in captured.iter() {
        assert!(uri.path().contains("amazon.nova-pro-v1"));
        assert_eq!(
            body["messages"][2]["content"][0]["toolResult"],
            json!({"toolUseId":"call","status":"error","content":[{"text":"failed"}]})
        );
    }
}

fn mixed_lines() -> [Value; 2] {
    [
        json!({"custom_id":"ordinary","method":"POST","url":"/v1/messages","body":{
        "model":"company/claude","max_tokens":5,"messages":[{"role":"user","content":"ping"}]}}),
        json!({"custom_id":"failed","method":"POST","url":"/v1/messages","body":failed_body()}),
    ]
}

async fn cool(pool: &PgPool, deployment: Uuid, seconds: i32) {
    sqlx::query("INSERT INTO deployment_health(deployment_id,open_until) VALUES($1,clock_timestamp()+$2::int*interval '1 second') ON CONFLICT(deployment_id) DO UPDATE SET open_until=excluded.open_until")
        .bind(deployment).bind(seconds).execute(pool).await.unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn bedrock_mixed_cooldown_waits_for_eligible_route(pool: PgPool) {
    let (w, nova, calls, _server) = routes(pool.clone()).await;
    cool(&pool, nova, 3600).await;
    let file = w.upload(&w.f.principal, &mixed_lines()).await;
    let job = w
        .create(w.f.principal, &file, BatchEndpoint::Messages, None)
        .await
        .unwrap();
    assert_eq!(job.mode(), Some(BatchMode::Gateway));
    // Ordinary Meta: (200 + 5) * 9. Failed tool result on Nova: (100 + 10) * 2.
    assert_eq!(reservation(&w.f, job.execution_id).await.1, Some(2065));
    let runner = Runner::new(w.jobs.clone());
    let mut run = tokio::spawn(async move { runner.run_next().await.unwrap() });
    tokio::time::timeout(std::time::Duration::from_secs(30),async {
        loop {
            let done: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM batch_lines WHERE job_id=$1 AND line_no=0 AND state='succeeded')").bind(job.id).fetch_one(&pool).await.unwrap();
            if done { break; }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }).await.expect("healthy line completes while Nova is cooling");
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), &mut run)
            .await
            .is_err()
    );
    let claimed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM batch_lines WHERE job_id=$1 AND line_no=1)",
    )
    .bind(job.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!claimed);
    assert_eq!(calls.lock().unwrap().len(), 1);
    // Expire the circuit in PostgreSQL. Check the claim against this database
    // timestamp after completion so an early claim cannot race the assertion.
    let reopened: gateway_datetime_compat::UtcInstant = sqlx::query_scalar("UPDATE deployment_health SET open_until=clock_timestamp() WHERE deployment_id=$1 RETURNING open_until").bind(nova).fetch_one(&pool).await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(45), run)
            .await
            .unwrap()
            .unwrap()
    );
    let claims: Vec<(i32,Uuid)> = sqlx::query_as("SELECT line_no,deployment_id FROM batch_lines WHERE job_id=$1 AND state='succeeded' ORDER BY line_no").bind(job.id).fetch_all(&pool).await.unwrap();
    assert_eq!(claims, [(0, w.claude_deployment), (1, nova)]);
    let timely: bool = sqlx::query_scalar(
        "SELECT started_at >= $2 FROM batch_lines WHERE job_id=$1 AND line_no=1",
    )
    .bind(job.id)
    .bind(reopened)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(timely);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].0.path().contains("meta.llama"));
    assert!(calls[1].0.path().contains("amazon.nova"));
    assert_eq!(
        calls[1].1["messages"][2]["content"][0]["toolResult"]["status"],
        "error"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn bedrock_mixed_residency_reports_numbered_errors(pool: PgPool) {
    let (w, nova, calls, _server) = routes(pool.clone()).await;
    sqlx::query("UPDATE routing_policies SET required_residency='us' WHERE model_id=$1")
        .bind(w.claude)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE deployment_routing SET residency=CASE WHEN deployment_id=$1 THEN 'eu' ELSE 'us' END").bind(nova).execute(&pool).await.unwrap();
    let mut lines = mixed_lines().to_vec();
    lines.push(json!({"custom_id":"invalid"}));
    let before = admission_rowcounts(&w.f).await;
    let file = w.upload(&w.f.principal, &lines).await;
    let result = w
        .create(w.f.principal, &file, BatchEndpoint::Messages, None)
        .await;
    let Err(CreateError::Invalid(issues)) = result else {
        panic!("expected per-line validation")
    };
    assert_eq!(
        issues
            .iter()
            .map(|i| (i.line, i.error.code()))
            .collect::<Vec<_>>(),
        [(2, "model_not_found"), (3, "invalid_line")]
    );
    assert_eq!(admission_rowcounts(&w.f).await, before);
    assert!(calls.lock().unwrap().is_empty());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn bedrock_whole_model_cooldown_uses_body_eligible_retry(pool: PgPool) {
    let (w, nova, calls, _server) = routes(pool.clone()).await;
    cool(&pool, w.claude_deployment, 300).await;
    cool(&pool, nova, 3600).await;
    let file = w.upload(&w.f.principal, &mixed_lines()[1..]).await;
    let remaining = || {
        sqlx::query_scalar::<_, i32>(
        "SELECT ceil(extract(epoch FROM open_until - clock_timestamp()))::int FROM deployment_health WHERE deployment_id=$1"
    ).bind(nova).fetch_one(&pool)
    };
    let before = remaining().await.unwrap();
    let error = w
        .create(w.f.principal, &file, BatchEndpoint::Messages, None)
        .await
        .unwrap_err();
    let after = remaining().await.unwrap();
    let CreateError::Job(JobError::Inference(InferenceError::RouteCoolingDown(seconds))) = error
    else {
        panic!("expected a retryable model cooldown, got {error:?}");
    };
    assert!(after > 300);
    assert!(
        (after..=before).contains(&(seconds as i32)),
        "expected Nova's retry hint, got {seconds}"
    );
    assert!(calls.lock().unwrap().is_empty());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn cooldown_price_preview_cannot_submit_a_native_batch(pool: PgPool) {
    let w = world(pool.clone(), true, true, 1).await;
    let accepting = second_claude_route(&w).await;
    w.openai.script.lock().unwrap().permissive = true;
    cool(&pool, accepting, 3600).await;
    for (text, expected) in [
        ("strict-tool", BatchMode::Gateway),
        ("ordinary", BatchMode::Native),
    ] {
        let file = w
            .upload(&w.f.principal, &[chat_line("one", "company/claude", text)])
            .await;
        let job = w
            .create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None)
            .await
            .unwrap();
        assert_eq!(job.mode(), Some(expected));
    }
    assert!(w.openai.calls().is_empty());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn cooldown_price_preview_fences_stale_candidates(pool: PgPool) {
    let (w, nova, _, _server) = routes(pool.clone()).await;
    let alias = "company/claude";
    let principal = &w.f.principal;
    let candidates = w.f.store.deployments(principal, alias).await.unwrap();
    let preview = || {
        crate::routing::batch_price_routes(
            &w.f.store,
            principal,
            alias,
            &candidates,
            Uuid::new_v4(),
        )
    };
    cool(&pool, nova, 3600).await;
    sqlx::query("UPDATE routing_policies SET required_residency='us' WHERE model_id=$1")
        .bind(w.claude)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE deployment_routing SET residency=CASE WHEN deployment_id=$1 THEN 'us' ELSE 'eu' END").bind(nova).execute(&pool).await.unwrap();
    let order = preview().await.unwrap();
    assert_eq!(order.deployment_ids, [nova]);
    assert!(order.retry_after.is_some());
    sqlx::query("UPDATE deployments SET enabled=false WHERE id=$1")
        .bind(nova)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        preview().await,
        Err(InferenceError::ModelUnavailable)
    ));
    sqlx::query("UPDATE deployments SET enabled=true WHERE id=$1")
        .bind(nova)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM workspace_model_grants WHERE workspace_id=$1 AND model_id=$2")
        .bind(principal.workspace_id)
        .bind(w.claude)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        preview().await,
        Err(InferenceError::ModelUnavailable)
    ));
}
