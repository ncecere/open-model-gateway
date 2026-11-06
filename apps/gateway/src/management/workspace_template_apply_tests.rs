//! #84 slice B: applying a published onboarding template creates one shared
//! workspace in one transaction, within the applying Platform Admin's own
//! authority; any failure leaves nothing behind. Real PostgreSQL.
use super::*;

const TEMPLATES: &str = "/api/v1/platform/workspace-templates";

struct Published {
    id: String,
    catalog: Uuid,
    model: Uuid,
}
/// A published template: replace catalogs with one catalog holding one
/// model, local limits, an owner, an admin slot and a required member slot.
async fn published(f: &Fixture, pool: &PgPool, limits: Value) -> Published {
    let reviewer = user(pool, &format!("reviewer-{}", Uuid::new_v4()), "admin").await;
    let catalog = Uuid::new_v4();
    sqlx::query("INSERT INTO catalogs(id,name) VALUES($1,'Template apply')")
        .bind(catalog)
        .execute(pool)
        .await
        .unwrap();
    let m = model(pool, &format!("company/apply-{}", Uuid::new_v4())).await;
    sqlx::query("INSERT INTO catalog_models(catalog_id,model_id) VALUES($1,$2)")
        .bind(catalog)
        .bind(m)
        .execute(pool)
        .await
        .unwrap();
    let content = json!({
        "kind": "team",
        "catalogs": {"mode": "replace", "catalog_ids": [catalog]},
        "model_ids": [m],
        "limits": limits,
        "owner": {"name": "lead"},
        "slots": [
            {"name": "admins", "role": "admin", "required": false, "multiple": true},
            {"name": "crew", "role": "member", "required": true, "multiple": false}
        ]
    });
    let (status, t) = call(
        &f.s,
        &f.admin,
        "POST",
        TEMPLATES,
        json!({"name":format!("Apply {}", Uuid::new_v4()),"content":content}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{t}");
    let id = t["id"].as_str().unwrap().to_owned();
    assert_eq!(
        call(
            &f.s,
            &reviewer,
            "POST",
            &format!("{TEMPLATES}/{id}/versions/1/review"),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            &format!("{TEMPLATES}/{id}/versions/1/publish"),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    Published {
        id,
        catalog,
        model: m,
    }
}
fn limits() -> Value {
    json!({"requests_per_minute":60,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[{"period":"month","amount_microusd":"1000000"}]})
}
async fn apply(
    f: &Fixture,
    actor: &BrowserPrincipal,
    t: &Published,
    body: Value,
) -> (StatusCode, Value) {
    call(
        &f.s,
        actor,
        "POST",
        &format!("{TEMPLATES}/{}/apply", t.id),
        body,
    )
    .await
}
async fn workspaces(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM workspaces")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn applying_a_published_template_creates_the_whole_workspace(pool: PgPool) {
    let f = fixture(&pool).await;
    let t = published(&f, &pool, limits()).await;
    let lead = user(&pool, "apply-lead", "user").await;
    let helper = user(&pool, "apply-helper", "user").await;
    let body = json!({"version":1,"name":"Applied team","assignments":[{"slot":"lead","user_ids":[lead.user_id]},{"slot":"admins","user_ids":[helper.user_id]},{"slot":"crew","user_ids":[f.member.user_id]}],"idempotency_key":"apply-1"});
    let (status, out) = apply(&f, &f.admin, &t, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let ws: Uuid = out["workspace_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(out["version"], 1);
    // The workspace, its members (manual grants only), catalogs, models, limits.
    let (name, kind): (String, String) =
        sqlx::query_as("SELECT name,kind FROM workspaces WHERE id=$1")
            .bind(ws)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((name.as_str(), kind.as_str()), ("Applied team", "team"));
    let members: Vec<(Uuid, String, String)> = sqlx::query_as("SELECT user_id,role,source FROM workspace_membership_grants WHERE workspace_id=$1 AND revoked_at IS NULL ORDER BY role").bind(ws).fetch_all(&pool).await.unwrap();
    assert_eq!(
        members,
        vec![
            (helper.user_id, "admin".into(), "manual".into()),
            (f.member.user_id, "member".into(), "manual".into()),
            (lead.user_id, "owner".into(), "manual".into())
        ]
    );
    let catalogs: Vec<Uuid> = sqlx::query_scalar(
        "SELECT catalog_id FROM workspace_catalog_override_items WHERE workspace_id=$1",
    )
    .bind(ws)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(catalogs, vec![t.catalog]);
    let selected: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_model_grants WHERE workspace_id=$1 AND model_id=$2 AND source='catalog')").bind(ws).bind(t.model).fetch_one(&pool).await.unwrap();
    assert!(selected);
    let rpm: Option<i64> = sqlx::query_scalar(
        "SELECT requests_per_minute FROM workspace_local_policies WHERE workspace_id=$1",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rpm, Some(60));
    // The immutable application record, and the audit trail.
    let (version, hash_ok): (i32, bool) = sqlx::query_as("SELECT a.version,a.content_hash=v.content_hash FROM workspace_template_applications a JOIN workspace_template_versions v ON (v.template_id,v.version)=(a.template_id,a.version) WHERE a.workspace_id=$1").bind(ws).fetch_one(&pool).await.unwrap();
    assert_eq!((version, hash_ok), (1, true));
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='workspace_template.applied' AND workspace_id=$1").bind(ws).fetch_one(&pool).await.unwrap();
    assert_eq!(applied, 1);
    // No platform role changed for anyone assigned.
    let roles: Vec<String> = sqlx::query_scalar("SELECT role FROM platform_role_grants WHERE user_id=ANY($1) AND revoked_at IS NULL ORDER BY role").bind(vec![lead.user_id, helper.user_id]).fetch_all(&pool).await.unwrap();
    assert_eq!(roles, vec!["user", "user"]);
    // A replay returns the same workspace; the key with another payload is 409.
    let (status, again) = apply(&f, &f.admin, &t, body.clone()).await;
    assert_eq!(
        (status, again["workspace_id"].clone()),
        (StatusCode::OK, out["workspace_id"].clone())
    );
    let mut other = body.clone();
    other["name"] = json!("Another team");
    assert_eq!(apply(&f, &f.admin, &t, other).await.0, StatusCode::CONFLICT);
    assert_eq!(workspaces(&pool).await, 4);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_template_that_no_longer_fits_changes_nothing(pool: PgPool) {
    let f = fixture(&pool).await;
    let t = published(&f, &pool, limits()).await;
    let lead = user(&pool, "fit-lead", "user").await;
    let base = json!({"version":1,"name":"Fit team","assignments":[{"slot":"lead","user_ids":[lead.user_id]},{"slot":"crew","user_ids":[f.member.user_id]}],"idempotency_key":"fit"});
    let before = workspaces(&pool).await;
    let with = |path: &str, value: Value, key: &str| {
        let mut b = base.clone();
        *b.pointer_mut(path).unwrap() = value;
        b["idempotency_key"] = json!(key);
        b
    };
    // Assignments that do not match the slots: 400, nothing created.
    for (n, bad) in [
        with(
            "/assignments",
            json!([{"slot":"crew","user_ids":[f.member.user_id]}]),
            "no-owner",
        ),
        with(
            "/assignments",
            json!([{"slot":"lead","user_ids":[lead.user_id, f.owner.user_id]},{"slot":"crew","user_ids":[f.member.user_id]}]),
            "two-owners",
        ),
        with(
            "/assignments",
            json!([{"slot":"lead","user_ids":[lead.user_id]}]),
            "required-missing",
        ),
        with(
            "/assignments",
            json!([{"slot":"lead","user_ids":[lead.user_id]},{"slot":"crew","user_ids":[f.member.user_id, f.owner.user_id]}]),
            "single-twice",
        ),
        with(
            "/assignments",
            json!([{"slot":"lead","user_ids":[lead.user_id]},{"slot":"crew","user_ids":[f.member.user_id]},{"slot":"guests","user_ids":[f.owner.user_id]}]),
            "unknown-slot",
        ),
        with(
            "/assignments",
            json!([{"slot":"lead","user_ids":[lead.user_id]},{"slot":"crew","user_ids":[lead.user_id]}]),
            "same-user",
        ),
        with("/name", json!(""), "blank-name"),
    ]
    .into_iter()
    .enumerate()
    {
        let (status, body) = apply(&f, &f.admin, &t, bad).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{n}: {body}");
    }
    // A stale or unpublished version: 409.
    let (status, body) = apply(&f, &f.admin, &t, with("/version", json!(2), "stale")).await;
    assert_eq!(
        (status, body["error"]["reason"].clone()),
        (StatusCode::CONFLICT, json!("template_version_stale")),
        "{body}"
    );
    // An unentitled or disabled user: 409.
    let stranger = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'stranger@example.test')")
        .bind(stranger)
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = apply(
        &f,
        &f.admin,
        &t,
        with(
            "/assignments",
            json!([{"slot":"lead","user_ids":[stranger]},{"slot":"crew","user_ids":[f.member.user_id]}]),
            "stranger",
        ),
    )
    .await;
    assert_eq!(
        (status, body["error"]["reason"].clone()),
        (
            StatusCode::CONFLICT,
            json!("template_assignment_ineligible")
        ),
        "{body}"
    );
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(lead.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, _) = apply(
        &f,
        &f.admin,
        &t,
        with("/name", json!("Disabled lead"), "disabled"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    sqlx::query("UPDATE users SET disabled_at=NULL WHERE id=$1")
        .bind(lead.user_id)
        .execute(&pool)
        .await
        .unwrap();
    // The model left its catalog: 409, nothing created.
    sqlx::query("DELETE FROM catalog_models WHERE catalog_id=$1")
        .bind(t.catalog)
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = apply(
        &f,
        &f.admin,
        &t,
        with("/name", json!("No model"), "no-model"),
    )
    .await;
    assert_eq!(
        (status, body["error"]["reason"].clone()),
        (StatusCode::CONFLICT, json!("template_model_unavailable")),
        "{body}"
    );
    sqlx::query("INSERT INTO catalog_models(catalog_id,model_id) VALUES($1,$2)")
        .bind(t.catalog)
        .bind(t.model)
        .execute(&pool)
        .await
        .unwrap();
    // Limits above the live platform layer (a tighter Team default): 409.
    let (status, body) = call(&f.s, &f.admin, "PUT", "/api/v1/platform/workspace-types/team/policy", json!({"requests_per_minute":30,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[]})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = apply(&f, &f.admin, &t, with("/name", json!("Too loose"), "loose")).await;
    assert_eq!(
        (status, body["error"]["reason"].clone()),
        (
            StatusCode::CONFLICT,
            json!("template_limits_exceed_platform")
        ),
        "{body}"
    );
    // Nothing was created by any refusal, and no application recorded.
    assert_eq!(workspaces(&pool).await, before);
    let applications: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workspace_template_applications")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(applications, 0);
    // Retired: no longer applicable.
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            &format!("{TEMPLATES}/{}/versions/1/retire", t.id),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, body) = apply(&f, &f.admin, &t, with("/name", json!("Retired"), "retired")).await;
    assert_eq!(
        (status, body["error"]["reason"].clone()),
        (StatusCode::CONFLICT, json!("template_version_stale"))
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn only_platform_admins_apply_and_records_are_immutable(pool: PgPool) {
    let f = fixture(&pool).await;
    let t = published(&f, &pool, Value::Null).await;
    let body = json!({"version":1,"name":"Admins only","assignments":[{"slot":"lead","user_ids":[f.owner.user_id]},{"slot":"crew","user_ids":[f.member.user_id]}],"idempotency_key":"only"});
    for actor in [&f.auditor, &f.owner, &f.member, &f.outsider] {
        assert_eq!(
            apply(&f, actor, &t, body.clone()).await.0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            &format!("{TEMPLATES}/{}/apply", Uuid::new_v4()),
            body.clone()
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (status, out) = apply(&f, &f.admin, &t, body).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    // Without limits, no local policy row is written.
    let ws: Uuid = out["workspace_id"].as_str().unwrap().parse().unwrap();
    let local: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workspace_local_policies WHERE workspace_id=$1")
            .bind(ws)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(local, 0);
    for sql in [
        "UPDATE workspace_template_applications SET version=2",
        "DELETE FROM workspace_template_applications",
        // An application of an unpublished version or to a personal workspace.
        "INSERT INTO workspace_template_applications(id,template_id,version,content_hash,workspace_id,applied_by,idempotency_digest,payload_digest) SELECT gen_random_uuid(),template_id,version,content_hash,(SELECT id FROM workspaces WHERE kind='personal' LIMIT 1),applied_by,sha256('x'::bytea),sha256('y'::bytea) FROM workspace_template_applications LIMIT 1",
    ] {
        assert!(
            sqlx::query(sql).execute(&pool).await.is_err(),
            "accepted {sql}"
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn replays_and_ineligible_people_are_reported_precisely(pool: PgPool) {
    let f = fixture(&pool).await;
    let t = published(&f, &pool, Value::Null).await;
    let lead = user(&pool, "precise-lead", "user").await;
    let helper = user(&pool, "precise-helper", "user").await;
    let body = json!({"version":1,"name":"Precise","assignments":[{"slot":"lead","user_ids":[lead.user_id]},{"slot":"admins","user_ids":[helper.user_id, f.owner.user_id]},{"slot":"crew","user_ids":[f.member.user_id]}],"idempotency_key":"precise"});
    let (status, first) = apply(&f, &f.admin, &t, body).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    // The same assignments in another order (slots and people) replay it.
    let reordered = json!({"version":1,"name":"Precise","assignments":[{"slot":"crew","user_ids":[f.member.user_id]},{"slot":"admins","user_ids":[f.owner.user_id, helper.user_id]},["lead",[lead.user_id]]],"idempotency_key":"precise"});
    let (status, again) = apply(&f, &f.admin, &t, reordered.clone()).await;
    assert_eq!(
        (
            status,
            again["replayed"].clone(),
            again["workspace_id"].clone()
        ),
        (StatusCode::OK, json!(true), first["workspace_id"].clone())
    );
    // After retirement the replay still returns the recorded workspace.
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            &format!("{TEMPLATES}/{}/versions/1/retire", t.id),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, retired) = apply(&f, &f.admin, &t, reordered).await;
    assert_eq!(
        (status, retired["replayed"].clone()),
        (StatusCode::OK, json!(true))
    );
    // Ineligible people (cleaned, nonexistent) are 409 naming the slot only.
    let t2 = published(&f, &pool, Value::Null).await;
    let cleaned = user(&pool, "precise-cleaned", "user").await;
    sqlx::query("UPDATE users SET cleaned_at=now() WHERE id=$1")
        .bind(cleaned.user_id)
        .execute(&pool)
        .await
        .unwrap();
    for (n, who) in [cleaned.user_id, Uuid::new_v4()].into_iter().enumerate() {
        let (status, body) = apply(&f, &f.admin, &t2, json!({"version":1,"name":format!("Ineligible {n}"),"assignments":[{"slot":"lead","user_ids":[lead.user_id]},{"slot":"crew","user_ids":[who]}],"idempotency_key":format!("ineligible-{n}")})).await;
        assert_eq!(
            (
                status,
                body["error"]["reason"].clone(),
                body["error"]["slot"].clone()
            ),
            (
                StatusCode::CONFLICT,
                json!("template_assignment_ineligible"),
                json!("crew")
            ),
            "{body}"
        );
        assert!(!body.to_string().contains(&who.to_string()));
    }
    // The same person twice in one slot, or a slot listed twice, is 400.
    for (n, assignments) in [
        json!([{"slot":"lead","user_ids":[lead.user_id]},{"slot":"admins","user_ids":[helper.user_id, helper.user_id]},{"slot":"crew","user_ids":[f.member.user_id]}]),
        json!([{"slot":"lead","user_ids":[lead.user_id]},{"slot":"crew","user_ids":[f.member.user_id]},{"slot":"crew","user_ids":[f.owner.user_id]}]),
    ]
    .into_iter()
    .enumerate()
    {
        let (status, body) = apply(&f, &f.admin, &t2, json!({"version":1,"name":format!("Twice {n}"),"assignments":assignments,"idempotency_key":format!("twice-{n}")})).await;
        assert!(matches!(status, StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY), "{n}: {status} {body}");
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn applications_are_bound_in_the_database_and_bodies_are_bounded(pool: PgPool) {
    let f = fixture(&pool).await;
    let t = published(&f, &pool, Value::Null).await;
    let lead = user(&pool, "bound-lead", "user").await;
    // Body-only limits before any lock: more than 100 people, a person twice
    // across slots, a slot listed twice.
    let many: Vec<Uuid> = (0..101).map(|_| Uuid::new_v4()).collect();
    for (n, assignments) in [
        json!([{"slot":"lead","user_ids":[lead.user_id]},{"slot":"admins","user_ids":many}]),
        json!([{"slot":"lead","user_ids":[lead.user_id]},{"slot":"crew","user_ids":[lead.user_id]}]),
        json!([{"slot":"lead","user_ids":[lead.user_id]},{"slot":"lead","user_ids":[lead.user_id]}]),
    ]
    .into_iter()
    .enumerate()
    {
        let (status, body) = apply(&f, &f.admin, &t, json!({"version":1,"name":format!("Bound {n}"),"assignments":assignments,"idempotency_key":format!("bound-{n}")})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{n}: {body}");
    }
    let (status, out) = apply(&f, &f.admin, &t, json!({"version":1,"name":"Bound ok","assignments":[{"slot":"lead","user_ids":[lead.user_id]},{"slot":"crew","user_ids":[f.member.user_id]}],"idempotency_key":"bound-ok"})).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    // The database binds an application: not to an existing workspace, not of
    // another kind, not by someone who is not a Platform Admin.
    let insert = "INSERT INTO workspace_template_applications(id,template_id,version,content_hash,workspace_id,applied_by,idempotency_digest,payload_digest) SELECT gen_random_uuid(),template_id,version,content_hash,$1,$2,sha256(gen_random_uuid()::text::bytea),payload_digest FROM workspace_template_applications LIMIT 1";
    for (ws, by) in [(f.team, f.admin.user_id), (f.project, f.admin.user_id)] {
        assert!(
            sqlx::query(insert)
                .bind(ws)
                .bind(by)
                .execute(&pool)
                .await
                .is_err(),
            "claimed an existing workspace"
        );
    }
    let mut tx = pool.begin().await.unwrap();
    let fresh = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Fresh project','project')")
        .bind(fresh)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        sqlx::query(insert)
            .bind(fresh)
            .bind(f.admin.user_id)
            .execute(&mut *tx)
            .await
            .is_err(),
        "another kind"
    );
    drop(tx);
    let mut tx = pool.begin().await.unwrap();
    let fresh = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Fresh team','team')")
        .bind(fresh)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        sqlx::query(insert)
            .bind(fresh)
            .bind(f.member.user_id)
            .execute(&mut *tx)
            .await
            .is_err(),
        "applied by a non-admin"
    );
    drop(tx);
    // The control: a fresh team by a Platform Admin is accepted.
    let mut tx = pool.begin().await.unwrap();
    let fresh = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Fresh team ok','team')")
        .bind(fresh)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(insert)
        .bind(fresh)
        .bind(f.admin.user_id)
        .execute(&mut *tx)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_applications_with_one_key_create_one_workspace(pool: PgPool) {
    let f = fixture(&pool).await;
    let t = published(&f, &pool, Value::Null).await;
    let body = json!({"version":1,"name":"Concurrent","assignments":[{"slot":"lead","user_ids":[f.owner.user_id]},{"slot":"crew","user_ids":[f.member.user_id]}],"idempotency_key":"concurrent"});
    let before = workspaces(&pool).await;
    let (one, two) = tokio::join!(
        apply(&f, &f.admin, &t, body.clone()),
        apply(&f, &f.admin, &t, body.clone())
    );
    assert_eq!(
        (one.0, two.0),
        (StatusCode::OK, StatusCode::OK),
        "{one:?} {two:?}"
    );
    assert_eq!(one.1["workspace_id"], two.1["workspace_id"]);
    assert_eq!(workspaces(&pool).await, before + 1);
    // Retiring the version during later applications: the replay still
    // returns the workspace, a new application is stale.
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            &format!("{TEMPLATES}/{}/versions/1/retire", t.id),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, again) = apply(&f, &f.admin, &t, body).await;
    assert_eq!(
        (status, again["replayed"].clone()),
        (StatusCode::OK, json!(true))
    );
    let (status, fresh) = apply(&f, &f.admin, &t, json!({"version":1,"name":"After retire","assignments":[{"slot":"lead","user_ids":[f.owner.user_id]},{"slot":"crew","user_ids":[f.member.user_id]}],"idempotency_key":"after-retire"})).await;
    assert_eq!(
        (status, fresh["error"]["reason"].clone()),
        (StatusCode::CONFLICT, json!("template_version_stale"))
    );
}
