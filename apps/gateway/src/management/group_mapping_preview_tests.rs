//! RED tests for the OIDC group-mapping preview (#89): preview must predict
//! exactly what the real write path (`lifecycle::apply_group_grants`, the
//! same predicates the preview evaluator reuses) then does, while itself
//! never writing anything. See `apps/gateway/src/lifecycle.rs` and
//! `apps/gateway/src/management/group_mapping_preview.rs`.
use super::*;
use crate::identity::AdminTokenContext;

const PATH: &str = "/api/v1/platform/oidc/group-mappings/preview";
fn path_for(id: Uuid) -> String {
    format!("/api/v1/platform/oidc/group-mappings/{id}/preview")
}

struct Counts {
    mappings: i64,
    platform_grants: i64,
    workspace_grants: i64,
    audit: i64,
    access_version: i64,
}
async fn counts(pool: &PgPool) -> Counts {
    Counts {
        mappings: sqlx::query_scalar("SELECT count(*) FROM oidc_group_mappings")
            .fetch_one(pool)
            .await
            .unwrap(),
        platform_grants: sqlx::query_scalar("SELECT count(*) FROM platform_role_grants")
            .fetch_one(pool)
            .await
            .unwrap(),
        workspace_grants: sqlx::query_scalar("SELECT count(*) FROM workspace_membership_grants")
            .fetch_one(pool)
            .await
            .unwrap(),
        audit: sqlx::query_scalar("SELECT count(*) FROM audit_events")
            .fetch_one(pool)
            .await
            .unwrap(),
        access_version: sqlx::query_scalar(
            "SELECT version FROM config_versions WHERE topic='access'",
        )
        .fetch_one(pool)
        .await
        .unwrap(),
    }
}
fn assert_unchanged(before: &Counts, after: &Counts) {
    assert_eq!(before.mappings, after.mappings, "mappings written");
    assert_eq!(
        before.platform_grants, after.platform_grants,
        "platform grants written"
    );
    assert_eq!(
        before.workspace_grants, after.workspace_grants,
        "workspace grants written"
    );
    assert_eq!(before.audit, after.audit, "audit written");
    assert_eq!(
        before.access_version, after.access_version,
        "config version bumped"
    );
}
async fn scim_group(pool: &PgPool, display_name: &str, members: &[Uuid]) {
    let group = Uuid::new_v4();
    sqlx::query("INSERT INTO scim_groups(id,display_name) VALUES($1,$2)")
        .bind(group)
        .bind(display_name)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO scim_group_members(group_id,user_id) SELECT $1,u FROM unnest($2::uuid[]) u",
    )
    .bind(group)
    .bind(members)
    .execute(pool)
    .await
    .unwrap();
}
async fn inert_user(pool: &PgPool, email: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
        .bind(id)
        .bind(email)
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn call_as_token(
    s: &Store,
    token: ManagementPrincipal,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = routes()
        .layer(Extension(token))
        .with_state(s.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
fn users_in(list: &Value) -> std::collections::BTreeSet<Uuid> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|v| Uuid::parse_str(v["user_id"].as_str().unwrap()).unwrap())
        .collect()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn preview_predicts_scim_platform_role_gain_exactly_matching_apply(pool: PgPool) {
    let f = fixture(&pool).await;
    let issuer = "https://idp.example.test";
    let user_a = inert_user(&pool, "a@example.test").await;
    let user_b = inert_user(&pool, "b@example.test").await;
    scim_group(&pool, "eng", &[user_a, user_b]).await;
    let before = counts(&pool).await;
    let (status, preview) = call(
        &f.s,
        &f.admin,
        "POST",
        PATH,
        json!({"issuer":issuer,"group_value":"eng","target_kind":"platform","platform_role":"auditor","enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["membership_basis"], "scim");
    assert_eq!(
        users_in(&preview["diff"]["platform_role_gained"]),
        [user_a, user_b].into_iter().collect()
    );
    assert_eq!(preview["diff"]["platform_role_lost"], json!([]));
    assert_eq!(
        preview["counts"],
        json!({"gained":2,"lost":0,"truncated":false})
    );
    assert!(preview.get("expected_revision").is_none());
    let after = counts(&pool).await;
    assert_unchanged(&before, &after);

    // Now apply for real: create the mapping, then run the exact write path
    // the preview previewed.
    let (status, created) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/oidc/group-mappings",
        json!({"issuer":issuer,"group_value":"eng","target_kind":"platform","platform_role":"auditor","enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for user in [user_a, user_b] {
        let mut tx = crate::db::begin(&pool).await.unwrap();
        crate::lifecycle::apply_group_grants(
            &mut tx,
            user,
            issuer,
            &["eng".to_owned()],
            Some(&["eng".to_owned()]),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    for user in [user_a, user_b] {
        let role: String = sqlx::query_scalar(
            "SELECT role FROM platform_role_grants WHERE user_id=$1 AND source='group' AND revoked_at IS NULL",
        )
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(role, "auditor", "apply must match what preview predicted");
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn preview_falls_back_to_sign_in_derived_basis_and_warns_without_a_scim_roster(pool: PgPool) {
    let f = fixture(&pool).await;
    let issuer = "https://idp.example.test";
    let map = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,workspace_id,workspace_role,enabled) VALUES($1,$2,'ops',$3,$4,'member',true)")
        .bind(map).bind(issuer).bind("workspace").bind(f.team).execute(&pool).await.unwrap();
    let holder = inert_user(&pool, "holder@example.test").await;
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source,mapping_id) VALUES($1,$2,'member','group',$3)")
        .bind(f.team).bind(holder).bind(map).execute(&pool).await.unwrap();
    let before = counts(&pool).await;
    // Preview promoting the same mapping's role from member to admin.
    let (status, preview) = call(
        &f.s,
        &f.admin,
        "POST",
        &path_for(map),
        json!({"issuer":issuer,"group_value":"ops","target_kind":"workspace","workspace_id":f.team,"workspace_role":"admin","enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["membership_basis"], "last_sign_in_derived");
    assert!(
        preview["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("No SCIM roster")),
        "{preview}"
    );
    assert_eq!(
        users_in(&preview["diff"]["workspace_membership_gained"]),
        [holder].into_iter().collect()
    );
    let after = counts(&pool).await;
    assert_unchanged(&before, &after);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn preview_delete_matches_what_deleting_the_mapping_then_revokes(pool: PgPool) {
    let f = fixture(&pool).await;
    let issuer = "https://idp.example.test";
    let map = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,workspace_id,workspace_role,enabled) VALUES($1,$2,'ops',$3,$4,'admin',true)")
        .bind(map).bind(issuer).bind("workspace").bind(f.team).execute(&pool).await.unwrap();
    let holder = inert_user(&pool, "holder2@example.test").await;
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source,mapping_id) VALUES($1,$2,'admin','group',$3)")
        .bind(f.team).bind(holder).bind(map).execute(&pool).await.unwrap();
    let before = counts(&pool).await;
    let (status, preview) = call(
        &f.s,
        &f.admin,
        "POST",
        &path_for(map),
        json!({"delete":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(
        users_in(&preview["diff"]["workspace_membership_lost"]),
        [holder].into_iter().collect()
    );
    let after = counts(&pool).await;
    assert_unchanged(&before, &after);
    // An omitted body is the same request as an explicit delete.
    let (status, omitted) = call(&f.s, &f.admin, "POST", &path_for(map), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(omitted["diff"], preview["diff"]);

    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "DELETE",
            &format!("/api/v1/platform/oidc/group-mappings/{map}"),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    let role: String = sqlx::query_scalar(
        "SELECT role FROM effective_workspace_memberships WHERE workspace_id=$1 AND user_id=$2",
    )
    .bind(f.team)
    .bind(holder)
    .fetch_one(&pool)
    .await
    .unwrap_or_default();
    assert_eq!(role, "", "the previewed grant was actually revoked");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_disabled_scim_member_never_appears_as_gaining_anything(pool: PgPool) {
    let f = fixture(&pool).await;
    let active = inert_user(&pool, "active@example.test").await;
    let disabled = inert_user(&pool, "disabled@example.test").await;
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(disabled)
        .execute(&pool)
        .await
        .unwrap();
    scim_group(&pool, "staff", &[active, disabled]).await;
    let (status, preview) = call(
        &f.s,
        &f.admin,
        "POST",
        PATH,
        json!({"issuer":"https://idp.example.test","group_value":"staff","target_kind":"platform","platform_role":"auditor","enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let gained = users_in(&preview["diff"]["platform_role_gained"]);
    assert!(gained.contains(&active));
    assert!(
        !gained.contains(&disabled),
        "a deactivated user must never appear to gain a grant: {preview}"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn preview_bounds_each_list_to_200_and_reports_the_true_total(pool: PgPool) {
    let f = fixture(&pool).await;
    let group = Uuid::new_v4();
    sqlx::query("INSERT INTO scim_groups(id,display_name) VALUES($1,'bulk')")
        .bind(group)
        .execute(&pool)
        .await
        .unwrap();
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "INSERT INTO users(id,email) SELECT gen_random_uuid(),'bulk-'||g||'@example.test' FROM generate_series(1,201) g RETURNING id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO scim_group_members(group_id,user_id) SELECT $1,u FROM unnest($2::uuid[]) u",
    )
    .bind(group)
    .bind(&ids)
    .execute(&pool)
    .await
    .unwrap();
    let (status, preview) = call(
        &f.s,
        &f.admin,
        "POST",
        PATH,
        json!({"issuer":"https://idp.example.test","group_value":"bulk","target_kind":"platform","platform_role":"auditor","enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(
        preview["diff"]["platform_role_gained"]
            .as_array()
            .unwrap()
            .len(),
        200
    );
    assert_eq!(preview["counts"]["gained"], 201);
    assert_eq!(preview["counts"]["truncated"], true);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn preview_rejects_an_unknown_field_the_same_way_create_does(pool: PgPool) {
    let f = fixture(&pool).await;
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            PATH,
            json!({"issuer":"https://idp.example.test","group_value":"g","target_kind":"platform","platform_role":"user","enabled":false,"unexpected":true})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            PATH,
            json!({"group_value":"g","target_kind":"platform","platform_role":"user","enabled":false})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST,
        "a proposal missing a required field is invalid like create/update"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn authority_matrix_admin_only_browser_only(pool: PgPool) {
    let f = fixture(&pool).await;
    let body = json!({"issuer":"https://idp.example.test","group_value":"g","target_kind":"platform","platform_role":"user","enabled":false});
    assert_eq!(
        call(&f.s, &f.admin, "POST", PATH, body.clone()).await.0,
        StatusCode::OK
    );
    for actor in [&f.auditor, &f.owner, &f.outsider] {
        assert_eq!(
            call(&f.s, actor, "POST", PATH, body.clone()).await.0,
            StatusCode::FORBIDDEN
        );
    }
    let token = ManagementPrincipal::AdminToken(AdminTokenContext {
        token_id: Uuid::new_v4(),
        owner_user_id: f.admin.user_id,
        scopes: vec!["access:write".to_owned()],
        ip_allowlist: None,
        client_address: None,
    });
    assert_eq!(
        call_as_token(&f.s, token, "POST", PATH, body).await.0,
        StatusCode::FORBIDDEN,
        "an admin token must never reach this handler, even with the write scope"
    );
}
