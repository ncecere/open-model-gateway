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
