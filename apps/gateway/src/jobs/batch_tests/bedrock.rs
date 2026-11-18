use super::*;
use crate::inference::admission_validation_tests::{issue_key, serve};
use crate::providers::bedrock::BedrockAdapter;

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn failed_tool_result_sizes_and_claims_the_eligible_bedrock_deployment(pool: PgPool) {
    let mut w = world(pool.clone(), false, false, 1).await;
    let token = issue_key(&mut w.f).await;
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
    let (base, _server) = serve(app).await;
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
    w.jobs.engine = Some(engine.clone());
    let body = json!({"model":"company/claude","max_tokens":10,"messages":[
        {"role":"user","content":"ping"},
        {"role":"assistant","content":[{"type":"tool_use","id":"call","name":"ping","input":{}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":"failed","is_error":true}]}],
        "tools":[{"name":"ping","input_schema":{"type":"object"}}]});
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
