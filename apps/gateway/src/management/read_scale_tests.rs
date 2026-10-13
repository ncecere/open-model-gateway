//! R06 (#65): large management lists stay fast and can't freeze other
//! requests. Lists select a page before building its rows, per-user lookups
//! are indexed, read-only lists use lock-free snapshots, and a slow read is
//! cancelled by PostgreSQL at the management read deadline.
use super::*;
use std::time::Duration;

async fn get_status(s: &Store, u: &BrowserPrincipal, path: &str) -> StatusCode {
    let response = routes()
        .layer(Extension(u.clone()))
        .with_state(s.clone())
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap());
    tokio::time::timeout(Duration::from_secs(5), response)
        .await
        .unwrap_or_else(|_| panic!("{path} waited on a lock"))
        .unwrap()
        .status()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn list_reads_do_not_wait_for_a_management_write(pool: PgPool) {
    let f = fixture(&pool).await;
    // What every management write holds while it runs.
    let mut writer = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM installation WHERE singleton FOR NO KEY UPDATE")
        .execute(&mut *writer)
        .await
        .unwrap();
    let (team, owner) = (f.team, f.owner.user_id);
    for (u, path) in [
        (&f.auditor, "/api/v1/platform/users".to_owned()),
        (&f.auditor, format!("/api/v1/platform/users/{owner}")),
        (&f.auditor, "/api/v1/platform/workspaces".to_owned()),
        (&f.owner, format!("/api/v1/workspaces/{team}/members")),
        (&f.owner, format!("/api/v1/workspaces/{team}/keys")),
        (
            &f.owner,
            format!("/api/v1/workspaces/{team}/service-accounts"),
        ),
    ] {
        assert_eq!(get_status(&f.s, u, &path).await, StatusCode::OK, "{path}");
    }
    // Authorization is still live: an outsider is refused without waiting either.
    assert_eq!(
        get_status(&f.s, &f.outsider, "/api/v1/platform/users").await,
        StatusCode::FORBIDDEN
    );
    writer.rollback().await.unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn users_list_builds_rows_only_for_the_requested_page(pool: PgPool) {
    let _f = fixture(&pool).await;
    sqlx::query("INSERT INTO users(id,email) SELECT gen_random_uuid(),'bulk-'||i||'@example.invalid' FROM generate_series(1,300) i")
        .execute(&pool)
        .await
        .unwrap();
    let plan: Value = sqlx::query_scalar(&format!(
        "EXPLAIN (ANALYZE, FORMAT JSON) {}",
        super::directory::users_list_sql()
    ))
    .bind(None::<String>)
    .bind(None::<String>)
    .bind(None::<String>)
    .bind(10_i64)
    .bind(250_i64)
    .fetch_one(&pool)
    .await
    .unwrap();
    // Every correlated subquery (sign-in, workspace count, role grants) runs once per returned row.
    fn worst(node: &Value) -> i64 {
        let own = if node["Subplan Name"].is_string() {
            node["Actual Loops"].as_i64().unwrap_or(0)
        } else {
            0
        };
        node["Plans"]
            .as_array()
            .into_iter()
            .flatten()
            .map(worst)
            .fold(own, i64::max)
    }
    let loops = worst(&plan[0]["Plan"]);
    assert!(loops > 0, "expected per-row subqueries in {plan}");
    assert!(
        loops <= 10,
        "subqueries ran {loops} times for a 10-row page"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn per_user_lookups_are_indexed(pool: PgPool) {
    for table in ["workspace_membership_grants", "platform_role_grants"] {
        let indexed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT FROM pg_index i JOIN pg_attribute a ON a.attrelid=i.indrelid AND a.attnum=i.indkey[0] WHERE i.indrelid=$1::regclass AND a.attname='user_id' AND i.indpred IS NULL)",
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(indexed, "{table} has no full index leading with user_id");
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn slow_management_reads_are_cancelled_at_the_deadline(pool: PgPool) {
    let f = fixture(&pool).await;
    let mut tx = resources::read_snapshot(&f.s).await.unwrap();
    let timeout: String = sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(timeout, "10s");
    // The same mechanism, shortened: PostgreSQL cancels the statement (57014),
    // which the API reports as a clear 503.
    sqlx::query("SET LOCAL statement_timeout='200ms'")
        .execute(&mut *tx)
        .await
        .unwrap();
    let error = sqlx::query("SELECT pg_sleep(5)")
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("57014")
    );
    let api: ApiError = error.into();
    assert_eq!(api.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(api.1, "The request took too long; narrow it and try again");
}
