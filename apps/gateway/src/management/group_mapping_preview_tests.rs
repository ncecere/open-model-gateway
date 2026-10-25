//! RED tests for the OIDC group-mapping preview (#89, review round 2):
//! preview must predict exactly what the real write path (PATCH/DELETE plus
//! `lifecycle::apply_group_grants`/`synchronize_scim_groups`, the same
//! predicates the preview evaluator reuses) then does -- both the
//! management write's own immediate, unconditional revocation and the net
//! effective-role change a later sign-in/SCIM reconciliation produces --
//! while itself never writing anything. See `apps/gateway/src/lifecycle.rs`
//! and `apps/gateway/src/management/group_mapping_preview.rs`.
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
/// [`call`] plus an `If-Match` header, for the digest/stale-confirmation tests.
async fn call_if_match(
    s: &Store,
    u: &BrowserPrincipal,
    method: &str,
    path: &str,
    body: Value,
    if_match: &str,
) -> (StatusCode, Value) {
    let response = routes()
        .layer(Extension(ManagementPrincipal::Browser(u.clone())))
        .layer(Extension(u.clone()))
        .with_state(s.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .header("if-match", if_match)
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
fn entry_for<'a>(list: &'a Value, user: Uuid) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|v| v["user_id"] == user.to_string())
        .unwrap_or_else(|| panic!("{user} not found in {list}"))
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
    assert_eq!(preview["complete"], true);
    assert!(preview.get("digest").is_some_and(Value::is_null));
    assert_eq!(preview["immediate"]["applies"], false);
    assert_eq!(
        users_in(&preview["diff"]["platform_role_gained"]),
        [user_a, user_b].into_iter().collect()
    );
    let entry = entry_for(&preview["diff"]["platform_role_gained"], user_a);
    assert_eq!(entry["before"], Value::Null);
    assert_eq!(entry["after"], "auditor");
    assert_eq!(preview["diff"]["platform_role_lost"], json!([]));
    assert_eq!(
        preview["counts"],
        json!({"gained":2,"lost":0,"truncated":false})
    );
    let after = counts(&pool).await;
    assert_unchanged(&before, &after);

    // Now apply for real: create the mapping, then run the exact write path
    // the preview previewed.
    let (status, _created) = call(
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

/// Review item 1: PATCH renaming a group (same target/role) immediately
/// revokes every existing grant from the old mapping, independent of
/// whether the renamed definition would recreate it. Review item 3: a
/// revoked key never reactivates even once the grant is restored.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn renaming_a_mapping_shows_the_immediate_revoke_even_when_reconciliation_restores_the_same_role(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let issuer = "https://idp.example.test";
    let holder = inert_user(&pool, "holder@example.test").await;
    scim_group(&pool, "engineering", &[holder]).await;
    let (status, created) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/oidc/group-mappings",
        json!({"issuer":issuer,"group_value":"eng","target_kind":"workspace","workspace_id":f.team,"workspace_role":"admin","enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let map = id(&created);
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source,mapping_id) VALUES($1,$2,'admin','group',$3)")
        .bind(f.team).bind(holder).bind(map).execute(&pool).await.unwrap();
    let (status, key) = key(&f, &f.owner, f.team, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    sqlx::query("UPDATE api_keys SET issued_to_user_id=$1 WHERE id=$2")
        .bind(holder)
        .bind(id(&key))
        .execute(&pool)
        .await
        .unwrap();

    // Preview a rename to "engineering" (holder is a SCIM member of both
    // the old and new value), same target and role.
    let (status, preview) = call(
        &f.s,
        &f.admin,
        "POST",
        &path_for(map),
        json!({"issuer":issuer,"group_value":"engineering","target_kind":"workspace","workspace_id":f.team,"workspace_role":"admin","enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["immediate"]["applies"], true);
    assert_eq!(preview["immediate"]["blocked_by_last_admin"], false);
    assert_eq!(
        users_in(&preview["immediate"]["workspace_membership_revoked"]),
        [holder].into_iter().collect(),
        "the rename immediately revokes the existing grant, even though it keeps the same role"
    );
    assert_eq!(preview["immediate"]["keys_revoked_count"], 1);
    // Reconciliation restores the same role, so the net diff shows no change.
    assert!(
        !users_in(&preview["diff"]["workspace_membership_gained"]).contains(&holder)
            && !users_in(&preview["diff"]["workspace_membership_lost"]).contains(&holder),
        "{preview}"
    );
    let digest = preview["digest"].as_str().unwrap().to_owned();

    // Apply for real.
    assert_eq!(
        call_if_match(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/oidc/group-mappings/{map}"),
            json!({"issuer":issuer,"group_value":"engineering","target_kind":"workspace","workspace_id":f.team,"workspace_role":"admin","enabled":true}),
            &digest,
        )
        .await
        .0,
        StatusCode::OK
    );
    let revoked_at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT revoked_at FROM workspace_membership_grants WHERE workspace_id=$1 AND user_id=$2 AND mapping_id=$3",
    )
    .bind(f.team)
    .bind(holder)
    .bind(map)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(revoked_at.is_some(), "the immediate revoke really happened");
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT revoked_at IS NOT NULL FROM api_keys WHERE id=$1")
            .bind(id(&key))
            .fetch_one(&pool)
            .await
            .unwrap(),
        "the key is permanently revoked by the immediate loss"
    );

    // Reconciliation (sign-in) restores the role for the renamed group.
    let mut tx = crate::db::begin(&pool).await.unwrap();
    crate::lifecycle::apply_group_grants(
        &mut tx,
        holder,
        issuer,
        &["engineering".to_owned()],
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let role: String = sqlx::query_scalar(
        "SELECT role FROM effective_workspace_memberships WHERE workspace_id=$1 AND user_id=$2",
    )
    .bind(f.team)
    .bind(holder)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        role, "admin",
        "reconciliation restored exactly what preview predicted"
    );
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT revoked_at IS NOT NULL FROM api_keys WHERE id=$1")
            .bind(id(&key))
            .fetch_one(&pool)
            .await
            .unwrap(),
        "revoked keys never reactivate even after the grant is restored"
    );
}

/// Review item 2: a manual `member` grant plus a group-sourced `admin`
/// grant is effectively `admin` by precedence; deleting the mapping is an
/// admin -> member downgrade, shown in the diff even though no roster
/// reconciliation is involved (workspace revocation is immediate).
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn deleting_a_mapping_shows_the_effective_precedence_downgrade(pool: PgPool) {
    let f = fixture(&pool).await;
    let issuer = "https://idp.example.test";
    let holder = inert_user(&pool, "holder2@example.test").await;
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,'member','manual')")
        .bind(f.team).bind(holder).execute(&pool).await.unwrap();
    let map = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,workspace_id,workspace_role,enabled) VALUES($1,$2,'ops',$3,$4,'admin',true)")
        .bind(map).bind(issuer).bind("workspace").bind(f.team).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source,mapping_id) VALUES($1,$2,'admin','group',$3)")
        .bind(f.team).bind(holder).bind(map).execute(&pool).await.unwrap();
    let (status, preview) = call(
        &f.s,
        &f.admin,
        "POST",
        &path_for(map),
        json!({"delete":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let entry = entry_for(&preview["diff"]["workspace_membership_lost"], holder);
    assert_eq!(entry["before"], "admin");
    assert_eq!(entry["after"], "member");
    let digest = preview["digest"].as_str().unwrap().to_owned();

    assert_eq!(
        call_if_match(
            &f.s,
            &f.admin,
            "DELETE",
            &format!("/api/v1/platform/oidc/group-mappings/{map}"),
            json!({}),
            &digest,
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
    .unwrap();
    assert_eq!(role, "member", "matches the previewed downgrade exactly");
}

/// Review item 3: a user suspended for entitlement loss, still inside its
/// grace window, is shown as reactivated/gained -- matching what
/// `synchronize_scim_groups` then actually does.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_user_suspended_for_entitlement_loss_in_grace_is_shown_as_reactivated(pool: PgPool) {
    let f = fixture(&pool).await;
    let issuer = "https://idp.example.test";
    let user = inert_user(&pool, "grace@example.test").await;
    sqlx::query("UPDATE users SET disabled_at=now(),disable_reason='entitlement_loss',cleanup_due_at=now()+interval '29 days' WHERE id=$1")
        .bind(user).execute(&pool).await.unwrap();
    scim_group(&pool, "staff", &[user]).await;
    let (status, preview) = call(
        &f.s,
        &f.admin,
        "POST",
        PATH,
        json!({"issuer":issuer,"group_value":"staff","target_kind":"platform","platform_role":"user","enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let entry = entry_for(&preview["diff"]["platform_role_gained"], user);
    assert_eq!(entry["reactivated"], true);

    // Real sync restores entitlement.
    let mut tx = crate::db::begin(&pool).await.unwrap();
    crate::lifecycle::synchronize_scim_groups(
        &mut tx,
        user,
        issuer,
        &["staff".to_owned()],
        &["staff".to_owned()],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT disabled_at IS NULL FROM users WHERE id=$1")
            .bind(user)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "matches the previewed reactivation"
    );
}

/// Review item 4: SCIM roster matching is exact, never case-folded.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn roster_matching_is_exact_case_sensitive(pool: PgPool) {
    let f = fixture(&pool).await;
    let user = inert_user(&pool, "case@example.test").await;
    scim_group(&pool, "Eng", &[user]).await;
    let (status, preview) = call(
        &f.s,
        &f.admin,
        "POST",
        PATH,
        json!({"issuer":"https://idp.example.test","group_value":"eng","target_kind":"platform","platform_role":"user","enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(
        preview["membership_basis"], "last_sign_in_derived",
        "a differently-cased SCIM group must not match: {preview}"
    );
    assert_eq!(preview["diff"]["platform_role_gained"], json!([]));
}

/// Review item 5: beyond the (test-lowered) roster cap, the whole preview
/// is marked incomplete instead of silently undercounting.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn roster_overflow_marks_the_preview_incomplete(pool: PgPool) {
    let f = fixture(&pool).await;
    let a = inert_user(&pool, "o1@example.test").await;
    let b = inert_user(&pool, "o2@example.test").await;
    let c = inert_user(&pool, "o3@example.test").await;
    let d = inert_user(&pool, "o4@example.test").await;
    scim_group(&pool, "overflow", &[a, b, c, d]).await;
    let (status, preview) = call(
        &f.s,
        &f.admin,
        "POST",
        PATH,
        json!({"issuer":"https://idp.example.test","group_value":"overflow","target_kind":"platform","platform_role":"user","enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["complete"], false);
    assert!(
        preview["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("evaluation cap")),
        "{preview}"
    );
}

/// Review item 7: a user who already holds the role via a manual grant
/// does not appear to "gain" anything -- the net effective role is unchanged.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_user_already_entitled_via_another_source_does_not_appear_to_gain(pool: PgPool) {
    let f = fixture(&pool).await;
    let user = inert_user(&pool, "already@example.test").await;
    sqlx::query(
        "INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'auditor','manual')",
    )
    .bind(user)
    .execute(&pool)
    .await
    .unwrap();
    scim_group(&pool, "redundant", &[user]).await;
    let (status, preview) = call(
        &f.s,
        &f.admin,
        "POST",
        PATH,
        json!({"issuer":"https://idp.example.test","group_value":"redundant","target_kind":"platform","platform_role":"auditor","enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert!(
        !users_in(&preview["diff"]["platform_role_gained"]).contains(&user),
        "already-entitled via another source: {preview}"
    );
}

/// Review item 6: `If-Match` refuses a mapping changed between review and apply.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_edit_between_preview_and_apply_is_refused(pool: PgPool) {
    let f = fixture(&pool).await;
    let issuer = "https://idp.example.test";
    let (status, created) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/oidc/group-mappings",
        json!({"issuer":issuer,"group_value":"ops","target_kind":"platform","platform_role":"user","enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let map = id(&created);
    let (status, preview) = call(
        &f.s,
        &f.admin,
        "POST",
        &path_for(map),
        json!({"delete":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let stale_digest = preview["digest"].as_str().unwrap().to_owned();

    // Someone else edits the mapping between review and apply.
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/oidc/group-mappings/{map}"),
            json!({"platform_role":"admin"})
        )
        .await
        .0,
        StatusCode::OK
    );

    // The stale digest is refused...
    let (status, body) = call_if_match(
        &f.s,
        &f.admin,
        "DELETE",
        &format!("/api/v1/platform/oidc/group-mappings/{map}"),
        json!({}),
        &stale_digest,
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED, "{body}");
    assert_eq!(body["error"]["reason"], "precondition_failed");
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM oidc_group_mappings WHERE id=$1)"
        )
        .bind(map)
        .fetch_one(&pool)
        .await
        .unwrap(),
        "a refused precondition must not have applied anything"
    );

    // ...a fresh preview's digest is accepted.
    let (status, fresh) = call(
        &f.s,
        &f.admin,
        "POST",
        &path_for(map),
        json!({"delete":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let fresh_digest = fresh["digest"].as_str().unwrap().to_owned();
    assert_ne!(fresh_digest, stale_digest);
    assert_eq!(
        call_if_match(
            &f.s,
            &f.admin,
            "DELETE",
            &format!("/api/v1/platform/oidc/group-mappings/{map}"),
            json!({}),
            &fresh_digest,
        )
        .await
        .0,
        StatusCode::OK
    );
    // No If-Match at all keeps the pre-existing behavior.
    let (status, created2) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/oidc/group-mappings",
        json!({"issuer":issuer,"group_value":"ops2","target_kind":"platform","platform_role":"user","enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "DELETE",
            &format!("/api/v1/platform/oidc/group-mappings/{}", id(&created2)),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
}

/// Review item 7 (standard review): never a personal workspace row.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn preview_never_names_a_personal_workspace(pool: PgPool) {
    let f = fixture(&pool).await;
    let user = inert_user(&pool, "personal@example.test").await;
    scim_group(&pool, "any", &[user]).await;
    let (status, preview) = call(
        &f.s,
        &f.admin,
        "POST",
        PATH,
        json!({"issuer":"https://idp.example.test","group_value":"any","target_kind":"workspace","workspace_id":f.personal,"workspace_role":"member","enabled":true}),
    )
    .await;
    // The personal workspace is not a valid mapping target at all
    // (`validate_mapping` only accepts a team/project workspace id).
    assert_eq!(status, StatusCode::NOT_FOUND, "{preview}");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn preview_bounds_each_list_to_200_and_reports_the_true_total(pool: PgPool) {
    let f = fixture(&pool).await;
    let issuer = "https://idp.example.test";
    let map = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role,enabled) VALUES($1,$2,'bulk','platform','auditor',true)")
        .bind(map)
        .bind(issuer)
        .execute(&pool)
        .await
        .unwrap();
    // 201 direct holders of this mapping's own grants (uncapped by the
    // roster cap, which only bounds the roster array): disabling the
    // mapping loses all 201 at once.
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "INSERT INTO users(id,email) SELECT gen_random_uuid(),'bulk-'||g||'@example.test' FROM generate_series(1,201) g RETURNING id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(user_id,role,source,mapping_id) SELECT u,'auditor','group',$1 FROM unnest($2::uuid[]) u")
        .bind(map)
        .bind(&ids)
        .execute(&pool)
        .await
        .unwrap();
    let (status, preview) = call(
        &f.s,
        &f.admin,
        "POST",
        &path_for(map),
        json!({"issuer":issuer,"group_value":"bulk","target_kind":"platform","platform_role":"auditor","enabled":false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(
        preview["diff"]["platform_role_lost"]
            .as_array()
            .unwrap()
            .len(),
        200
    );
    assert_eq!(preview["counts"]["lost"], 201);
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
