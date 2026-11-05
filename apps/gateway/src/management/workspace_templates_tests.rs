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
        "INSERT INTO workspace_template_versions(id,template_id,version,content,content_hash,author,reviewed_by,reviewed_at) SELECT gen_random_uuid(),template_id,5,content,content_hash,author,NULL,now() FROM workspace_template_versions LIMIT 1",
    ] {
        assert!(
            sqlx::query(sql).execute(&pool).await.is_err(),
            "accepted {sql}"
        );
    }
}
