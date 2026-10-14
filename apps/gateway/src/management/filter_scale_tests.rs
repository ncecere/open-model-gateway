//! R06 (#69): Logs, Usage and admin pickers search keys and workspaces on the
//! server, and rows carry the workspace names the pages show.
use super::*;

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_lists_search_by_name(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = model(&pool, "company/smart").await;
    direct(&pool, f.team, m).await;
    let keys = format!("/api/v1/workspaces/{}/keys", f.team);
    for name in ["Laptop", "Build server", "laptop backup"] {
        let (status, body) = call(
            &f.s,
            &f.owner,
            "POST",
            &keys,
            json!({"name":name,"expires_in_days":30}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, found) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("{keys}?q=LAPTOP&limit=20"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    let mut names: Vec<&str> = found["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["Laptop", "laptop backup"]);
    let (status, _) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("{keys}?q={}", "x".repeat(201)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A member's search still sees only their own keys.
    let (_, member) = call(
        &f.s,
        &f.member,
        "GET",
        &format!("{keys}?q=laptop"),
        Value::Null,
    )
    .await;
    assert_eq!(member["data"], json!([]));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn group_mappings_name_their_workspace(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, body) = call(&f.s, &f.admin, "POST", "/api/v1/platform/oidc/group-mappings", json!({"issuer":"https://idp.example.invalid","group_value":"eng","target_kind":"workspace","platform_role":null,"workspace_id":f.team,"workspace_role":"member","enabled":true})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, list) = call(
        &f.s,
        &f.auditor,
        "GET",
        "/api/v1/platform/oidc/group-mappings",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["data"][0]["workspace_name"], "Team");
    assert_eq!(list["data"][0]["workspace_kind"], "team");
}

/// "Add models" and "Assign models" search only models that can still be added,
/// so 20 matches are never all already present (R06, #68).
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn model_search_can_exclude_models_already_added(pool: PgPool) {
    let f = fixture(&pool).await;
    let added = model(&pool, "company/added").await;
    let other = model(&pool, "company/other").await;
    let catalog = Uuid::new_v4();
    sqlx::query("INSERT INTO catalogs(id,name) VALUES($1,'Standard')")
        .bind(catalog)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalog_models(catalog_id,model_id) VALUES($1,$2)")
        .bind(catalog)
        .bind(added)
        .execute(&pool)
        .await
        .unwrap();
    direct(&pool, f.team, added).await;
    let ids = |v: &Value| -> Vec<String> {
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_owned())
            .collect()
    };
    for query in [
        format!("not_in_catalog={catalog}"),
        format!("not_direct_in={}", f.team),
    ] {
        let (status, body) = call(
            &f.s,
            &f.admin,
            "GET",
            &format!("/api/v1/platform/models?q=company&{query}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(ids(&body), [other.to_string()], "{query}");
    }
    let (_, all) = call(
        &f.s,
        &f.admin,
        "GET",
        "/api/v1/platform/models?q=company",
        Value::Null,
    )
    .await;
    assert_eq!(ids(&all).len(), 2);
}
