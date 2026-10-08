use super::*;
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn manual_membership_changes_preserve_group_sources_last_owner_and_revoke_departed_human_keys(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "DELETE",
            &format!("/api/v1/workspaces/{}/members/{}", f.team, f.owner.user_id),
            json!({})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    // Non-member Platform Admins have no owner powers through workspace routes;
    // platform-level membership administration uses the platform route.
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            &format!("/api/v1/workspaces/{}/members", f.team),
            json!({"user_id":f.owner.user_id,"role":"member"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            &format!("/api/v1/platform/workspaces/{}/members", f.team),
            json!({"user_id":f.owner.user_id,"role":"member"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let map = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,workspace_id,workspace_role) VALUES($1,'https://id.test','members','workspace',$2,'admin')").bind(map).bind(f.team).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source,mapping_id) VALUES($1,$2,'admin','group',$3)").bind(f.team).bind(f.member.user_id).bind(map).execute(&pool).await.unwrap();
    let (status, k) = key(&f, &f.member, f.team, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        call(
            &f.s,
            &f.member,
            "POST",
            &format!("/api/v1/workspaces/{}/members", f.team),
            json!({"user_id":f.outsider.user_id,"role":"owner"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "DELETE",
            &format!("/api/v1/workspaces/{}/members/{}", f.team, f.member.user_id),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(
        f.s.authenticate(k["token"].as_str().unwrap())
            .await
            .unwrap()
            .is_some()
    );
    let role: String = sqlx::query_scalar(
        "SELECT role FROM effective_workspace_memberships WHERE workspace_id=$1 AND user_id=$2",
    )
    .bind(f.team)
    .bind(f.member.user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(role, "admin");
    let (_, members) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("/api/v1/workspaces/{}/members", f.team),
        json!({}),
    )
    .await;
    let m = members["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["user_id"] == f.member.user_id.to_string())
        .unwrap();
    assert_eq!(m["membership_source"], "group");
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
    assert!(
        f.s.authenticate(k["token"].as_str().unwrap())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "POST",
            &format!("/api/v1/workspaces/{}/members", f.team),
            json!({"user_id":f.outsider.user_id,"role":"owner"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "DELETE",
            &format!("/api/v1/workspaces/{}/members/{}", f.team, f.owner.user_id),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn invitations_match_verified_principal_not_profile_and_preserve_manual_admin(pool: PgPool) {
    let f = fixture(&pool).await;
    // Direct-controller fixtures simulate the trusted verified principal; the identity
    // tests separately prove real signed callback and middleware construction of it.
    sqlx::query("UPDATE users SET email='profile-only@example.test' WHERE id=$1")
        .bind(f.outsider.user_id)
        .execute(&pool)
        .await
        .unwrap();
    member(&pool, f.project, f.outsider.user_id, "admin").await;
    let path = format!("/api/v1/workspaces/{}/invitations", f.project);
    let (status, profile_invite) = call(
        &f.s,
        &f.owner,
        "POST",
        &path,
        json!({"email":"profile-only@example.test","role":"admin"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        call(
            &f.s,
            &f.outsider,
            "POST",
            "/api/v1/invitations/accept",
            json!({"token":profile_invite["token"]})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (status, verified_invite) = call(
        &f.s,
        &f.owner,
        "POST",
        &path,
        json!({"email":f.outsider.email,"role":"member"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, accepted) = call(
        &f.s,
        &f.outsider,
        "POST",
        "/api/v1/invitations/accept",
        json!({"token":verified_invite["token"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let role: String = sqlx::query_scalar("SELECT role FROM workspace_membership_grants WHERE workspace_id=$1 AND user_id=$2 AND source='manual' AND revoked_at IS NULL")
        .bind(f.project).bind(f.outsider.user_id).fetch_one(&pool).await.unwrap();
    assert_eq!(role, "admin");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn invitations_bind_verified_session_email_are_one_time_and_recheck_inviter_authority(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let path = format!("/api/v1/workspaces/{}/invitations", f.project);
    assert_eq!(
        call(
            &f.s,
            &f.member,
            "POST",
            &path,
            json!({"email":f.member.email,"role":"member"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "POST",
            &path,
            json!({"email":f.member.email,"role":"owner"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (status, invite) = call(
        &f.s,
        &f.owner,
        "POST",
        &path,
        json!({"email":f.member.email.to_uppercase(),"role":"member"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = invite["token"].clone();
    assert_eq!(
        call(
            &f.s,
            &f.outsider,
            "POST",
            "/api/v1/invitations/accept",
            json!({"token":token})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (status, accepted) = call(
        &f.s,
        &f.member,
        "POST",
        "/api/v1/invitations/accept",
        json!({"token":token}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(accepted, json!({"workspace_id":f.project}));
    assert_eq!(
        call(
            &f.s,
            &f.member,
            "POST",
            "/api/v1/invitations/accept",
            json!({"token":token})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (_, list) = call(&f.s, &f.owner, "GET", &path, json!({})).await;
    assert!(!list.to_string().contains("token_hash"));
    assert!(!list.to_string().contains(token.as_str().unwrap()));
    let (_, revocable) = call(
        &f.s,
        &f.owner,
        "POST",
        &path,
        json!({"email":f.outsider.email,"role":"admin"}),
    )
    .await;
    let iid = id(&revocable);
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "DELETE",
            &format!("{path}/{iid}"),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &f.s,
            &f.outsider,
            "POST",
            "/api/v1/invitations/accept",
            json!({"token":revocable["token"]})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (_, invalidated) = call(
        &f.s,
        &f.owner,
        "POST",
        &path,
        json!({"email":f.outsider.email,"role":"member"}),
    )
    .await;
    // Simulate entitlement loss: not an administrative last-owner edit.
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(f.owner.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &f.s,
            &f.outsider,
            "POST",
            "/api/v1/invitations/accept",
            json!({"token":invalidated["token"]})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_membership_routes_are_platform_scoped_and_shared_only(pool: PgPool) {
    let f = fixture(&pool).await;
    let path = format!("/api/v1/platform/workspaces/{}/members", f.project);
    // Auditor reads, cannot write; ordinary users and workspace owners cannot use platform routes.
    let (status, rows) = call(&f.s, &f.auditor, "GET", &path, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rows["data"].as_array().unwrap().len(), 1);
    for u in [&f.auditor, &f.owner] {
        assert_eq!(
            call(
                &f.s,
                u,
                "POST",
                &path,
                json!({"user_id":f.outsider.user_id,"role":"member"})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        call(&f.s, &f.owner, "GET", &path, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    // Admin can set owners and remove manual grants, guarded by last-owner.
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            &path,
            json!({"user_id":f.outsider.user_id,"role":"owner"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "DELETE",
            &format!("{path}/{}", f.owner.user_id),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "DELETE",
            &format!("{path}/{}", f.outsider.user_id),
            json!({})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    // Personal workspaces are not addressable here.
    for method in ["GET", "POST"] {
        assert_eq!(
            call(
                &f.s,
                &f.admin,
                method,
                &format!("/api/v1/platform/workspaces/{}/members", f.personal),
                json!({"user_id":f.outsider.user_id,"role":"member"})
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn member_candidates_are_bounded_entitled_non_members_for_workspace_admins(pool: PgPool) {
    let f = fixture(&pool).await;
    for i in 0..25 {
        user(&pool, &format!("cand{i:02}"), "user").await;
    }
    // Suspended and unentitled users never appear.
    let suspended = user(&pool, "cand-suspended", "user").await;
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(suspended.user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'cand-noentitlement@example.test')")
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    let path = |q: &str| format!("/api/v1/workspaces/{}/member-candidates?q={q}", f.team);
    let (status, v) = call(&f.s, &f.owner, "GET", &path("CAND"), json!({})).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let rows = v["data"].as_array().unwrap();
    assert_eq!(rows.len(), 20);
    assert!(rows.iter().all(|r| {
        let o = r.as_object().unwrap();
        // user_id, email and the optional display name (presentation only).
        o.len() == 3
            && r["email"].as_str().unwrap().starts_with("cand")
            && r["display_name"].is_null()
    }));
    assert!(!v.to_string().contains("suspended") && !v.to_string().contains("noentitlement"));
    // Prefix matches sort first; existing members are excluded.
    let (_, v) = call(&f.s, &f.owner, "GET", &path("member"), json!({})).await;
    assert!(v["data"].as_array().unwrap().is_empty(), "{v}");
    let (_, v) = call(&f.s, &f.owner, "GET", &path("outsider"), json!({})).await;
    assert_eq!(v["data"][0]["user_id"], f.outsider.user_id.to_string());
    // Members, non-member platform staff and personal workspaces are refused.
    for u in [&f.member, &f.admin, &f.auditor] {
        assert_eq!(
            call(&f.s, u, "GET", &path("cand"), json!({})).await.0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "GET",
            &format!("/api/v1/workspaces/{}/member-candidates?q=cand", f.personal),
            json!({})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    for bad in ["", "%20%20"] {
        assert_eq!(
            call(&f.s, &f.owner, "GET", &path(bad), json!({})).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    // Picked candidates are added by user id through the existing endpoint.
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "POST",
            &format!("/api/v1/workspaces/{}/members", f.team),
            json!({"user_id":f.outsider.user_id,"role":"member"})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, v) = call(&f.s, &f.owner, "GET", &path("outsider"), json!({})).await;
    assert!(v["data"].as_array().unwrap().is_empty());
}
