//! #84 slice A: authoring and review of Team/Project onboarding templates on
//! real PostgreSQL. Immutable versions; a different Platform Admin reviews
//! before publication; one published version per template; references
//! validated; browser-only Platform Admin routes.
use super::*;

const TEMPLATES: &str = "/api/v1/platform/workspace-templates";

async fn catalog(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO catalogs(id,name) VALUES($1,$2)")
        .bind(id)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    id
}
fn content(catalog: Uuid, model: Uuid) -> Value {
    json!({
        "kind": "team",
        "catalogs": {"mode": "replace", "catalog_ids": [catalog]},
        "model_ids": [model],
        "limits": null,
        "owner": {"name": "lead"},
        "slots": [
            {"name": "engineers", "role": "member", "required": false, "multiple": true}
        ]
    })
}
fn tid(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn templates_are_reviewed_by_another_admin_before_publication(pool: PgPool) {
    let f = fixture(&pool).await;
    let reviewer = user(&pool, "second-admin", "admin").await;
    let c = catalog(&pool, "Onboarding").await;
    let m = model(&pool, "company/onboard").await;
    let (status, t) = call(
        &f.s,
        &f.admin,
        "POST",
        TEMPLATES,
        json!({"name":"Team starter","content":content(c, m)}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{t}");
    assert_eq!(t["versions"][0]["version"], 1);
    assert_eq!(t["versions"][0]["state"], "pending_review");
    assert!(t["versions"][0]["content_hash"].as_str().unwrap().len() == 64);
    let id = tid(&t);
    let v1 = format!("{TEMPLATES}/{id}/versions/1");
    // Not published before review; the author cannot review its own version.
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            &format!("{v1}/publish"),
            Value::Null
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(&f.s, &f.admin, "POST", &format!("{v1}/review"), Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    // Another Platform Admin reviews; then it can be published, once.
    let (status, reviewed) = call(
        &f.s,
        &reviewer,
        "POST",
        &format!("{v1}/review"),
        Value::Null,
    )
    .await;
    assert_eq!(
        (status, reviewed["versions"][0]["state"].clone()),
        (StatusCode::OK, json!("reviewed"))
    );
    assert_eq!(
        reviewed["versions"][0]["reviewed_by"],
        reviewer.user_id.to_string()
    );
    let (status, published) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{v1}/publish"),
        Value::Null,
    )
    .await;
    assert_eq!(
        (status, published["versions"][0]["state"].clone()),
        (StatusCode::OK, json!("published"))
    );
    assert_eq!(
        call(&f.s, &f.admin, "POST", &format!("{v1}/review"), Value::Null)
            .await
            .0,
        StatusCode::CONFLICT
    );
    // A new version is pending; publishing it retires the previous one.
    let mut next = content(c, m);
    next["slots"][0]["role"] = json!("admin");
    let (status, t2) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{TEMPLATES}/{id}/versions"),
        json!({"content":next}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{t2}");
    let v2 = format!("{TEMPLATES}/{id}/versions/2");
    assert_eq!(
        call(
            &f.s,
            &reviewer,
            "POST",
            &format!("{v2}/review"),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, after) = call(
        &f.s,
        &reviewer,
        "POST",
        &format!("{v2}/publish"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{after}");
    let states: Vec<(i64, String)> = after["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            (
                v["version"].as_i64().unwrap(),
                v["state"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(states, vec![(2, "published".into()), (1, "retired".into())]);
    // Retired versions stay retired.
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            &format!("{v1}/publish"),
            Value::Null
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (status, retired) =
        call(&f.s, &f.admin, "POST", &format!("{v2}/retire"), Value::Null).await;
    assert_eq!(
        (status, retired["versions"][0]["state"].clone()),
        (StatusCode::OK, json!("retired"))
    );
    // Lists and detail for Platform readers; audited.
    let (status, list) = call(&f.s, &f.auditor, "GET", TEMPLATES, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["data"][0]["name"], "Team starter");
    let (status, detail) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("{TEMPLATES}/{id}"),
        Value::Null,
    )
    .await;
    assert_eq!(
        (status, detail["versions"].as_array().unwrap().len()),
        (StatusCode::OK, 2)
    );
    let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_events WHERE resource_type='workspace_template' ORDER BY created_at").fetch_all(&pool).await.unwrap();
    assert_eq!(
        actions,
        vec![
            "workspace_template.created",
            "workspace_template.reviewed",
            "workspace_template.published",
            "workspace_template.version_created",
            "workspace_template.reviewed",
            // Publishing version 2 retires version 1, audited as such.
            "workspace_template.retired",
            "workspace_template.published",
            "workspace_template.retired"
        ]
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn template_content_is_references_only_and_validated(pool: PgPool) {
    let f = fixture(&pool).await;
    let c = catalog(&pool, "Valid").await;
    let m = model(&pool, "company/valid").await;
    let good = content(c, m);
    let with = |path: &str, value: Value| {
        let mut v = good.clone();
        let ptr = v.pointer_mut(path).unwrap();
        *ptr = value;
        v
    };
    for (n, bad) in [
        with("/kind", json!("personal")),
        with("/catalogs", json!({"mode":"replace","catalog_ids":[Uuid::new_v4()]})),
        with("/catalogs", json!({"mode":"merge"})),
        with("/model_ids", json!([Uuid::new_v4()])),
        with("/owner", json!({"name":"Lead Person"})),
        with("/owner", json!({"name":"lead","multiple":true})),
        with("/slots", json!([{"name":"a","role":"owner","required":true,"multiple":false}])),
        with("/slots", json!([{"name":"lead","role":"member","required":false,"multiple":true}])),
        with("/slots", json!([{"name":"a","role":"member","required":false,"multiple":true},{"name":"a","role":"admin","required":false,"multiple":true}])),
        with("/slots", json!([{"name":"x","role":"platform_admin","required":true,"multiple":false}])),
        with("/slots", json!((0..10).map(|n| json!({"name":format!("s{n}"),"role":"member","required":false,"multiple":true})).collect::<Vec<_>>())),
        with("/limits", json!({"requests_per_minute":"lots"})),
    ]
    .into_iter()
    .enumerate()
    {
        let (status, body) = call(&f.s, &f.admin, "POST", TEMPLATES, json!({"name":format!("Bad {n}"),"content":bad})).await;
        assert!(matches!(status, StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY), "{n}: {status} {body}");
    }
    // Unknown content fields (an email, a group mapping) are refused.
    let mut extra = good.clone();
    extra["members"] = json!(["someone@example.test"]);
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            TEMPLATES,
            json!({"name":"Extra","content":extra})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    // A valid limits body is accepted (it is validated against live layers at apply).
    let limited = with(
        "/limits",
        json!({"requests_per_minute":60,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[]}),
    );
    let (status, body) = call(
        &f.s,
        &f.admin,
        "POST",
        TEMPLATES,
        json!({"name":"Limited","content":limited}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Names are unique ignoring case and surrounding spaces.
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            TEMPLATES,
            json!({"name":" limited ","content":good})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    // Duplicate names are 409; nothing else was created.
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            TEMPLATES,
            json!({"name":"Limited","content":good})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let created: i64 = sqlx::query_scalar("SELECT count(*) FROM workspace_templates")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(created, 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn templates_are_platform_admin_only_and_immutable(pool: PgPool) {
    let f = fixture(&pool).await;
    let c = catalog(&pool, "Only").await;
    let m = model(&pool, "company/only").await;
    let (_, t) = call(
        &f.s,
        &f.admin,
        "POST",
        TEMPLATES,
        json!({"name":"Only admins","content":content(c, m)}),
    )
    .await;
    let id = tid(&t);
    for actor in [&f.auditor, &f.owner, &f.member, &f.outsider] {
        assert_eq!(
            call(
                &f.s,
                actor,
                "POST",
                TEMPLATES,
                json!({"name":"Nope","content":content(c, m)})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(
                &f.s,
                actor,
                "POST",
                &format!("{TEMPLATES}/{id}/versions/1/review"),
                Value::Null
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    for actor in [&f.owner, &f.member, &f.outsider] {
        assert_eq!(
            call(&f.s, actor, "GET", TEMPLATES, Value::Null).await.0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "GET",
            &format!("{TEMPLATES}/{}", Uuid::new_v4()),
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
            &format!("{TEMPLATES}/{id}/versions/9/review"),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // In the database: content, authorship and decisions never change, the
    // author never reviews, nothing is published unreviewed or deleted.
    for sql in [
        "UPDATE workspace_template_versions SET content='{}'::jsonb",
        "UPDATE workspace_template_versions SET author=reviewed_by",
        "UPDATE workspace_template_versions SET reviewed_by=author,reviewed_at=now()",
        "UPDATE workspace_template_versions SET published_at=now()",
        "DELETE FROM workspace_template_versions",
        "DELETE FROM workspace_templates",
        "UPDATE workspace_templates SET name='renamed'",
        // A hash that is not of the content; a reviewer who is not a Platform Admin.
        "UPDATE workspace_template_versions SET content_hash=sha256('bogus'::bytea)",
        "UPDATE workspace_template_versions SET reviewed_by=(SELECT id FROM users WHERE email LIKE 'member%'),reviewed_at=now() WHERE reviewed_at IS NULL",
        "INSERT INTO workspace_template_versions(id,template_id,version,content,content_hash,author,reviewed_by,reviewed_at) SELECT gen_random_uuid(),template_id,5,content,content_hash,author,NULL,now() FROM workspace_template_versions LIMIT 1",
    ] {
        assert!(
            sqlx::query(sql).execute(&pool).await.is_err(),
            "accepted {sql}"
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn template_steps_hold_under_races_demotion_and_a_single_admin(pool: PgPool) {
    let f = fixture(&pool).await;
    let c = catalog(&pool, "Races").await;
    let m = model(&pool, "company/races").await;
    // A single Platform Admin cannot publish: nobody else can review.
    let (_, t) = call(
        &f.s,
        &f.admin,
        "POST",
        TEMPLATES,
        json!({"name":"Solo","content":content(c, m)}),
    )
    .await;
    let id = tid(&t);
    let v = |n: i32, step: &str| format!("{TEMPLATES}/{id}/versions/{n}/{step}");
    assert_eq!(
        call(&f.s, &f.admin, "POST", &v(1, "review"), Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&f.s, &f.admin, "POST", &v(1, "publish"), Value::Null)
            .await
            .0,
        StatusCode::CONFLICT
    );
    // A reviewer later demoted: the review stands and another admin publishes.
    let reviewer = user(&pool, "soon-demoted", "admin").await;
    assert_eq!(
        call(&f.s, &reviewer, "POST", &v(1, "review"), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(reviewer.user_id)
        .execute(&pool)
        .await
        .unwrap();
    for step in ["review", "publish", "retire"] {
        assert_eq!(
            call(&f.s, &reviewer, "POST", &v(1, step), Value::Null)
                .await
                .0,
            StatusCode::FORBIDDEN,
            "{step}"
        );
        assert_eq!(
            call(&f.s, &f.auditor, "POST", &v(1, step), Value::Null)
                .await
                .0,
            StatusCode::FORBIDDEN,
            "{step}"
        );
    }
    // Two reviewed versions published at once: exactly one ends published.
    let second = user(&pool, "second-reviewer", "admin").await;
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{TEMPLATES}/{id}/versions"),
        json!({"content":content(c, m)}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        call(&f.s, &second, "POST", &v(2, "review"), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    let (p1, p2) = (v(1, "publish"), v(2, "publish"));
    let (one, two) = tokio::join!(
        call(&f.s, &f.admin, "POST", &p1, Value::Null),
        call(&f.s, &second, "POST", &p2, Value::Null)
    );
    assert_eq!(
        (one.0, two.0),
        (StatusCode::OK, StatusCode::OK),
        "{one:?} {two:?}"
    );
    let published: i64 = sqlx::query_scalar("SELECT count(*) FROM workspace_template_versions WHERE template_id=$1::uuid AND published_at IS NOT NULL AND retired_at IS NULL").bind(&id).fetch_one(&pool).await.unwrap();
    assert_eq!(published, 1);
    // A pending version retired cannot then be reviewed or published.
    let (_, _) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("{TEMPLATES}/{id}/versions"),
        json!({"content":content(c, m)}),
    )
    .await;
    assert_eq!(
        call(&f.s, &f.admin, "POST", &v(3, "retire"), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f.s, &second, "POST", &v(3, "review"), Value::Null)
            .await
            .0,
        StatusCode::CONFLICT
    );
    // Each step is audited with its version and actor.
    let (actor, meta): (Uuid, Value) = sqlx::query_as("SELECT actor_user_id,metadata FROM audit_events WHERE action='workspace_template.reviewed' AND resource_id=$1::uuid ORDER BY created_at LIMIT 1").bind(&id).fetch_one(&pool).await.unwrap();
    assert_eq!((actor, meta), (reviewer.user_id, json!({"version":1})));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn template_limits_and_forms_are_strict(pool: PgPool) {
    let f = fixture(&pool).await;
    let c = catalog(&pool, "Strict").await;
    let m = model(&pool, "company/strict").await;
    let base = content(c, m);
    let with_limits = |limits: Value| {
        let mut v = base.clone();
        v["limits"] = limits;
        v
    };
    let ok = json!({"requests_per_minute":60,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[{"period":"month","amount_microusd":"1000"}]});
    for (n, bad) in [
        with_limits(json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"monthly_budget_microusd":"1000"})),
        with_limits(json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[],"budget_period":"month"})),
        with_limits(json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"budgets":null})),
        with_limits(json!({"requests_per_minute":0,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[]})),
        with_limits(json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[{"period":"day","amount_microusd":"1"},{"period":"day","amount_microusd":"2"}]})),
        with_limits(json!([60, null, null])),
        with_limits(json!("60")),
        // Positional forms inside content, and catalog_ids beside inherit.
        { let mut v = base.clone(); v["owner"] = json!(["lead"]); v },
        { let mut v = base.clone(); v["catalogs"] = json!(["replace", [c]]); v },
        { let mut v = base.clone(); v["slots"] = json!([["crew", "member", false, true]]); v },
        { let mut v = base.clone(); v["catalogs"] = json!({"mode":"inherit","catalog_ids":null}); v },
    ]
    .into_iter()
    .enumerate()
    {
        let (status, body) = call(&f.s, &f.admin, "POST", TEMPLATES, json!({"name":format!("Strict {n}"),"content":bad})).await;
        assert!(matches!(status, StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY), "{n}: {status} {body}");
    }
    let (status, body) = call(
        &f.s,
        &f.admin,
        "POST",
        TEMPLATES,
        json!({"name":"Strict ok","content":with_limits(ok)}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // An omitted limits is stored as null.
    let mut omitted = base.clone();
    omitted.as_object_mut().unwrap().remove("limits");
    let (status, body) = call(
        &f.s,
        &f.admin,
        "POST",
        TEMPLATES,
        json!({"name":"Strict omitted","content":omitted}),
    )
    .await;
    assert_eq!(
        (status, body["versions"][0]["content"]["limits"].clone()),
        (StatusCode::OK, Value::Null)
    );
    // The stored hash is the database's hash of the stored content.
    let matches: bool = sqlx::query_scalar("SELECT bool_and(content_hash=sha256(convert_to(content::text,'UTF8'))) FROM workspace_template_versions").fetch_one(&pool).await.unwrap();
    assert!(matches);
}
