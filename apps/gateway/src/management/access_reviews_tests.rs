//! #90 slice 1: access review campaigns on real PostgreSQL. A Platform Admin
//! starts a one-shot campaign that snapshots the live shared memberships
//! and/or platform roles it covers; Platform Admins and Auditors read it.
//! Browser-only; the database derives every item from its grant.
use super::*;

const REVIEWS: &str = "/api/v1/platform/access-reviews";

async fn start(f: &Fixture, body: Value) -> (StatusCode, Value) {
    call(&f.s, &f.admin, "POST", REVIEWS, body).await
}
async fn items(f: &Fixture, campaign: &Value) -> Vec<Value> {
    let (status, page) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("{REVIEWS}/{}/items?limit=200", id(campaign)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    page["data"].as_array().unwrap().clone()
}
fn keyed(items: &[Value]) -> std::collections::BTreeSet<String> {
    items
        .iter()
        .map(|i| {
            format!(
                "{}:{}:{}:{}:{}",
                i["subject_kind"].as_str().unwrap(),
                i["workspace_name"].as_str().unwrap_or("-"),
                i["email"].as_str().unwrap(),
                i["role"].as_str().unwrap(),
                i["source"].as_str().unwrap()
            )
        })
        .collect()
}
async fn group_member(pool: &PgPool, ws: Uuid, user: Uuid) -> Uuid {
    sqlx::query_scalar("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source,mapping_id) VALUES($1,$2,'member','group',$3) RETURNING id")
        .bind(ws)
        .bind(user)
        .bind(Uuid::new_v4())
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_campaign_snapshots_live_shared_access(pool: PgPool) {
    let f = fixture(&pool).await;
    // Group provenance is reviewed too, shown distinctly.
    group_member(&pool, f.project, f.outsider.user_id).await;
    // Not reviewed: a revoked grant, a disabled workspace, a disabled user.
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source,revoked_at) VALUES($1,$2,'admin','manual',now())")
        .bind(f.project).bind(f.member.user_id).execute(&pool).await.unwrap();
    let off = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces(id,name,kind,disabled_at) VALUES($1,'Off','team',now())")
        .bind(off)
        .execute(&pool)
        .await
        .unwrap();
    member(&pool, off, f.member.user_id, "member").await;
    let gone = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,disabled_at) VALUES($1,'gone@example.test',now())")
        .bind(gone)
        .execute(&pool)
        .await
        .unwrap();
    member(&pool, f.team, gone, "member").await;

    let (status, c) = start(
        &f,
        json!({"name":"Q4 shared access","subjects":["workspace_memberships"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{c}");
    assert_eq!(c["name"], "Q4 shared access");
    assert_eq!(c["subjects"], json!(["workspace_memberships"]));
    assert_eq!(c["item_count"], 4);
    assert!(c["closed_at"].is_null());
    // Due in 14 days by default.
    let due: chrono::DateTime<chrono::Utc> = c["due_at"].as_str().unwrap().parse().unwrap();
    let created: chrono::DateTime<chrono::Utc> = c["created_at"].as_str().unwrap().parse().unwrap();
    assert_eq!((due - created).num_days(), 14);
    // Personal workspaces are never reviewed; Team and Project are.
    assert_eq!(
        keyed(&items(&f, &c).await),
        [
            "workspace_membership:Project:outsider@example.test:member:group",
            "workspace_membership:Project:owner@example.test:owner:manual",
            "workspace_membership:Team:member@example.test:member:manual",
            "workspace_membership:Team:owner@example.test:owner:manual",
        ]
        .map(String::from)
        .into()
    );
    // Platform roles, alone or with memberships; a workspace filter narrows memberships.
    let (status, roles) = start(
        &f,
        json!({"name":"Platform roles","subjects":["platform_roles"],"due_in_days":30}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{roles}");
    let got = keyed(&items(&f, &roles).await);
    assert_eq!(got.len(), 5, "{got:?}");
    assert!(got.contains("platform_role:-:admin@example.test:admin:manual"));
    assert!(got.contains("platform_role:-:auditor@example.test:auditor:manual"));
    let (status, narrowed) = start(&f, json!({"name":"Team only","subjects":["workspace_memberships","platform_roles"],"workspace_ids":[f.team]})).await;
    assert_eq!(status, StatusCode::OK, "{narrowed}");
    let got = keyed(&items(&f, &narrowed).await);
    assert_eq!(
        got.iter()
            .filter(|k| k.starts_with("workspace_membership:"))
            .count(),
        2,
        "{got:?}"
    );
    assert!(got.iter().all(|k| !k.contains(":Project:")));
    assert_eq!(
        got.iter()
            .filter(|k| k.starts_with("platform_role:"))
            .count(),
        5
    );

    // A snapshot: later changes do not add or remove items; the item shows
    // whether its grant is still active.
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id=$2")
        .bind(f.team).bind(f.member.user_id).execute(&pool).await.unwrap();
    member(&pool, f.team, f.auditor.user_id, "member").await;
    let after = items(&f, &c).await;
    assert_eq!(after.len(), 4);
    let revoked = after
        .iter()
        .find(|i| i["email"] == "member@example.test")
        .unwrap();
    assert_eq!(revoked["active"], false);
    assert!(
        after
            .iter()
            .filter(|i| i["email"] != "member@example.test")
            .all(|i| i["active"] == true)
    );

    // List and detail.
    let (status, list) = call(&f.s, &f.auditor, "GET", REVIEWS, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["data"].as_array().unwrap().len(), 3);
    let (status, detail) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("{REVIEWS}/{}", id(&c)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["item_count"], 4);
    assert_eq!(
        detail["counts"],
        json!({"workspace_membership":4,"platform_role":0,"manual":3,"group":1,"bootstrap":0,"inactive":1})
    );
    // Audited, with the item count only.
    let audit: (String, Value) = sqlx::query_as("SELECT resource_type,metadata FROM audit_events WHERE action='access_review.created' AND resource_id=$1")
        .bind(id(&c)).fetch_one(&pool).await.unwrap();
    assert_eq!(audit, ("access_review".to_owned(), json!({"count":4})));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn only_platform_admins_start_and_platform_readers_read(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, c) = start(&f, json!({"name":"Mine","subjects":["platform_roles"]})).await;
    assert_eq!(status, StatusCode::OK, "{c}");
    let body = json!({"name":"Nope","subjects":["platform_roles"]});
    for actor in [&f.auditor, &f.owner, &f.member, &f.outsider] {
        let (status, _) = call(&f.s, actor, "POST", REVIEWS, body.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{}", actor.email);
    }
    for path in [
        REVIEWS.to_owned(),
        format!("{REVIEWS}/{}", id(&c)),
        format!("{REVIEWS}/{}/items", id(&c)),
    ] {
        for actor in [&f.owner, &f.member, &f.outsider] {
            let (status, _) = call(&f.s, actor, "GET", &path, Value::Null).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{} {path}", actor.email);
        }
        let (status, _) = call(&f.s, &f.admin, "GET", &path, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }
    let unknown = format!("{REVIEWS}/{}", Uuid::new_v4());
    assert_eq!(
        call(&f.s, &f.auditor, "GET", &unknown, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &f.s,
            &f.auditor,
            "GET",
            &format!("{unknown}/items"),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // Every route is browser-only: admin tokens never reach them.
    for (method, path) in [
        ("GET", REVIEWS),
        ("POST", REVIEWS),
        ("GET", "/api/v1/platform/access-reviews/{id}"),
        ("GET", "/api/v1/platform/access-reviews/{id}/items"),
    ] {
        let token = crate::route_policy::token_policy(method, path)
            .unwrap_or_else(|| panic!("{method} {path} undeclared"));
        assert!(
            matches!(token, crate::route_policy::TokenPolicy::Denied),
            "{method} {path}"
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn campaign_bodies_and_bounds_are_strict(pool: PgPool) {
    let f = fixture(&pool).await;
    let other = Uuid::new_v4();
    for (label, body) in [
        (
            "empty name",
            json!({"name":" ","subjects":["platform_roles"]}),
        ),
        (
            "long name",
            json!({"name":"x".repeat(121),"subjects":["platform_roles"]}),
        ),
        ("no subjects", json!({"name":"A","subjects":[]})),
        (
            "unknown subject",
            json!({"name":"A","subjects":["api_keys"]}),
        ),
        (
            "repeated subject",
            json!({"name":"A","subjects":["platform_roles","platform_roles"]}),
        ),
        (
            "due 0",
            json!({"name":"A","subjects":["platform_roles"],"due_in_days":0}),
        ),
        (
            "due 91",
            json!({"name":"A","subjects":["platform_roles"],"due_in_days":91}),
        ),
        (
            "personal filter",
            json!({"name":"A","subjects":["workspace_memberships"],"workspace_ids":[f.personal]}),
        ),
        (
            "unknown filter",
            json!({"name":"A","subjects":["workspace_memberships"],"workspace_ids":[other]}),
        ),
        (
            "repeated filter",
            json!({"name":"A","subjects":["workspace_memberships"],"workspace_ids":[f.team,f.team]}),
        ),
        (
            "filter without memberships",
            json!({"name":"A","subjects":["platform_roles"],"workspace_ids":[f.team]}),
        ),
        (
            "too many filters",
            json!({"name":"A","subjects":["workspace_memberships"],"workspace_ids":vec![f.team;51]}),
        ),
    ] {
        let (status, _) = start(&f, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label}");
    }
    // Unknown fields are refused by the JSON extractor, as on every strict body.
    let (status, _) = start(
        &f,
        json!({"name":"A","subjects":["platform_roles"],"reviewers":[]}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "unknown field");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM access_review_campaigns")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    // At most 20 open campaigns.
    for n in 0..20 {
        let (status, c) = start(
            &f,
            json!({"name":format!("C{n}"),"subjects":["platform_roles"]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{c}");
    }
    let (status, body) = start(&f, json!({"name":"C21","subjects":["platform_roles"]})).await;
    assert_eq!(
        (status, body["error"]["reason"].as_str()),
        (StatusCode::CONFLICT, Some("access_review_limit"))
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_campaign_holds_at_most_5000_items(pool: PgPool) {
    let f = fixture(&pool).await;
    // 4,999 more members of the Team: 5,001 shared memberships in all.
    sqlx::query("WITH u AS (INSERT INTO users(id,email) SELECT gen_random_uuid(),'bulk'||n||'@example.test' FROM generate_series(1,4999) n RETURNING id) INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) SELECT $1,id,'member','manual' FROM u")
        .bind(f.team).execute(&pool).await.unwrap();
    let (status, body) = start(
        &f,
        json!({"name":"All","subjects":["workspace_memberships"]}),
    )
    .await;
    assert_eq!(
        (status, body["error"]["reason"].as_str()),
        (StatusCode::CONFLICT, Some("access_review_too_large")),
        "{body}"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM access_review_items")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    // Narrowed to the Project it fits.
    let (status, c) = start(
        &f,
        json!({"name":"Project","subjects":["workspace_memberships"],"workspace_ids":[f.project]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{c}");
    assert_eq!(c["item_count"], 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn the_database_derives_and_binds_every_item(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, c) = start(
        &f,
        json!({"name":"Bound","subjects":["workspace_memberships"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{c}");
    let campaign = id(&c);
    let grant: Uuid = sqlx::query_scalar(
        "SELECT id FROM workspace_membership_grants WHERE workspace_id=$1 AND user_id=$2",
    )
    .bind(f.team)
    .bind(f.member.user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    // Items are added only in the transaction that creates the campaign.
    let late = sqlx::query("INSERT INTO access_review_items(id,campaign_id,subject_kind,membership_grant_id) VALUES($1,$2,'workspace_membership',$3)")
        .bind(Uuid::new_v4()).bind(campaign).bind(grant).execute(&pool).await;
    assert!(late.is_err(), "late item accepted");
    let mut tx = pool.begin().await.unwrap();
    let fresh = Uuid::new_v4();
    sqlx::query("INSERT INTO access_review_campaigns(id,name,subjects,workspace_ids,created_by,due_at) VALUES($1,'Direct','{workspace_memberships}','{}',$2,now()+interval '7 days')")
        .bind(fresh).bind(f.admin.user_id).execute(&mut *tx).await.unwrap();
    // Forged fields are replaced by the grant's own.
    sqlx::query("INSERT INTO access_review_items(id,campaign_id,subject_kind,membership_grant_id,workspace_id,user_id,role,source) VALUES($1,$2,'workspace_membership',$3,$4,$5,'owner','group')")
        .bind(Uuid::new_v4()).bind(fresh).bind(grant).bind(f.project).bind(f.outsider.user_id).execute(&mut *tx).await.unwrap();
    let derived: (Uuid, Uuid, String, String) = sqlx::query_as(
        "SELECT workspace_id,user_id,role,source FROM access_review_items WHERE campaign_id=$1",
    )
    .bind(fresh)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(
        derived,
        (
            f.team,
            f.member.user_id,
            "member".to_owned(),
            "manual".to_owned()
        )
    );
    // A subject the campaign does not cover, a personal workspace's grant.
    let platform: Uuid = sqlx::query_scalar("SELECT id FROM platform_role_grants WHERE user_id=$1")
        .bind(f.member.user_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let personal: Uuid =
        sqlx::query_scalar("SELECT id FROM workspace_membership_grants WHERE workspace_id=$1")
            .bind(f.personal)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    for (label, sql, target) in [
        (
            "uncovered subject",
            "INSERT INTO access_review_items(id,campaign_id,subject_kind,platform_grant_id) VALUES($1,$2,'platform_role',$3)",
            platform,
        ),
        (
            "personal workspace",
            "INSERT INTO access_review_items(id,campaign_id,subject_kind,membership_grant_id) VALUES($1,$2,'workspace_membership',$3)",
            personal,
        ),
    ] {
        sqlx::query("SAVEPOINT s").execute(&mut *tx).await.unwrap();
        let r = sqlx::query(sql)
            .bind(Uuid::new_v4())
            .bind(fresh)
            .bind(target)
            .execute(&mut *tx)
            .await;
        assert!(r.is_err(), "{label} accepted");
        sqlx::query("ROLLBACK TO SAVEPOINT s")
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.rollback().await.unwrap();
    // Only a Platform Admin starts one; due 1 to 90 days out; records never change.
    for (label, sql, who) in [
        (
            "non-admin",
            "INSERT INTO access_review_campaigns(id,name,subjects,workspace_ids,created_by,due_at) VALUES($1,'X','{platform_roles}','{}',$2,now()+interval '7 days')",
            f.auditor.user_id,
        ),
        (
            "due too late",
            "INSERT INTO access_review_campaigns(id,name,subjects,workspace_ids,created_by,due_at) VALUES($1,'X','{platform_roles}','{}',$2,now()+interval '91 days')",
            f.admin.user_id,
        ),
    ] {
        let r = sqlx::query(sql)
            .bind(Uuid::new_v4())
            .bind(who)
            .execute(&pool)
            .await;
        assert!(r.is_err(), "{label} accepted");
    }
    for sql in [
        "UPDATE access_review_campaigns SET name='Renamed'",
        "UPDATE access_review_items SET role='owner'",
        "DELETE FROM access_review_items",
        "DELETE FROM access_review_campaigns",
    ] {
        assert!(sqlx::query(sql).execute(&pool).await.is_err(), "{sql}");
    }
}
