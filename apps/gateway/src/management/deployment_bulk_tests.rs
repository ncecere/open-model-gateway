//! #88: explicit-target bulk disable, on real PostgreSQL: independent targets
//! with per-target outcomes, actor-bound idempotency, no replay of unknown
//! outcomes, history correlation and immutable operation records.
use super::*;

const BULK: &str = "/api/v1/platform/deployment-bulk-operations";

async fn route(f: &Fixture, pool: &PgPool, name: &str) -> Uuid {
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
    id(&body)
}
fn body(key: &str, items: &[(Uuid, &str)]) -> Value {
    json!({"action":"disable","idempotency_key":key,"items":items.iter().map(|(id,r)| json!({"id":id,"expected_revision":r})).collect::<Vec<_>>()})
}
async fn revision(pool: &PgPool, d: Uuid) -> (bool, i64) {
    sqlx::query_as("SELECT enabled,revision FROM deployments WHERE id=$1")
        .bind(d)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn bulk_disable_applies_each_target_independently(pool: PgPool) {
    let f = fixture(&pool).await;
    let (a, b, c) = (
        route(&f, &pool, "bulk-a").await,
        route(&f, &pool, "bulk-b").await,
        route(&f, &pool, "bulk-c").await,
    );
    // c is already disabled at revision 2.
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("/api/v1/platform/deployments/{c}/configuration/apply"),
        json!({"enabled":false,"expected_revision":"1"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let missing = Uuid::new_v4();
    let (status, out) = call(
        &f.s,
        &f.admin,
        "POST",
        BULK,
        body("bulk-1", &[(a, "1"), (b, "7"), (c, "2"), (missing, "1")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["completed"], true);
    assert_eq!(out["replayed"], false);
    let items = out["items"].as_array().unwrap();
    let summary: Vec<(String, Value, Value)> = items
        .iter()
        .map(|i| {
            (
                i["id"].as_str().unwrap().to_owned(),
                i["outcome"].clone(),
                i["reason"].clone(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (a.to_string(), json!("committed"), Value::Null),
            (b.to_string(), json!("conflict"), json!("revision_conflict")),
            (c.to_string(), json!("unchanged"), Value::Null),
            (missing.to_string(), json!("refused"), json!("not_found")),
        ]
    );
    assert_eq!(items[0]["revision"], "2");
    assert_eq!(items[1]["revision"], "1");
    assert_eq!(revision(&pool, a).await, (false, 2));
    assert_eq!(revision(&pool, b).await, (true, 1));
    assert_eq!(revision(&pool, c).await, (false, 2));
    // The committed change names the operation, in history and audit.
    let operation = Uuid::parse_str(out["operation_id"].as_str().unwrap()).unwrap();
    let (change, linked, metadata): (Uuid, Option<Uuid>, Value) = sqlx::query_as("SELECT c.id,c.bulk_operation_id,a.metadata FROM configuration_changes c JOIN audit_events a ON (a.id,a.created_at)=(c.audit_event_id,c.audit_created_at) WHERE c.deployment_id=$1 AND c.revision=2").bind(a).fetch_one(&pool).await.unwrap();
    assert_eq!(items[0]["change_id"], change.to_string());
    assert_eq!(linked, Some(operation));
    assert_eq!(metadata["bulk_operation_id"], operation.to_string());
    let (actor, event): (Option<Uuid>, Value) = sqlx::query_as("SELECT actor_user_id,metadata FROM audit_events WHERE action='configuration.bulk_disabled' AND resource_id=$1").bind(operation).fetch_one(&pool).await.unwrap();
    assert_eq!(actor, Some(f.admin.user_id));
    assert_eq!(event["count"], 4);
    assert_eq!(event["items"][1], json!({"id":b,"outcome":"conflict"}));
    // History shows no link on changes made outside the operation.
    let unlinked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM configuration_changes WHERE bulk_operation_id IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unlinked, 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn bulk_idempotency_is_actor_bound_and_never_replays(pool: PgPool) {
    let f = fixture(&pool).await;
    let a = route(&f, &pool, "idem-a").await;
    let first = call(&f.s, &f.admin, "POST", BULK, body("same-key", &[(a, "1")])).await;
    assert_eq!(first.0, StatusCode::OK);
    // Same key, same targets: the recorded outcomes, nothing applied again.
    let replay = call(&f.s, &f.admin, "POST", BULK, body("same-key", &[(a, "1")])).await;
    assert_eq!(replay.0, StatusCode::OK);
    assert_eq!(replay.1["replayed"], true);
    assert_eq!(replay.1["items"], first.1["items"]);
    assert_eq!(replay.1["operation_id"], first.1["operation_id"]);
    assert_eq!(revision(&pool, a).await, (false, 2));
    // Same key, other targets.
    let (status, out) = call(&f.s, &f.admin, "POST", BULK, body("same-key", &[(a, "2")])).await;
    assert_eq!(status, StatusCode::CONFLICT, "{out}");
    assert_eq!(out["error"]["reason"], "idempotency_key_reused");
    // Status: the caller's own operation only.
    let (status, own) = call(
        &f.s,
        &f.admin,
        "GET",
        &format!("{BULK}/same-key"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(own["items"], first.1["items"]);
    let other = user(&pool, "other-admin", "admin").await;
    assert_eq!(
        call(
            &f.s,
            &other,
            "GET",
            &format!("{BULK}/same-key"),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // Another admin's identical key is a separate operation.
    let (status, theirs) = call(&f.s, &other, "POST", BULK, body("same-key", &[(a, "1")])).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(theirs["operation_id"], first.1["operation_id"]);
    assert_eq!(theirs["items"][0]["outcome"], "conflict");
    // An interrupted operation (no completion, an item without an outcome) is
    // never replayed: the caller reads its status instead. Seeded as a crash
    // would leave it, with the digest of the same targets.
    let digest = {
        use sha2::{Digest, Sha256};
        Sha256::digest(format!("deployment.disable\n{a}:2").as_bytes()).to_vec()
    };
    let interrupted = Uuid::now_v7();
    sqlx::query("INSERT INTO configuration_bulk_operations(id,actor_user_id,idempotency_key,payload_digest,action,item_count) VALUES($1,$2,'interrupted-2',$3,'deployment.disable',1)").bind(interrupted).bind(f.admin.user_id).bind(&digest).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO configuration_bulk_items(operation_id,position,deployment_id,expected_revision) VALUES($1,0,$2,2)").bind(interrupted).bind(a).execute(&pool).await.unwrap();
    // It cannot be marked complete while an item has no outcome.
    assert!(
        sqlx::query("UPDATE configuration_bulk_operations SET completed_at=now() WHERE id=$1")
            .bind(interrupted)
            .execute(&pool)
            .await
            .is_err()
    );
    let (status, out) = call(
        &f.s,
        &f.admin,
        "POST",
        BULK,
        body("interrupted-2", &[(a, "2")]),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{out}");
    assert_eq!(out["error"]["reason"], "bulk_incomplete");
    let (_, status_body) = call(
        &f.s,
        &f.admin,
        "GET",
        &format!("{BULK}/interrupted-2"),
        Value::Null,
    )
    .await;
    assert_eq!(status_body["completed"], false);
    assert!(status_body["items"][0]["outcome"].is_null());
    assert_eq!(revision(&pool, a).await, (false, 2));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn bulk_refuses_unbounded_or_ambiguous_requests_and_other_actors(pool: PgPool) {
    let f = fixture(&pool).await;
    let a = route(&f, &pool, "refuse-a").await;
    let many: Vec<(Uuid, &str)> = (0..51).map(|_| (Uuid::new_v4(), "1")).collect();
    for bad in [
        body("k", &[]),
        body("k", &many),
        body("k", &[(a, "1"), (a, "1")]),
        body("k", &[(a, "x")]),
        body("k", &[(a, "-1")]),
        body("", &[(a, "1")]),
        body("bad key!", &[(a, "1")]),
        body(&"k".repeat(129), &[(a, "1")]),
        json!({"action":"enable","idempotency_key":"k","items":[{"id":a,"expected_revision":"1"}]}),
    ] {
        let (status, _) = call(&f.s, &f.admin, "POST", BULK, bad.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    }
    for bad in [
        json!({"action":"disable","idempotency_key":"k","items":[{"id":a,"expected_revision":"1","enabled":true}]}),
        json!({"action":"disable","idempotency_key":"k","filter":"all","items":[]}),
    ] {
        let (status, _) = call(&f.s, &f.admin, "POST", BULK, bad.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    let full: Vec<(Uuid, &str)> = (0..50).map(|_| (Uuid::new_v4(), "1")).collect();
    let (status, out) = call(&f.s, &f.admin, "POST", BULK, body("fifty", &full)).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["items"].as_array().unwrap().len(), 50);
    for actor in [&f.auditor, &f.owner, &f.outsider] {
        assert_eq!(
            call(&f.s, actor, "POST", BULK, body("k2", &[(a, "1")]))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&f.s, actor, "GET", &format!("{BULK}/fifty"), Value::Null)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(revision(&pool, a).await, (true, 1));
    let created: i64 = sqlx::query_scalar("SELECT count(*) FROM configuration_bulk_operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(created, 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn bulk_records_are_immutable_and_links_are_bound(pool: PgPool) {
    let f = fixture(&pool).await;
    let a = route(&f, &pool, "immutable-a").await;
    let b = route(&f, &pool, "immutable-b").await;
    let (_, out) = call(&f.s, &f.admin, "POST", BULK, body("immutable", &[(a, "1")])).await;
    let operation = Uuid::parse_str(out["operation_id"].as_str().unwrap()).unwrap();
    for sql in [
        "UPDATE configuration_bulk_items SET outcome='refused',reason='forbidden',change_id=NULL",
        "UPDATE configuration_bulk_operations SET completed_at=now()",
        "DELETE FROM configuration_bulk_items",
        "DELETE FROM configuration_bulk_operations",
        "TRUNCATE configuration_bulk_items",
        "UPDATE configuration_changes SET bulk_operation_id=NULL",
    ] {
        assert!(
            sqlx::query(sql).execute(&pool).await.is_err(),
            "accepted {sql}"
        );
    }
    // A bulk link needs a pending item of that operation for that target, and
    // only a disable.
    for (target, enabled) in [(b, false), (a, true)] {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT set_config('omg.configuration_bulk',$1,true)")
            .bind(operation.to_string())
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(
            sqlx::query("UPDATE deployments SET enabled=$2 WHERE id=$1")
                .bind(target)
                .bind(enabled)
                .execute(&mut *tx)
                .await
                .is_err(),
            "linked an unrelated change"
        );
    }
}
