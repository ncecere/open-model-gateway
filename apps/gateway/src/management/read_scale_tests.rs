//! R06 (#65): large management lists stay fast and can't freeze other
//! requests. Lists select a page before building its rows, per-user lookups
//! are indexed, read-only lists use lock-free snapshots, and a slow read is
//! cancelled by PostgreSQL at the management read deadline.
use super::*;
use std::time::Duration;

/// `label` names the endpoint in failure messages (paths carry ids).
async fn get_status(s: &Store, u: &BrowserPrincipal, path: &str, label: &str) -> StatusCode {
    let response = routes()
        .layer(Extension(ManagementPrincipal::Browser(u.clone())))
        .layer(Extension(u.clone()))
        .with_state(s.clone())
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap());
    tokio::time::timeout(Duration::from_secs(5), response)
        .await
        .unwrap_or_else(|_| panic!("{label} waited on a lock"))
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
    for (u, path, label) in [
        (
            &f.auditor,
            "/api/v1/platform/users".to_owned(),
            "platform users",
        ),
        (
            &f.auditor,
            format!("/api/v1/platform/users/{owner}"),
            "platform user",
        ),
        (
            &f.auditor,
            "/api/v1/platform/workspaces".to_owned(),
            "platform workspaces",
        ),
        (
            &f.owner,
            format!("/api/v1/workspaces/{team}/members"),
            "members",
        ),
        (&f.owner, format!("/api/v1/workspaces/{team}/keys"), "keys"),
        (
            &f.owner,
            format!("/api/v1/workspaces/{team}/service-accounts"),
            "service accounts",
        ),
    ] {
        assert_eq!(
            get_status(&f.s, u, &path, label).await,
            StatusCode::OK,
            "{label}"
        );
    }
    // Authorization is still live: an outsider is refused without waiting either.
    assert_eq!(
        get_status(
            &f.s,
            &f.outsider,
            "/api/v1/platform/users",
            "platform users (outsider)"
        )
        .await,
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

/// Most times any correlated subquery ran while building returned rows: the
/// page's own selection (the materialized `page` CTE, whose filters and sort
/// key must visit every candidate) is not counted.
fn row_subplan_loops(node: &Value) -> i64 {
    if node["Subplan Name"] == "CTE page" {
        return 0;
    }
    let own = if node["Subplan Name"].is_string() {
        node["Actual Loops"].as_i64().unwrap_or(0)
    } else {
        0
    };
    node["Plans"]
        .as_array()
        .into_iter()
        .flatten()
        .map(row_subplan_loops)
        .fold(own, i64::max)
}
fn names(v: &Value, field: &str) -> Vec<String> {
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r[field].as_str().unwrap().to_owned())
        .collect()
}
/// 300 enabled models in a catalog offered to every team workspace.
async fn bulk_catalog_models(pool: &PgPool) {
    sqlx::query("INSERT INTO models(id,public_name) SELECT gen_random_uuid(),'bulk-'||lpad(i::text,3,'0') FROM generate_series(1,300) i")
        .execute(pool)
        .await
        .unwrap();
    let catalog = Uuid::now_v7();
    sqlx::query("INSERT INTO catalogs(id,name) VALUES($1,'Bulk')")
        .bind(catalog)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalog_models(catalog_id,model_id) SELECT $1,id FROM models WHERE public_name LIKE 'bulk-%'")
        .bind(catalog)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_type_catalogs(kind,catalog_id) VALUES('team',$1)")
        .bind(catalog)
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_models_list_builds_rows_only_for_the_requested_page(pool: PgPool) {
    let f = fixture(&pool).await;
    bulk_catalog_models(&pool).await;
    // Same page, order and filters as before.
    let (status, v) = call(
        &f.s,
        &f.auditor,
        "GET",
        "/api/v1/platform/models?limit=3&offset=51&q=bulk-2",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(
        names(&v, "public_name"),
        ["bulk-251", "bulk-252", "bulk-253"].map(String::from)
    );
    let (_, v) = call(
        &f.s,
        &f.auditor,
        "GET",
        "/api/v1/platform/models?limit=2&q=bulk-29&sort=newest",
        json!({}),
    )
    .await;
    assert_eq!(v["data"].as_array().unwrap().len(), 2, "{v}");
    for sort in [
        super::models_ux::PlatformModelSort::Name,
        super::models_ux::PlatformModelSort::Price,
    ] {
        let plan: Value = sqlx::query_scalar(&format!(
            "EXPLAIN (ANALYZE, FORMAT JSON) {}",
            super::models_ux::platform_models_sql(sort)
        ))
        .bind(None::<String>)
        .bind(None::<bool>)
        .bind(None::<Uuid>)
        .bind(None::<i64>)
        .bind(None::<String>)
        .bind(true)
        .bind("deny")
        .bind(None::<String>)
        .bind(10_i64)
        .bind(250_i64)
        .bind(None::<Uuid>)
        .bind(None::<Uuid>)
        .fetch_one(&pool)
        .await
        .unwrap();
        // Readiness counts, connections, catalogs and prices run once per returned row.
        let loops = row_subplan_loops(&plan[0]["Plan"]);
        assert!(loops > 0, "expected per-row subqueries in {plan}");
        assert!(
            loops <= 10,
            "subqueries ran {loops} times for a 10-row page"
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn workspace_catalog_builds_rows_only_for_the_requested_page(pool: PgPool) {
    let f = fixture(&pool).await;
    bulk_catalog_models(&pool).await;
    let (status, v) = call(
        &f.s,
        &f.member,
        "GET",
        &format!(
            "/api/v1/workspaces/{}/catalog?limit=3&offset=250&q=BULK",
            f.team
        ),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(
        names(&v, "public_name"),
        ["bulk-251", "bulk-252", "bulk-253"].map(String::from)
    );
    assert_eq!(v["data"][0]["eligibility"], "available_from_catalog");
    // Authorization is unchanged: an outsider still cannot read it.
    let (status, _) = call(
        &f.s,
        &f.outsider,
        "GET",
        &format!("/api/v1/workspaces/{}/catalog", f.team),
        json!({}),
    )
    .await;
    assert_ne!(status, StatusCode::OK);
    for sort in [
        super::models_ux::PlatformModelSort::Name,
        super::models_ux::PlatformModelSort::Price,
    ] {
        let plan: Value = sqlx::query_scalar(&format!(
            "EXPLAIN (ANALYZE, FORMAT JSON) {}",
            super::models_ux::workspace_catalog_sql(sort)
        ))
        .bind(f.team)
        .bind(None::<String>)
        .bind(None::<String>)
        .bind(10_i64)
        .bind(250_i64)
        .fetch_one(&pool)
        .await
        .unwrap();
        // Eligibility, route counts and prices run once per returned row.
        let loops = row_subplan_loops(&plan[0]["Plan"]);
        assert!(loops > 0, "expected per-row subqueries in {plan}");
        assert!(
            loops <= 10,
            "subqueries ran {loops} times for a 10-row page"
        );
    }
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
