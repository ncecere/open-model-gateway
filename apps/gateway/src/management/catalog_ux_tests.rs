//! Disabled-workspace read-only views for platform readers, catalog "who gets
//! it" summaries and the atomic catalog Defaults matrix.
use super::*;

async fn get(f: &Fixture, u: &BrowserPrincipal, path: &str) -> (StatusCode, Value) {
    call(&f.s, u, "GET", path, json!({})).await
}
fn ids(v: &Value) -> Vec<String> {
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_readers_read_disabled_workspace_configuration_read_only(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = model(&pool, "disabled-view-model").await;
    direct(&pool, f.project, m).await;
    let ws = f.project;
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/workspaces/{ws}"),
            json!({"disabled":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    let reads = [
        format!("/api/v1/platform/workspaces/{ws}"),
        format!("/api/v1/platform/workspaces/{ws}/policy"),
        format!("/api/v1/platform/workspaces/{ws}/catalogs"),
        format!("/api/v1/platform/workspaces/{ws}/members"),
        format!("/api/v1/workspaces/{ws}/models"),
        format!("/api/v1/workspaces/{ws}/access"),
    ];
    for u in [&f.admin, &f.auditor] {
        for path in &reads {
            assert_eq!(get(&f, u, path).await.0, StatusCode::OK, "{path}");
        }
        let (_, access) = get(&f, u, &format!("/api/v1/workspaces/{ws}/access")).await;
        assert_eq!(access["workspace_disabled"], true);
        let row = access["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["model_id"] == m.to_string())
            .unwrap()
            .clone();
        // Configuration still shows the assignment, but nothing is usable while disabled.
        assert_eq!(row["status"], "unavailable");
        assert!(
            row["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["code"] == "workspace_disabled" && r["layer"] == "platform")
        );
        let (_, models) = get(&f, u, &format!("/api/v1/workspaces/{ws}/models")).await;
        assert_eq!(ids(&models), vec![m.to_string()]);
        let (_, policy) = get(&f, u, &format!("/api/v1/platform/workspaces/{ws}/policy")).await;
        assert_eq!(policy["mode"], "inherit");
    }
    // Members (without platform authority) and outsiders still get 404, never a read.
    for u in [&f.owner, &f.outsider] {
        for path in [
            format!("/api/v1/workspaces/{ws}/models"),
            format!("/api/v1/workspaces/{ws}/access"),
        ] {
            assert_eq!(get(&f, u, &path).await.0, StatusCode::NOT_FOUND, "{path}");
        }
    }
    // Mutations of a disabled workspace keep refusing, even for Platform Admins.
    let policy = json!({"requests_per_minute":5,"tokens_per_minute":null,"concurrent_requests":null,"monthly_budget_microusd":null});
    for (u, method, path, body) in [
        (
            &f.admin,
            "PUT",
            format!("/api/v1/platform/workspaces/{ws}/policy"),
            policy.clone(),
        ),
        (
            &f.admin,
            "DELETE",
            format!("/api/v1/platform/workspaces/{ws}/policy"),
            json!({}),
        ),
        (
            &f.owner,
            "PUT",
            format!("/api/v1/workspaces/{ws}/policy"),
            policy.clone(),
        ),
        (
            &f.admin,
            "POST",
            format!("/api/v1/workspaces/{ws}/models"),
            json!({"model_id":m}),
        ),
        (
            &f.admin,
            "POST",
            format!("/api/v1/platform/workspaces/{ws}/members"),
            json!({"user_id":f.outsider.user_id,"role":"member"}),
        ),
    ] {
        let status = call(&f.s, u, method, &path, body).await.0;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
    }
    // Key-level access stays member-only and needs an active workspace.
    assert_eq!(
        get(
            &f,
            &f.admin,
            &format!("/api/v1/workspaces/{ws}/keys/{}/access", Uuid::new_v4())
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // Active workspaces are unaffected: no disabled flag or reason.
    let (status, access) = get(
        &f,
        &f.auditor,
        &format!("/api/v1/workspaces/{}/access", f.team),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(access["workspace_disabled"], false);
    // Personal workspaces stay owner-private even when another reader asks.
    assert_eq!(
        get(
            &f,
            &f.admin,
            &format!("/api/v1/workspaces/{}/access", f.personal)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn catalog_summaries_and_defaults_matrix_keep_live_semantics(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = model(&pool, "matrix-model").await;
    let create = |name: &'static str| {
        let f = &f;
        async move {
            let (status, v) = call(
                &f.s,
                &f.admin,
                "POST",
                "/api/v1/platform/catalogs",
                json!({"name":name,"description":null}),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            id(&v)
        }
    };
    let (a, b) = (create("Approved").await, create("Beta").await);
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PUT",
            &format!("/api/v1/platform/catalogs/{a}/models"),
            json!({"model_ids":[m]})
        )
        .await
        .0,
        StatusCode::OK
    );
    // Auditors read the matrix but cannot save it; partial bodies are rejected.
    let (_, empty) = get(&f, &f.auditor, "/api/v1/platform/catalog-defaults").await;
    assert_eq!(empty, json!({"personal":[],"team":[],"project":[]}));
    let matrix = json!({"personal":[a],"team":[b,a],"project":[a]});
    assert_eq!(
        call(
            &f.s,
            &f.auditor,
            "PUT",
            "/api/v1/platform/catalog-defaults",
            matrix.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert!(
        call(
            &f.s,
            &f.admin,
            "PUT",
            "/api/v1/platform/catalog-defaults",
            json!({"personal":[a]})
        )
        .await
        .0
        .is_client_error()
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PUT",
            "/api/v1/platform/catalog-defaults",
            json!({"personal":[Uuid::new_v4()],"team":[],"project":[]})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (status, saved) = call(
        &f.s,
        &f.admin,
        "PUT",
        "/api/v1/platform/catalog-defaults",
        matrix,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["changed"], json!(["personal", "team", "project"]));
    let (_, current) = get(&f, &f.admin, "/api/v1/platform/catalog-defaults").await;
    let mut team = vec![a.to_string(), b.to_string()];
    team.sort();
    assert_eq!(current["team"], json!(team));
    // Live type defaults: the project inherits A, so its owner can add the model.
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "POST",
            &format!("/api/v1/workspaces/{}/models", f.project),
            json!({"model_id":m})
        )
        .await
        .0,
        StatusCode::OK
    );
    // Workspaces' own catalog choices: a team (listed) and a personal one (count only).
    for ws in [f.team, f.personal] {
        assert_eq!(
            call(
                &f.s,
                &f.admin,
                "PUT",
                &format!("/api/v1/platform/workspaces/{ws}/catalogs"),
                json!({"mode":"replace","catalog_ids":[b]})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    let (_, list) = get(&f, &f.auditor, "/api/v1/platform/catalogs").await;
    let row = |id: Uuid| {
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id.to_string())
            .unwrap()
            .clone()
    };
    let (ra, rb) = (row(a), row(b));
    assert_eq!(ra["model_count"], 1);
    assert_eq!(ra["models"][0]["public_name"], "matrix-model");
    assert_eq!(ra["default_for"], json!(["personal", "team", "project"]));
    assert_eq!(ra["own_choice_count"], 0);
    assert_eq!(rb["model_count"], 0);
    assert_eq!(rb["default_for"], json!(["team"]));
    assert_eq!(rb["own_choice_count"], 2);
    let (_, detail) = get(&f, &f.auditor, &format!("/api/v1/platform/catalogs/{b}")).await;
    assert_eq!(
        detail["own_choice"],
        json!({"workspaces":[{"id":f.team,"name":"Team","kind":"team","disabled":false}],"personal_count":1})
    );
    assert!(!detail.to_string().contains("Personal"));
    // Unchecking a catalog retires selections made only through it; checking it again never restores them.
    let (status, saved) = call(
        &f.s,
        &f.admin,
        "PUT",
        "/api/v1/platform/catalog-defaults",
        json!({"personal":[a],"team":[a,b],"project":[]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["changed"], json!(["project"]));
    let selected = format!("/api/v1/workspaces/{}/models", f.project);
    assert_eq!(get(&f, &f.owner, &selected).await.1["data"], json!([]));
    call(
        &f.s,
        &f.admin,
        "PUT",
        "/api/v1/platform/catalog-defaults",
        json!({"personal":[a],"team":[a,b],"project":[a]}),
    )
    .await;
    assert_eq!(get(&f, &f.owner, &selected).await.1["data"], json!([]));
    // An unchanged save changes and audits nothing.
    let audits = |pool: PgPool| async move {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM audit_events WHERE action='catalog.type_defaults_replaced'",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let before = audits(pool.clone()).await;
    let (_, saved) = call(
        &f.s,
        &f.admin,
        "PUT",
        "/api/v1/platform/catalog-defaults",
        json!({"personal":[a],"team":[b,a],"project":[a]}),
    )
    .await;
    assert_eq!(saved["changed"], json!([]));
    assert_eq!(audits(pool.clone()).await, before);
}
