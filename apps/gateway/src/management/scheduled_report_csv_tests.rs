//! #102D: real PostgreSQL, immutable snapshots and live authority (not mocks).
use super::scheduled_report_read_tests::occurrence_body;
use super::*;
async fn response(f: &Fixture, who: &BrowserPrincipal, path: &str) -> Response {
    routes()
        .layer(Extension(ManagementPrincipal::Browser(who.clone())))
        .layer(Extension(who.clone()))
        .with_state(f.s.clone())
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn report_csv_scope_bounds_escaping_expiry_and_no_store(pool: PgPool) {
    let f = fixture(&pool).await;
    let body = json!({"totals":{"known_cost_microusd":"9007199254740993","held_microusd":"1","root_requests":null},"breakdowns":{"models":[{"id":"m","name":" \t=HYPERLINK(\"bad\")\r\nnext","totals":{"known_cost_microusd":"1","root_requests":null}}]},"coverage":{"priced_attempts":null},"unavailable":{"pricing_coverage":"not_in_rollups"},"breakdowns_truncated":true,"truncated_dimensions":["models"],"held_is_floor":true});
    let (schedule, run, path) =
        occurrence_body(&f, &pool, &f.owner, Some(f.team), Some(body.clone())).await;
    let csv = format!("{path}/csv");
    let res = response(&f, &f.owner, &csv).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["cache-control"], "no-store");
    assert_eq!(res.headers()["x-content-type-options"], "nosniff");
    assert_eq!(
        res.headers()["content-disposition"],
        "attachment; filename=\"scheduled-report.csv\""
    );
    assert_eq!(res.headers()["x-export-rows"], "2");
    let text = String::from_utf8(
        axum::body::to_bytes(res.into_body(), 1_048_576)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(text.contains("9007199254740993"));
    assert!(text.contains("\"' \t=HYPERLINK(\"\"bad\"\")\r\nnext\""));
    assert!(text.contains("not_in_rollups"));
    assert!(
        text.contains("\"9007199254740993\",\"1\",\"\",\"\",\"\","),
        "unknown counts remain empty"
    );
    let runs = format!(
        "/api/v1/workspaces/{}/report-schedules/{schedule}/runs",
        f.team
    );
    let (status, list) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("{runs}?limit=20"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["data"][0]["id"], run.to_string());
    assert_eq!(list["outcomes"][0]["category"], "success");
    for limit in [0, 101] {
        assert_eq!(
            response(&f, &f.owner, &format!("{runs}?limit={limit}"))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    for who in [&f.member, &f.admin, &f.auditor, &f.outsider] {
        assert_eq!(
            response(&f, who, &csv).await.status(),
            StatusCode::FORBIDDEN
        );
    }
    let cross_scope = format!("/api/v1/platform/report-schedules/{schedule}/runs/{run}/csv");
    assert_eq!(
        response(&f, &f.admin, &cross_scope).await.status(),
        StatusCode::NOT_FOUND
    );
    let (_, _, platform) = occurrence_body(&f, &pool, &f.admin, None, Some(body.clone())).await;
    assert_eq!(
        response(&f, &f.auditor, &format!("{platform}/csv"))
            .await
            .status(),
        StatusCode::OK
    );
    let (_, _, personal) =
        occurrence_body(&f, &pool, &f.owner, Some(f.personal), Some(body.clone())).await;
    assert_eq!(
        response(&f, &f.admin, &format!("{personal}/csv"))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE platform_role_grants SET revoked_at=clock_timestamp() WHERE user_id=$1")
        .bind(f.auditor.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        response(&f, &f.auditor, &format!("{platform}/csv"))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    // Expiry is a safe, authorized metadata read, never a stale body/download.
    // Test-only owner time travel; runtime cannot mutate immutable expiry.
    let mut tx = crate::db::begin(&pool).await.unwrap();
    sqlx::query("ALTER TABLE report_snapshots DISABLE TRIGGER report_snapshots_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE report_snapshots SET expires_at=clock_timestamp()-interval '1 second' WHERE run_id=$1").bind(run).execute(&mut *tx).await.unwrap();
    sqlx::query("ALTER TABLE report_runs DISABLE TRIGGER report_runs_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE report_runs SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1",
    )
    .bind(run)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("ALTER TABLE report_runs ENABLE TRIGGER report_runs_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE report_snapshots ENABLE TRIGGER report_snapshots_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        response(&f, &f.owner, &csv).await.status(),
        StatusCode::GONE
    );
    let (_, detail) = call(&f.s, &f.owner, "GET", &path, Value::Null).await;
    assert_eq!(detail["expired"], true);
    assert!(detail["snapshot"].is_null());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT omg_report_prune_snapshots(100)")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    let (_, pruned) = call(&f.s, &f.owner, "GET", &path, Value::Null).await;
    assert_eq!(pruned["expired"], true);
    assert!(pruned["snapshot"].is_null());
    assert_eq!(
        response(&f, &f.owner, &csv).await.status(),
        StatusCode::GONE
    );
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=clock_timestamp() WHERE workspace_id=$1 AND user_id=$2").bind(f.team).bind(f.owner.user_id).execute(&pool).await.unwrap();
    assert_eq!(
        response(&f, &f.owner, &csv).await.status(),
        StatusCode::FORBIDDEN
    );
    let mut at_cap = body.clone();
    at_cap["breakdowns"]["models"] = json!(vec![json!({"id":"x","name":"x","totals":{}}); 999]);
    let (_, _, capped) = occurrence_body(&f, &pool, &f.admin, None, Some(at_cap)).await;
    let capped = response(&f, &f.admin, &format!("{capped}/csv")).await;
    assert_eq!(capped.status(), StatusCode::OK);
    assert_eq!(capped.headers()["x-export-rows"], "1000");
    let mut oversized = body;
    oversized["breakdowns"]["models"] = json!(vec![json!({"id":"x","name":"x","totals":{}}); 1000]);
    let (_, _, large) = occurrence_body(&f, &pool, &f.admin, None, Some(oversized)).await;
    assert_eq!(
        response(&f, &f.admin, &format!("{large}/csv"))
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    for route in [
        "/api/v1/platform/report-schedules/{id}/runs/{run}/csv",
        "/api/v1/workspaces/{ws}/report-schedules/{id}/runs/{run}/csv",
    ] {
        assert!(matches!(
            crate::route_policy::token_policy("GET", route).unwrap(),
            crate::route_policy::TokenPolicy::Denied
        ));
    }
}
