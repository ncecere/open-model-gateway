//! #106: Admin › Settings › Audit export, on PostgreSQL.
use super::*;

const STATUS: &str = "/api/v1/platform/settings/audit-export";
const TEST: &str = "/api/v1/platform/settings/audit-export/test";

async fn receiver() -> (String, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    let app = axum::Router::new().route(
        "/ingest",
        axum::routing::post(move |body: axum::body::Bytes| {
            let log = log.clone();
            async move {
                log.lock()
                    .unwrap()
                    .push(serde_json::from_slice::<Value>(&body).unwrap());
                StatusCode::NO_CONTENT
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/ingest", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn status_is_platform_read_only_and_never_shows_the_url_or_secret(pool: PgPool) {
    let f = fixture(&pool).await;
    for actor in [&f.admin, &f.auditor] {
        let (status, v) = call(&f.s, actor, "GET", STATUS, json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["configured"], false);
        assert_eq!(v["enabled"], false);
        assert_eq!(v["schema"], "omg.audit.v1");
    }
    for actor in [&f.owner, &f.member, &f.outsider] {
        assert_eq!(
            call(&f.s, actor, "GET", STATUS, json!({})).await.0,
            StatusCode::FORBIDDEN
        );
    }
    // Nothing to change: no PUT/PATCH/DELETE.
    for method in ["PUT", "PATCH", "DELETE"] {
        assert_eq!(
            call(&f.s, &f.admin, method, STATUS, json!({"enabled":false}))
                .await
                .0,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method}"
        );
    }
    // Not configured: a test send is a conflict, not a silent success.
    assert_eq!(
        call(&f.s, &f.admin, "POST", TEST, json!({})).await.0,
        StatusCode::CONFLICT
    );
    // Only a Platform Admin learns that: others are refused first.
    for actor in [&f.auditor, &f.member] {
        assert_eq!(
            call(&f.s, actor, "POST", TEST, json!({})).await.0,
            StatusCode::FORBIDDEN
        );
    }
    let (url, _) = receiver().await;
    let exporter =
        crate::audit_export::Exporter::new(&format!("{url}?token=hidden"), vec![7; 32], true)
            .unwrap()
            .unwrap();
    let setting = crate::audit_export::Setting::On(
        crate::audit_export::Exporter::new(&format!("{url}?token=hidden"), vec![7; 32], true)
            .unwrap()
            .unwrap(),
    );
    crate::audit_export::sync_state(&f.s, &setting)
        .await
        .unwrap();
    let s = f.s.clone().with_audit_export(std::sync::Arc::new(exporter));
    let (_, v) = call(&s, &f.auditor, "GET", STATUS, json!({})).await;
    assert_eq!(
        (
            v["configured"].clone(),
            v["enabled"].clone(),
            v["destination_host"].clone()
        ),
        (json!(true), json!(true), json!("127.0.0.1"))
    );
    assert!(
        !v.to_string().contains("hidden") && !v.to_string().contains("ingest"),
        "no path, query or secret"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_test_send_is_signed_audited_admin_only_and_rate_limited(pool: PgPool) {
    let f = fixture(&pool).await;
    let (url, seen) = receiver().await;
    let exporter = crate::audit_export::Exporter::new(&url, vec![7; 32], true)
        .unwrap()
        .unwrap();
    let s = f.s.clone().with_audit_export(std::sync::Arc::new(exporter));
    assert_eq!(
        call(&s, &f.auditor, "POST", TEST, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    for _ in 0..2 {
        assert_eq!(
            call(&s, &f.admin, "POST", TEST, json!({})).await,
            (StatusCode::OK, json!({"ok":true,"error":null}))
        );
    }
    assert_eq!(
        call(&s, &f.admin, "POST", TEST, json!({})).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    let bodies = seen.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2);
    assert_eq!(
        (bodies[0]["test"].clone(), bodies[0]["events"].clone()),
        (json!(true), json!([]))
    );
    let audited: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='audit_export.tested'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(audited, 2);
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_export_queue")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        queued, 0,
        "export not enabled in this database: the test send queues nothing"
    );
}
