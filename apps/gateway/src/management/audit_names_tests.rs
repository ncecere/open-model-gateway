//! R06 (#67): audit and key lists carry the names the dashboard shows, so it
//! never loads every user, member or workspace to turn ids into names.
use super::*;

async fn event(pool: &PgPool, actor: Uuid, ws: Option<Uuid>, kind: &str, id: Uuid) {
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,workspace_id,action,resource_type,resource_id,metadata) VALUES($1,$2,$3,'test.change',$4,$5,'{}')")
        .bind(Uuid::now_v7()).bind(actor).bind(ws).bind(kind).bind(id).execute(pool).await.unwrap();
}
fn row<'a>(rows: &'a Value, kind: &str) -> &'a Value {
    rows["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["resource_type"] == kind)
        .unwrap_or_else(|| panic!("no {kind} event"))
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_audit_names_actors_workspaces_and_targets(pool: PgPool) {
    let f = fixture(&pool).await;
    sqlx::query("UPDATE users SET display_name='Owen Owner' WHERE id=$1")
        .bind(f.owner.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let m = model(&pool, "company/smart").await;
    let (center, catalog) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO cost_centers(id,name,code) VALUES($1,'Research','CC-1')")
        .bind(center)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalogs(id,name) VALUES($1,'Standard')")
        .bind(catalog)
        .execute(&pool)
        .await
        .unwrap();
    event(&pool, f.admin.user_id, None, "user", f.owner.user_id).await;
    event(&pool, f.admin.user_id, Some(f.team), "workspace", f.team).await;
    event(&pool, f.admin.user_id, None, "model", m).await;
    event(&pool, f.admin.user_id, None, "cost_center", center).await;
    event(&pool, f.admin.user_id, None, "catalog", catalog).await;
    let (status, rows) = call(
        &f.s,
        &f.auditor,
        "GET",
        "/api/v1/platform/audit",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    let admin_email = format!("{}", row(&rows, "user")["actor_name"]);
    assert!(
        admin_email.contains('@'),
        "actor named by email without a display name: {admin_email}"
    );
    assert_eq!(row(&rows, "user")["target_name"], "Owen Owner");
    assert_eq!(row(&rows, "workspace")["target_name"], "Team");
    assert_eq!(row(&rows, "workspace")["workspace_name"], "Team");
    assert_eq!(row(&rows, "workspace")["workspace_kind"], "team");
    assert_eq!(row(&rows, "model")["target_name"], "company/smart");
    assert_eq!(row(&rows, "cost_center")["target_name"], "Research");
    assert_eq!(row(&rows, "catalog")["target_name"], "Standard");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn workspace_audit_and_keys_name_people_only_where_visible(pool: PgPool) {
    let f = fixture(&pool).await;
    sqlx::query("UPDATE users SET display_name='Mia Member' WHERE id=$1")
        .bind(f.member.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let m = model(&pool, "company/smart").await;
    direct(&pool, f.team, m).await;
    let (status, key) = call(
        &f.s,
        &f.member,
        "POST",
        &format!("/api/v1/workspaces/{}/keys", f.team),
        json!({"name":"Mia laptop","expires_in_days":30}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{key}");
    let key_id: Uuid = key["id"].as_str().unwrap().parse().unwrap();
    event(&pool, f.member.user_id, Some(f.team), "key", key_id).await;
    event(&pool, f.member.user_id, Some(f.team), "model", m).await;
    // The admin sees who did it and the key's name.
    let audit = format!("/api/v1/workspaces/{}/audit", f.team);
    let (_, admin) = call(&f.s, &f.owner, "GET", &audit, Value::Null).await;
    assert_eq!(row(&admin, "key")["actor_name"], "Mia Member");
    assert_eq!(row(&admin, "key")["target_name"], "Mia laptop");
    assert_eq!(row(&admin, "model")["target_name"], "company/smart");
    // The member sees only their own events, named.
    let (_, member) = call(&f.s, &f.member, "GET", &audit, Value::Null).await;
    assert_eq!(row(&member, "key")["actor_name"], "Mia Member");
    // Key lists name the issuer for those who can see the key.
    let keys = format!("/api/v1/workspaces/{}/keys", f.team);
    let (_, list) = call(&f.s, &f.owner, "GET", &keys, Value::Null).await;
    assert_eq!(list["data"][0]["issued_to_name"], "Mia Member");
    let (_, detail) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("{keys}/{key_id}"),
        Value::Null,
    )
    .await;
    assert_eq!(detail["issued_to_name"], "Mia Member");
}

/// Platform Admins add members by searching too, never by loading every user.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_member_candidates_search_entitled_non_members(pool: PgPool) {
    let f = fixture(&pool).await;
    let path =
        |ws: Uuid, q: &str| format!("/api/v1/platform/workspaces/{ws}/member-candidates?q={q}");
    let (status, body) = call(
        &f.s,
        &f.admin,
        "GET",
        &path(f.team, "outsider"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let emails: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["email"].as_str().unwrap())
        .collect();
    assert_eq!(emails.len(), 1);
    assert!(emails[0].contains("outsider"));
    // Current members are not candidates.
    let (_, members) = call(&f.s, &f.admin, "GET", &path(f.team, "member"), Value::Null).await;
    assert_eq!(members["data"], json!([]));
    // Shared workspaces only; platform staff only.
    let (status, _) = call(
        &f.s,
        &f.admin,
        "GET",
        &path(f.personal, "outsider"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &f.s,
        &f.outsider,
        "GET",
        &path(f.team, "owner"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&f.s, &f.admin, "GET", &path(f.team, ""), Value::Null).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Names never reach people who couldn't see them before: personal owners don't
/// learn who on the platform staff changed their workspace, and former members'
/// names stay hidden. Cleaned identities stay labelled as retained.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn names_stay_within_previous_visibility(pool: PgPool) {
    let f = fixture(&pool).await;
    // A Platform Admin's change in someone's personal workspace.
    event(
        &pool,
        f.admin.user_id,
        Some(f.personal),
        "workspace",
        f.personal,
    )
    .await;
    let (_, personal) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("/api/v1/workspaces/{}/audit", f.personal),
        Value::Null,
    )
    .await;
    assert_eq!(row(&personal, "workspace")["actor_name"], Value::Null);
    // A former member's key and events in a team.
    let m = model(&pool, "company/smart").await;
    direct(&pool, f.team, m).await;
    let (_, key) = call(
        &f.s,
        &f.member,
        "POST",
        &format!("/api/v1/workspaces/{}/keys", f.team),
        json!({"name":"Old laptop","expires_in_days":30}),
    )
    .await;
    let key_id: Uuid = key["id"].as_str().unwrap().parse().unwrap();
    event(&pool, f.member.user_id, Some(f.team), "key", key_id).await;
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id=$2").bind(f.team).bind(f.member.user_id).execute(&pool).await.unwrap();
    let (_, team) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("/api/v1/workspaces/{}/audit", f.team),
        Value::Null,
    )
    .await;
    assert_eq!(row(&team, "key")["actor_name"], Value::Null);
    let (_, keys) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("/api/v1/workspaces/{}/keys", f.team),
        Value::Null,
    )
    .await;
    let held = keys["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == key["id"])
        .unwrap();
    assert_eq!(held["issued_to_name"], Value::Null);
    // A cleaned identity in the platform audit.
    sqlx::query("UPDATE users SET email=NULL,display_name=NULL WHERE id=$1")
        .bind(f.outsider.user_id)
        .execute(&pool)
        .await
        .unwrap();
    event(&pool, f.admin.user_id, None, "user", f.outsider.user_id).await;
    let (_, platform) = call(
        &f.s,
        &f.auditor,
        "GET",
        "/api/v1/platform/audit",
        Value::Null,
    )
    .await;
    assert_eq!(row(&platform, "user")["target_name"], "Retained identity");
}

/// Member lists search on the server, so the members table never loads a whole team.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn member_lists_search_on_the_server(pool: PgPool) {
    let f = fixture(&pool).await;
    for path in [
        format!("/api/v1/workspaces/{}/members", f.team),
        format!("/api/v1/platform/workspaces/{}/members", f.team),
    ] {
        let who = if path.contains("/platform/") {
            &f.auditor
        } else {
            &f.owner
        };
        let (status, all) = call(&f.s, who, "GET", &path, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{all}");
        assert_eq!(all["data"].as_array().unwrap().len(), 2);
        let (status, found) = call(
            &f.s,
            who,
            "GET",
            &format!("{path}?q=MEMBER&limit=10"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{found}");
        assert_eq!(found["data"].as_array().unwrap().len(), 1);
        assert_eq!(found["data"][0]["user_id"], json!(f.member.user_id));
        let (status, _) = call(
            &f.s,
            who,
            "GET",
            &format!("{path}?q={}", "x".repeat(201)),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}

/// The Team/Project "Initial owner" search asks only for entitled people.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn users_can_be_filtered_to_anyone_with_a_platform_role(pool: PgPool) {
    let f = fixture(&pool).await;
    let unentitled = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'owner-to-be@example.invalid')")
        .bind(unentitled)
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = call(
        &f.s,
        &f.admin,
        "GET",
        "/api/v1/platform/users?status=active&role=any&q=owner",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [f.owner.user_id.to_string()]);
}
