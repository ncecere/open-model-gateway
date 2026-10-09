//! Admin › route › Batch scheduling: defaults, validation, authorization,
//! audit, and the route's batch queue status.
use super::*;

async fn route(pool: &PgPool, provider: &str, endpoint: Option<&str>) -> Uuid {
    let (model, connection, deployment) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO models(id,public_name,supported_protocols) VALUES($1,$2,ARRAY['chat_completions'])").bind(model).bind(format!("m-{model}")).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint) VALUES($1,$2,$3,$4,$5)").bind(connection).bind(format!("{provider} {connection}")).bind(provider).bind(if endpoint.is_some() { "none" } else { "env:TEST" }).bind(endpoint).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,'gemma')").bind(deployment).bind(model).bind(connection).execute(pool).await.unwrap();
    deployment
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn route_batch_scheduling_settings(pool: PgPool) {
    let f = fixture(&pool).await;
    let vllm = route(&pool, "vllm", Some("http://10.0.0.5:8000/v1")).await;
    let cloud = route(&pool, "openai", None).await;
    let path = |d: Uuid| format!("/api/v1/platform/deployments/{d}/batch-scheduling");
    // Defaults: 2 lines at once, nothing else; an empty queue.
    let (status, body) = call(&f.s, &f.auditor, "GET", &path(vllm), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["settings"]["max_concurrency"], 2);
    assert_eq!(body["settings"]["window"], Value::Null);
    assert_eq!(body["priority_supported"], true);
    assert_eq!(body["status"]["queued_lines"], 0);
    assert_eq!(body["status"]["running_lines"], 0);
    assert_eq!(body["status"]["metrics"], Value::Null);
    let (_, body) = call(&f.s, &f.auditor, "GET", &path(cloud), Value::Null).await;
    assert_eq!(body["priority_supported"], false);
    let settings = json!({
        "max_concurrency": 3,
        "yield_live_threshold": 2,
        "priority": 10,
        "window": {"timezone": "America/New_York", "days": ["mon","tue","wed","thu","fri"], "start": "19:00", "end": "07:00"}
    });
    // Auditors read; only Admins write.
    let (status, _) = call(&f.s, &f.auditor, "PUT", &path(vllm), settings.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = call(&f.s, &f.admin, "PUT", &path(vllm), settings.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = call(&f.s, &f.admin, "GET", &path(vllm), Value::Null).await;
    assert_eq!(body["settings"]["max_concurrency"], 3);
    assert_eq!(body["settings"]["yield_live_threshold"], 2);
    assert_eq!(body["settings"]["priority"], 10);
    assert_eq!(body["settings"]["window"], settings["window"]);
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='batch_scheduling.updated' AND resource_id=$1",
    )
    .bind(vllm)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audits, 1);
    // Refused: priority hints off vLLM-compatible routes, a negative
    // priority, unknown zones, out-of-range numbers, unknown fields, and a
    // metrics URL outside the approved local endpoints (none are approved
    // here, so every metrics URL fails closed).
    for (d, bad) in [
        (cloud, json!({"max_concurrency": 2, "priority": 5})),
        (vllm, json!({"max_concurrency": 2, "priority": -5})),
        (vllm, json!({"max_concurrency": 0})),
        (
            vllm,
            json!({"max_concurrency": 2, "yield_live_threshold": 0}),
        ),
        (
            vllm,
            json!({"max_concurrency": 2, "window": {"timezone": "Mars/Base", "days": ["mon"], "start": "19:00", "end": "07:00"}}),
        ),
        (
            vllm,
            json!({"max_concurrency": 2, "window": {"timezone": "UTC", "days": [], "start": "19:00", "end": "07:00"}}),
        ),
        (
            vllm,
            json!({"max_concurrency": 2, "metrics": {"url": "http://10.0.0.5:8000/metrics", "max_waiting": 0}}),
        ),
        (
            vllm,
            json!({"max_concurrency": 2, "metrics": {"url": "http://169.254.169.254/metrics", "max_waiting": 0}}),
        ),
    ] {
        let (status, body) = call(&f.s, &f.admin, "PUT", &path(d), bad.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad} → {body}");
    }
    let (status, _) = call(
        &f.s,
        &f.admin,
        "PUT",
        &path(vllm),
        json!({"max_concurrency": 2, "surprise": true}),
    )
    .await;
    assert!(status.is_client_error());
    // Settings unchanged by refused writes.
    let (_, body) = call(&f.s, &f.admin, "GET", &path(vllm), Value::Null).await;
    assert_eq!(body["settings"]["max_concurrency"], 3);
    // Unknown routes are not found.
    let (status, _) = call(&f.s, &f.admin, "GET", &path(Uuid::new_v4()), Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A plain user cannot read platform routes.
    let (status, _) = call(&f.s, &f.member, "GET", &path(vllm), Value::Null).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Reset to defaults.
    let (status, _) = call(
        &f.s,
        &f.admin,
        "PUT",
        &path(vllm),
        json!({"max_concurrency": 2}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = call(&f.s, &f.admin, "GET", &path(vllm), Value::Null).await;
    assert_eq!(body["settings"], body["defaults"]);
}
