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
