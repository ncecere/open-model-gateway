//! Real loopback gateway + PostgreSQL, including a real adapter/mock upstream.
use super::super::http_support::{Resolver, Server, client};
use super::*;
use crate::providers::openrouter::{OpenRouterAdapter, OpenRouterConfig};

#[dbtest::test]
async fn running_gateway_openrouter_nonstream_200_rejection_keeps_free_zero(pool: PgPool) {
    let f = fixture(pool).await;
    price(&f, true).await;
    sqlx::query(
        "UPDATE provider_connections SET provider='openrouter',credential_ref='env:MOCK_ONLY'",
    )
    .execute(&f.store.pool)
    .await
    .unwrap();
    let upstream = Server::start(
        axum::Router::new()
            .fallback(|| async { axum::Json(json!({"error":{"code":400,"message":"rejected"}})) }),
    )
    .await;
    let adapter = OpenRouterAdapter::for_test(
        Arc::new(Resolver),
        OpenRouterConfig::default(),
        format!("{}/api/v1", upstream.base),
    );
    let mut registry = ProviderRegistry::default();
    registry.register(Arc::new(adapter)).unwrap();
    let engine = Engine::new(Arc::new(f.store.clone()), registry, EngineLimits::default()).unwrap();
    let gateway = Server::start(crate::http::router_with_engine(
        f.store.clone(),
        None,
        engine,
    ))
    .await;
    let bearer = crate::jobs::db_tests::bearer(&f, f.principal.workspace_id).await;
    let response = client().post(format!("{}/v1/chat/completions", gateway.base))
        .header("authorization", &bearer)
        .json(&json!({"model":"company/smart","messages":[{"role":"user","content":"test"}],"max_tokens":10,"stream":false}))
        .send().await.unwrap();
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "upstream_rejected");
    let id: Uuid = sqlx::query_scalar("SELECT id FROM inference_executions")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        amounts(&f, id).await,
        ("settled".into(), Some(0), Some(0), false)
    );
    let tokens: (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT input_tokens,output_tokens FROM inference_executions WHERE id=$1")
            .bind(id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(tokens, (Some(0), Some(0)));
}

#[dbtest::test]
async fn running_gateway_accepted_video_failure_and_refusals(pool: PgPool) {
    use crate::jobs::db_tests::{self, Fake, upstream_video, video_setup};
    let f = fixture(pool).await;
    video_setup(&f).await;
    price(&f, true).await;
    let fake = Fake::default();
    fake.push_video(upstream_video(JobState::Queued, Some(8)));
    fake.push_video(upstream_video(JobState::Failed, None));
    let mut registry = ProviderRegistry::default();
    registry.register(Arc::new(fake)).unwrap();
    let engine = Engine::new(Arc::new(f.store.clone()), registry, EngineLimits::default()).unwrap();
    let gateway = Server::start(crate::http::router_with_engine(
        f.store.clone(),
        None,
        engine,
    ))
    .await;
    let auth = db_tests::bearer(&f, f.principal.workspace_id).await;
    let theirs = db_tests::bearer(&f, f.team.workspace_id).await;
    let client = client();
    let response = client
        .post(format!("{}/v1/videos", gateway.base))
        .header("authorization", &auth)
        .json(&json!({"model":"company/smart","prompt":"a calm lake","seconds":"8"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    let id = body["id"].as_str().unwrap();
    let url = format!("{}/v1/videos/{id}", gateway.base);
    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        client
            .get(&url)
            .header("authorization", theirs)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    for _ in 0..2 {
        let response = client
            .get(&url)
            .header("authorization", &auth)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["status"], "failed");
    }
    let execution: Uuid = sqlx::query_scalar("SELECT execution_id FROM async_jobs")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    unknown(&f, execution, Usage::default(), Some(0)).await;
}

#[dbtest::test]
async fn running_gateway_native_batch_failure_and_refusals(pool: PgPool) {
    let mut w = world(pool, true, false, 1).await;
    price(&w.f, true).await;
    let runtime = FileStoreRuntime::memory();
    w.files = crate::filestore::FileStorage::new(w.f.store.clone(), runtime.clone());
    let mut registry = ProviderRegistry::default();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let capture = calls.clone();
    let upstream = Server::start(axum::Router::new().fallback(move |method: axum::http::Method, uri: axum::http::Uri, _: axum::body::Bytes| {
        capture.lock().unwrap().push((method.clone(), uri.path().to_owned()));
        async move {
            let body = match (method.as_str(), uri.path()) {
                ("POST", "/v1/files") => json!({"id":"file-in1","object":"file","bytes":1,"created_at":1,"filename":"batch.jsonl","purpose":"batch"}),
                ("POST", "/v1/batches") => json!({"id":"batch-up1","status":"validating","input_file_id":"file-in1"}),
                ("GET", "/v1/batches/batch-up1") => json!({"id":"batch-up1","status":"failed","input_file_id":"file-in1","request_counts":{"total":1,"completed":0,"failed":1}}),
                ("DELETE", "/v1/files/file-in1") => json!({"id":"file-in1","deleted":true}),
                _ => panic!("unexpected mock provider request"),
            };
            axum::Json(body)
        }
    })).await;
    let adapter = crate::providers::openai::OpenAiAdapter::for_test(
        Arc::new(Resolver),
        format!("{}/v1", upstream.base),
    );
    registry.register(Arc::new(adapter)).unwrap();
    w.jobs = Jobs::with(w.f.store.clone(), registry.clone(), JobLimits::default())
        .with_files(Some(runtime.clone()));
    let engine = Engine::new(
        Arc::new(w.f.store.clone()),
        registry,
        EngineLimits::default(),
    )
    .unwrap();
    let gateway = Server::start(
        crate::http::router_with_engine(w.f.store.clone(), None, engine)
            .layer(axum::Extension(runtime)),
    )
    .await;
    let auth = crate::jobs::db_tests::bearer(&w.f, w.f.principal.workspace_id).await;
    let theirs = crate::jobs::db_tests::bearer(&w.f, w.f.team.workspace_id).await;
    let file = w
        .upload(&w.f.principal, &[chat_line("a", "company/smart", "one")])
        .await;
    let client = client();
    let response = client.post(format!("{}/v1/batches", gateway.base)).header("authorization", &auth)
        .json(&json!({"input_file_id":file,"endpoint":"/v1/chat/completions","completion_window":"24h"})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    let id = body["id"].as_str().unwrap();
    let url = format!("{}/v1/batches/{id}", gateway.base);
    assert_eq!(
        client
            .get(&url)
            .header("authorization", &theirs)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    w.poll().await;
    w.poll().await;
    for _ in 0..2 {
        let response = client
            .get(&url)
            .header("authorization", &auth)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["status"], "failed");
    }
    let execution: Uuid = sqlx::query_scalar("SELECT execution_id FROM async_jobs")
        .fetch_one(&w.f.store.pool)
        .await
        .unwrap();
    unknown(&w.f, execution, Usage::default(), Some(0)).await;
    // The refused foreign read never submits; terminal reads never poll again.
    let paths: Vec<String> = calls
        .lock()
        .unwrap()
        .iter()
        .map(|(method, path)| format!("{method} {path}"))
        .collect();
    assert_eq!(
        paths,
        [
            "POST /v1/files",
            "POST /v1/batches",
            "GET /v1/batches/batch-up1",
            "DELETE /v1/files/file-in1"
        ]
    );
}
