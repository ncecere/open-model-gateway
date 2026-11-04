//! #87: guarded apply and restore of deployment enablement, on real
//! PostgreSQL. A restore is a new, currently authorized change linked to its
//! source; it never edits history, and any intervening change refuses it.
use super::*;

fn path(id: Uuid, rest: &str) -> String {
    format!("/api/v1/platform/deployments/{id}/configuration/{rest}")
}

async fn create(f: &Fixture, pool: &PgPool, name: &str) -> Uuid {
    let m = model(pool, name).await;
    let p = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,$2,'openai','env:UNUSED')").bind(p).bind(name).execute(pool).await.unwrap();
    let (status, body) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/deployments",
        json!({"model_id":m,"provider_connection_id":p,"upstream_model":name,"enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

async fn apply(f: &Fixture, u: &BrowserPrincipal, d: Uuid, body: Value) -> (StatusCode, Value) {
    call(&f.s, u, "POST", &path(d, "apply"), body).await
}

async fn changes(pool: &PgPool, d: Uuid) -> Vec<(i64, bool, Option<Uuid>, Uuid)> {
    sqlx::query_as("SELECT revision,after_enabled,restored_from,id FROM configuration_changes WHERE deployment_id=$1 ORDER BY revision")
        .bind(d)
        .fetch_all(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn guarded_apply_enforces_the_expected_revision(pool: PgPool) {
    let f = fixture(&pool).await;
    let d = create(&f, &pool, "guarded").await;
    let (_, preview) = call(
        &f.s,
        &f.admin,
        "POST",
        &path(d, "preview"),
        json!({"enabled":false}),
    )
    .await;
    assert_eq!(preview["expected_revision"], "1");
    let (status, body) = apply(
        &f,
        &f.admin,
        d,
        json!({"enabled":false,"expected_revision":"1"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcome"], "applied");
    assert_eq!(body["revision"], "2");
    let rows = changes(&pool, d).await;
    assert_eq!(body["change_id"], rows[1].3.to_string());
    assert!(body["restored_from"].is_null());
    // A stale preview refuses: nothing is written.
    let (status, body) = apply(
        &f,
        &f.admin,
        d,
        json!({"enabled":true,"expected_revision":"1"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["reason"], "revision_conflict");
    assert_eq!(changes(&pool, d).await.len(), 2);
    // Change away and back (ABA): the state matches revision 2, the revision does not.
    for (enabled, expected) in [(true, "2"), (false, "3")] {
        let (status, _) = apply(
            &f,
            &f.admin,
            d,
            json!({"enabled":enabled,"expected_revision":expected}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, body) = apply(
        &f,
        &f.admin,
        d,
        json!({"enabled":true,"expected_revision":"2"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["reason"], "revision_conflict");
    // Requesting the current state is reported, never recorded.
    let (status, body) = apply(
        &f,
        &f.admin,
        d,
        json!({"enabled":false,"expected_revision":"4"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcome"], "unchanged");
    assert_eq!(body["revision"], "4");
    assert!(body["change_id"].is_null());
    assert_eq!(changes(&pool, d).await.len(), 4);
    // Malformed revisions and unknown targets.
    for revision in [json!(4), json!("-1"), json!("04x"), json!(""), Value::Null] {
        let (status, _) = apply(
            &f,
            &f.admin,
            d,
            json!({"enabled":true,"expected_revision":revision}),
        )
        .await;
        assert!(
            matches!(
                status,
                StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
            ),
            "{revision}: {status}"
        );
    }
    let (status, _) = apply(
        &f,
        &f.admin,
        Uuid::new_v4(),
        json!({"enabled":true,"expected_revision":"1"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn restore_is_a_new_change_linked_to_its_source(pool: PgPool) {
    let f = fixture(&pool).await;
    let d = create(&f, &pool, "restore").await;
    let (status, _) = apply(
        &f,
        &f.admin,
        d,
        json!({"enabled":false,"expected_revision":"1"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let source = changes(&pool, d).await[0].3;
    let (status, preview) = call(
        &f.s,
        &f.admin,
        "POST",
        &path(d, "restore-preview"),
        json!({"change_id":source}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["expected_revision"], "2");
    assert_eq!(
        preview["diff"],
        json!([{"field":"enabled","before":false,"after":true}])
    );
    assert_eq!(preview["restore"]["change_id"], source.to_string());
    assert_eq!(preview["restore"]["revision"], "1");
    assert_eq!(preview["restore"]["enabled"], true);
    // A preview writes nothing.
    assert_eq!(changes(&pool, d).await.len(), 2);
    let (status, body) = apply(
        &f,
        &f.admin,
        d,
        json!({"enabled":true,"expected_revision":"2","restore_of":source}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcome"], "applied");
    assert_eq!(body["restored_from"], source.to_string());
    let rows = changes(&pool, d).await;
    assert_eq!(rows.len(), 3);
    assert_eq!((rows[2].0, rows[2].1, rows[2].2), (3, true, Some(source)));
    let actor: Option<Uuid> = sqlx::query_scalar("SELECT a.actor_user_id FROM audit_events a JOIN configuration_changes c ON (a.id,a.created_at)=(c.audit_event_id,c.audit_created_at) WHERE c.deployment_id=$1 AND c.revision=3").bind(d).fetch_one(&pool).await.unwrap();
    assert_eq!(actor, Some(f.admin.user_id));
    // History shows the link; the source entry is unchanged.
    let (_, history) = call(&f.s, &f.auditor, "GET", &path(d, "history"), Value::Null).await;
    assert_eq!(history["data"][0]["restored_from"], source.to_string());
    assert!(history["data"][2]["restored_from"].is_null());
    // The link is immutable like the rest of the snapshot.
    assert!(
        sqlx::query("UPDATE configuration_changes SET restored_from=NULL WHERE deployment_id=$1")
            .bind(d)
            .execute(&pool)
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn restore_refuses_unsafe_sources_and_unauthorized_actors(pool: PgPool) {
    let f = fixture(&pool).await;
    let d = create(&f, &pool, "refusals").await;
    let other = create(&f, &pool, "refusals-other").await;
    let (status, _) = apply(
        &f,
        &f.admin,
        d,
        json!({"enabled":false,"expected_revision":"1"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows = changes(&pool, d).await;
    let (enabled_source, disabled_source) = (rows[0].3, rows[1].3);
    let foreign = changes(&pool, other).await[0].3;
    // Unknown and foreign sources are indistinguishable.
    for change in [Uuid::new_v4(), foreign] {
        let (status, _) = call(
            &f.s,
            &f.admin,
            "POST",
            &path(d, "restore-preview"),
            json!({"change_id":change}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = apply(
            &f,
            &f.admin,
            d,
            json!({"enabled":true,"expected_revision":"2","restore_of":change}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    // A legacy event without a snapshot has no values to restore.
    let legacy = Uuid::new_v4();
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,action,resource_type,resource_id,metadata,created_at) VALUES($1,$2,'configuration.updated','deployments',$3,'{}',now()-interval '1 day')").bind(legacy).bind(f.admin.user_id).bind(d).execute(&pool).await.unwrap();
    let (status, body) = call(
        &f.s,
        &f.admin,
        "POST",
        &path(d, "restore-preview"),
        json!({"change_id":legacy}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["reason"], "restore_values_unavailable");
    // The apply must state the source's value.
    let (status, body) = apply(
        &f,
        &f.admin,
        d,
        json!({"enabled":true,"expected_revision":"2","restore_of":disabled_source}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["reason"], "restore_source_mismatch");
    assert_eq!(changes(&pool, d).await.len(), 2);
    // Live platform authority: auditors and members never apply or restore.
    for actor in [&f.auditor, &f.owner, &f.outsider] {
        for (rest, body) in [
            ("apply", json!({"enabled":true,"expected_revision":"2"})),
            ("restore-preview", json!({"change_id":enabled_source})),
        ] {
            assert_eq!(
                call(&f.s, actor, "POST", &path(d, rest), body).await.0,
                StatusCode::FORBIDDEN
            );
        }
    }
    sqlx::query("DELETE FROM platform_role_grants WHERE user_id=$1 AND role='admin'")
        .bind(f.admin.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        apply(
            &f,
            &f.admin,
            d,
            json!({"enabled":true,"expected_revision":"2","restore_of":enabled_source}),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(changes(&pool, d).await.len(), 2);
    // The database refuses a link to another target's change or to a value
    // the source did not have, whatever writer sets the link.
    for source in [foreign, disabled_source] {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT set_config('omg.configuration_restore',$1,true)")
            .bind(source.to_string())
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(
            sqlx::query("UPDATE deployments SET enabled=true WHERE id=$1")
                .bind(d)
                .execute(&mut *tx)
                .await
                .is_err(),
            "linked an unsafe source"
        );
    }
    assert_eq!(changes(&pool, d).await.len(), 2);
}
