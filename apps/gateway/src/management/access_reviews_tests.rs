//! #90 slice 1: access review campaigns on real PostgreSQL. A Platform Admin
//! starts a one-shot campaign that snapshots the live shared memberships
//! and/or elevated platform roles it covers, and closes it; Platform Admins
//! and Auditors read it. Browser-only; the database derives every item from
//! its grant.
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
fn reason(body: &Value) -> Option<&str> {
    body["error"]["reason"].as_str()
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
async fn team(pool: &PgPool, name: &str) -> Uuid {
    let ws = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,$2,'team')")
        .bind(ws)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    ws
}
/// The SQLSTATE of a refused statement (`None` if it was accepted).
fn sqlstate(r: Result<sqlx::postgres::PgQueryResult, sqlx::Error>) -> Option<String> {
    r.err().map(|e| {
        e.as_database_error()
            .and_then(|d| d.code())
            .map(|c| c.into_owned())
            .unwrap_or_else(|| format!("{e}"))
    })
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_campaign_snapshots_live_shared_access(pool: PgPool) {
    let f = fixture(&pool).await;
    // Group provenance is reviewed too, shown distinctly (memberships and platform roles).
    group_member(&pool, f.project, f.outsider.user_id).await;
    sqlx::query("INSERT INTO platform_role_grants(user_id,role,source,mapping_id) VALUES($1,'auditor','group',$2)")
        .bind(f.outsider.user_id).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    // Not reviewed: a revoked grant, a disabled workspace, a disabled user, a cleaned-up user.
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source,revoked_at) VALUES($1,$2,'admin','manual',now())")
        .bind(f.project).bind(f.member.user_id).execute(&pool).await.unwrap();
    let off = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces(id,name,kind,disabled_at) VALUES($1,'Off','team',now())")
        .bind(off)
        .execute(&pool)
        .await
        .unwrap();
    member(&pool, off, f.member.user_id, "member").await;
    for (email, column) in [
        ("gone@example.test", "disabled_at"),
        ("cleaned@example.test", "cleaned_at"),
    ] {
        let u = Uuid::new_v4();
        sqlx::query(&format!(
            "INSERT INTO users(id,email,{column}) VALUES($1,$2,now())"
        ))
        .bind(u)
        .bind(email)
        .execute(&pool)
        .await
        .unwrap();
        member(&pool, f.team, u, "member").await;
        sqlx::query(
            "INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'admin','manual')",
        )
        .bind(u)
        .execute(&pool)
        .await
        .unwrap();
    }

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
    // Platform roles: the elevated grants (Admin, Auditor; manual or group),
    // never the User entitlement every person holds.
    let (status, roles) = start(
        &f,
        json!({"name":"Platform roles","subjects":["platform_roles"],"due_in_days":30}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{roles}");
    assert_eq!(
        keyed(&items(&f, &roles).await),
        [
            "platform_role:-:admin@example.test:admin:manual",
            "platform_role:-:auditor@example.test:auditor:manual",
            "platform_role:-:outsider@example.test:auditor:group",
        ]
        .map(String::from)
        .into()
    );
    let (status, narrowed) = start(&f, json!({"name":"Team only","subjects":["workspace_memberships","platform_roles"],"workspace_ids":[f.team]})).await;
    assert_eq!(status, StatusCode::OK, "{narrowed}");
    assert_eq!(
        narrowed["subjects"],
        json!(["platform_roles", "workspace_memberships"]),
        "stored sorted"
    );
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
        3
    );

    // A snapshot: later changes do not add or remove items; an item shows
    // whether its grant is still in effect (revoked, or its person disabled).
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id=$2")
        .bind(f.team).bind(f.member.user_id).execute(&pool).await.unwrap();
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(f.outsider.user_id)
        .execute(&pool)
        .await
        .unwrap();
    member(&pool, f.team, f.auditor.user_id, "member").await;
    let after = items(&f, &c).await;
    assert_eq!(after.len(), 4);
    let inactive: std::collections::BTreeSet<&str> = after
        .iter()
        .filter(|i| i["active"] == false)
        .map(|i| i["email"].as_str().unwrap())
        .collect();
    assert_eq!(
        inactive,
        ["member@example.test", "outsider@example.test"].into()
    );
    assert!(after.iter().all(|i| i["active"].is_boolean()));

    // Paging: one item per page covers every item once.
    let mut paged = std::collections::BTreeSet::new();
    for offset in 0..5 {
        let (status, page) = call(
            &f.s,
            &f.auditor,
            "GET",
            &format!("{REVIEWS}/{}/items?limit=1&offset={offset}", id(&c)),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        for i in page["data"].as_array().unwrap() {
            assert!(
                paged.insert(i["id"].as_str().unwrap().to_owned()),
                "an item on two pages"
            );
        }
    }
    assert_eq!(paged.len(), 4);

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
        json!({"by_kind":{"workspace_membership":4,"platform_role":0},"by_source":{"manual":3,"group":1,"bootstrap":0},"inactive":2})
    );
    // Audited, with the item count only.
    let audit: (String, Value) = sqlx::query_as("SELECT resource_type,metadata FROM audit_events WHERE action='access_review.created' AND resource_id=$1")
        .bind(id(&c)).fetch_one(&pool).await.unwrap();
    assert_eq!(audit, ("access_review".to_owned(), json!({"count":4})));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn only_platform_admins_start_and_close_and_platform_readers_read(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, c) = start(&f, json!({"name":"Mine","subjects":["platform_roles"]})).await;
    assert_eq!(status, StatusCode::OK, "{c}");
    let body = json!({"name":"Nope","subjects":["platform_roles"]});
    let close = format!("{REVIEWS}/{}/close", id(&c));
    for actor in [&f.auditor, &f.owner, &f.member, &f.outsider] {
        let (status, _) = call(&f.s, actor, "POST", REVIEWS, body.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{}", actor.email);
        let (status, _) = call(&f.s, actor, "POST", &close, Value::Null).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "close {}", actor.email);
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
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            &format!("{unknown}/close"),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // Closing happens once, audited.
    let (status, closed) = call(&f.s, &f.admin, "POST", &close, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    assert!(closed["closed_at"].is_string());
    let (status, again) = call(&f.s, &f.admin, "POST", &close, Value::Null).await;
    assert_eq!(
        (status, reason(&again)),
        (StatusCode::CONFLICT, Some("access_review_closed"))
    );
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='access_review.closed' AND resource_id=$1",
    )
    .bind(id(&c))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audited, 1);
    // Every route is browser-only: admin tokens never reach them.
    for (method, path) in [
        ("GET", REVIEWS),
        ("POST", REVIEWS),
        ("GET", "/api/v1/platform/access-reviews/{id}"),
        ("GET", "/api/v1/platform/access-reviews/{id}/items"),
        ("POST", "/api/v1/platform/access-reviews/{id}/close"),
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
    let mut fifty = vec![f.team];
    for n in 0..50 {
        fifty.push(team(&pool, &format!("Filter {n}")).await);
    }
    for (label, body) in [
        (
            "empty name",
            json!({"name":" ","subjects":["platform_roles"]}),
        ),
        (
            "long name",
            json!({"name":"x".repeat(121),"subjects":["platform_roles"]}),
        ),
        (
            "control character",
            json!({"name":"a\u{7}b","subjects":["platform_roles"]}),
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
            "51 workspaces",
            json!({"name":"A","subjects":["workspace_memberships"],"workspace_ids":fifty}),
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
    // 50 workspaces are accepted.
    let (status, c) = start(
        &f,
        json!({"name":"Fifty","subjects":["workspace_memberships"],"workspace_ids":&fifty[..50]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{c}");
    // Nothing to review (empty or disabled Teams): refused, no campaign.
    let empty = team(&pool, "Empty").await;
    let off = team(&pool, "Disabled").await;
    member(&pool, off, f.member.user_id, "member").await;
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1")
        .bind(off)
        .execute(&pool)
        .await
        .unwrap();
    for ws in [empty, off] {
        let (status, body) = start(
            &f,
            json!({"name":"Empty","subjects":["workspace_memberships"],"workspace_ids":[ws]}),
        )
        .await;
        assert_eq!(
            (status, reason(&body)),
            (StatusCode::CONFLICT, Some("access_review_empty")),
            "{body}"
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM access_review_campaigns")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    // At most 20 open campaigns; closing one frees a place.
    for n in 1..20 {
        let (status, c) = start(
            &f,
            json!({"name":format!("C{n}"),"subjects":["platform_roles"]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{c}");
    }
    let (status, body) = start(&f, json!({"name":"C21","subjects":["platform_roles"]})).await;
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::CONFLICT, Some("access_review_limit"))
    );
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{REVIEWS}/{}/close", id(&c)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = start(&f, json!({"name":"C21","subjects":["platform_roles"]})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_starts_never_exceed_twenty_open(pool: PgPool) {
    let f = fixture(&pool).await;
    for n in 0..19 {
        let (status, c) = start(
            &f,
            json!({"name":format!("C{n}"),"subjects":["platform_roles"]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{c}");
    }
    let body = json!({"name":"Race","subjects":["platform_roles"]});
    let (a, b) = tokio::join!(start(&f, body.clone()), start(&f, body));
    let mut statuses = [a.0, b.0];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::OK, StatusCode::CONFLICT],
        "{} {}",
        a.1,
        b.1
    );
    let open: i64 =
        sqlx::query_scalar("SELECT count(*) FROM access_review_campaigns WHERE closed_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(open, 20);
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
        (status, reason(&body)),
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
async fn access_changing_during_the_snapshot_refuses_the_campaign(pool: PgPool) {
    let f = fixture(&pool).await;
    // A real race: the snapshot's first item waits on an advisory lock this
    // test holds; meanwhile another transaction disables the Team member
    // (taking its scope lock first, as every authority change must) and
    // commits; then the snapshot continues and the guard, reading in its own
    // snapshot, finds the member disabled.
    sqlx::query("CREATE FUNCTION test_wait_first() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock_shared(90901); RETURN NEW; END $$")
        .execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER aaa_test_wait_first BEFORE INSERT ON access_review_items FOR EACH ROW EXECUTE FUNCTION test_wait_first()")
        .execute(&pool).await.unwrap();
    let mut hold = pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock(90901)")
        .execute(&mut *hold)
        .await
        .unwrap();
    let member = f.member.user_id;
    let race = async {
        loop {
            let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_locks WHERE locktype='advisory' AND objid=90901 AND NOT granted")
                .fetch_one(&pool).await.unwrap();
            if waiting > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT omg_scope_lock_exclusive(72419512, omg_scope_key($1))")
            .bind(member)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
            .bind(member)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        sqlx::query("SELECT pg_advisory_unlock(90901)")
            .execute(&mut *hold)
            .await
            .unwrap();
    };
    let ((status, body), ()) = tokio::join!(
        start(
            &f,
            json!({"name":"Race","subjects":["workspace_memberships"]})
        ),
        race
    );
    assert_eq!(
        (status, reason(&body)),
        (StatusCode::CONFLICT, Some("access_review_changed")),
        "{body}"
    );
    // Nothing was recorded.
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM access_review_campaigns")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM access_review_items")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
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
    let item = "INSERT INTO access_review_items(id,campaign_id,subject_kind,membership_grant_id) VALUES($1,$2,'workspace_membership',$3)";
    // Items are added only in the transaction that creates the campaign.
    let late = sqlx::query(item)
        .bind(Uuid::new_v4())
        .bind(campaign)
        .bind(grant)
        .execute(&pool)
        .await;
    assert_eq!(sqlstate(late).as_deref(), Some("23514"), "late item");
    let mut tx = pool.begin().await.unwrap();
    let fresh = Uuid::new_v4();
    sqlx::query("INSERT INTO access_review_campaigns(id,name,subjects,workspace_ids,created_by,due_at) VALUES($1,'Direct','{workspace_memberships}',$3,$2,now()+interval '7 days')")
        .bind(fresh).bind(f.admin.user_id).bind(vec![f.team]).execute(&mut *tx).await.unwrap();
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
    // A subject the campaign does not cover, a personal workspace's grant, a
    // grant outside the filter, the User entitlement.
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
    let project: Uuid =
        sqlx::query_scalar("SELECT id FROM workspace_membership_grants WHERE workspace_id=$1")
            .bind(f.project)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    for (label, sql, target) in [
        (
            "uncovered subject",
            "INSERT INTO access_review_items(id,campaign_id,subject_kind,platform_grant_id) VALUES($1,$2,'platform_role',$3)",
            platform,
        ),
        ("personal workspace", item, personal),
        ("outside the filter", item, project),
    ] {
        sqlx::query("SAVEPOINT s").execute(&mut *tx).await.unwrap();
        let r = sqlx::query(sql)
            .bind(Uuid::new_v4())
            .bind(fresh)
            .bind(target)
            .execute(&mut *tx)
            .await;
        assert_eq!(sqlstate(r).as_deref(), Some("23514"), "{label}");
        sqlx::query("ROLLBACK TO SAVEPOINT s")
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    // A closed campaign takes no item, even in its own transaction.
    sqlx::query("UPDATE access_review_campaigns SET closed_by=$2, closed_at=now() WHERE id=$1")
        .bind(fresh)
        .bind(f.admin.user_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    let r = sqlx::query(item)
        .bind(Uuid::new_v4())
        .bind(fresh)
        .bind(grant)
        .execute(&mut *tx)
        .await;
    assert_eq!(sqlstate(r).as_deref(), Some("23514"), "item after close");
    tx.rollback().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let roles = Uuid::new_v4();
    sqlx::query("INSERT INTO access_review_campaigns(id,name,subjects,workspace_ids,created_by,due_at) VALUES($1,'Roles','{platform_roles}','{}',$2,now()+interval '7 days')")
        .bind(roles).bind(f.admin.user_id).execute(&mut *tx).await.unwrap();
    let r = sqlx::query("INSERT INTO access_review_items(id,campaign_id,subject_kind,platform_grant_id) VALUES($1,$2,'platform_role',$3)").bind(Uuid::new_v4()).bind(roles).bind(platform).execute(&mut *tx).await;
    assert_eq!(
        sqlstate(r).as_deref(),
        Some("23514"),
        "the User entitlement"
    );
    tx.rollback().await.unwrap();
    // Only a Platform Admin starts one; due 1 to 90 days out; a non-blank name.
    for (label, name, days, who) in [
        ("non-admin", "X", 7, f.auditor.user_id),
        ("due too late", "X", 91, f.admin.user_id),
        ("blank name", " ", 7, f.admin.user_id),
        ("control character", "a\u{1}b", 7, f.admin.user_id),
    ] {
        let r = sqlx::query("INSERT INTO access_review_campaigns(id,name,subjects,workspace_ids,created_by,due_at) VALUES($1,$2,'{platform_roles}','{}',$3,now()+make_interval(days=>$4))")
            .bind(Uuid::new_v4()).bind(name).bind(who).bind(days).execute(&pool).await;
        assert_eq!(sqlstate(r).as_deref(), Some("23514"), "{label}");
    }
    // Closing: once, by a current Platform Admin; nothing else changes.
    let close = "UPDATE access_review_campaigns SET closed_by=$2, closed_at=now() WHERE id=$1";
    assert_eq!(
        sqlstate(
            sqlx::query(close)
                .bind(campaign)
                .bind(f.auditor.user_id)
                .execute(&pool)
                .await
        )
        .as_deref(),
        Some("23514"),
        "closed by a non-admin"
    );
    sqlx::query(close)
        .bind(campaign)
        .bind(f.admin.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlstate(
            sqlx::query(close)
                .bind(campaign)
                .bind(f.admin.user_id)
                .execute(&pool)
                .await
        )
        .as_deref(),
        Some("23514"),
        "closed twice"
    );
    for (sql, code) in [
        ("UPDATE access_review_campaigns SET name='Renamed'", "23514"),
        ("UPDATE access_review_items SET role='owner'", "P0001"),
        ("DELETE FROM access_review_items", "P0001"),
        ("DELETE FROM access_review_campaigns", "P0001"),
    ] {
        assert_eq!(
            sqlstate(sqlx::query(sql).execute(&pool).await).as_deref(),
            Some(code),
            "{sql}"
        );
    }
}
