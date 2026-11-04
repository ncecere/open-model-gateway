//! #88: explicit-target bulk disable, on real PostgreSQL: independent targets
//! with per-target outcomes, actor-bound idempotency, no replay of unknown
//! outcomes (an interrupted operation completes once, its undecided targets
//! unattempted), history correlation and immutable operation records.
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
    assert_eq!(revision(&pool, a).await, (false, 2));
}

/// An operation as an interrupted request leaves it: created with its items,
/// none decided, with the digest of the same targets. `lapsed`: its lease.
async fn interrupted(pool: &PgPool, actor: Uuid, key: &str, a: Uuid, lapsed: bool) -> Uuid {
    let digest = {
        use sha2::{Digest, Sha256};
        Sha256::digest(format!("deployment.disable\n{a}:1").as_bytes()).to_vec()
    };
    let operation = Uuid::now_v7();
    sqlx::query("INSERT INTO configuration_bulk_operations(id,actor_user_id,idempotency_key,payload_digest,action,item_count,lease_until) VALUES($1,$2,$3,$4,'deployment.disable',1,clock_timestamp()+CASE WHEN $5 THEN interval '-1 second' ELSE interval '1 minute' END)").bind(operation).bind(actor).bind(key).bind(&digest).bind(lapsed).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO configuration_bulk_items(operation_id,position,deployment_id,expected_revision) VALUES($1,0,$2,1)").bind(operation).bind(a).execute(pool).await.unwrap();
    operation
}
async fn outcome_events(pool: &PgPool, operation: Uuid) -> Vec<Value> {
    sqlx::query_scalar("SELECT metadata FROM audit_events WHERE action='configuration.bulk_disabled' AND resource_id=$1")
        .bind(operation)
        .fetch_all(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn interrupted_operations_complete_once_and_are_never_replayed(pool: PgPool) {
    let f = fixture(&pool).await;
    let a = route(&f, &pool, "interrupted-a").await;
    // In progress (its lease is live): never replayed; status shows the
    // target undecided and completes nothing.
    let live = interrupted(&pool, f.admin.user_id, "in-progress", a, false).await;
    // It cannot be marked complete while an item has no outcome.
    assert!(
        sqlx::query("UPDATE configuration_bulk_operations SET completed_at=now() WHERE id=$1")
            .bind(live)
            .execute(&pool)
            .await
            .is_err()
    );
    let (status, out) = call(
        &f.s,
        &f.admin,
        "POST",
        BULK,
        body("in-progress", &[(a, "1")]),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{out}");
    assert_eq!(out["error"]["reason"], "bulk_incomplete");
    let (_, pending) = call(
        &f.s,
        &f.admin,
        "GET",
        &format!("{BULK}/in-progress"),
        Value::Null,
    )
    .await;
    assert_eq!(pending["completed"], false);
    assert!(pending["items"][0]["outcome"].is_null());
    assert!(outcome_events(&pool, live).await.is_empty());
    // Lapsed: the status read completes it once, the target unattempted and
    // not applied, with one outcome event; later reads and replays add none.
    let lapsed = interrupted(&pool, f.admin.user_id, "lapsed", a, true).await;
    let (status, done) = call(
        &f.s,
        &f.admin,
        "GET",
        &format!("{BULK}/lapsed"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["completed"], true);
    assert_eq!(done["items"][0]["outcome"], "unattempted");
    assert!(done["items"][0]["revision"].is_null());
    let (status, replay) = call(&f.s, &f.admin, "POST", BULK, body("lapsed", &[(a, "1")])).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(
        (replay["replayed"].clone(), replay["items"].clone()),
        (json!(true), done["items"].clone())
    );
    call(
        &f.s,
        &f.admin,
        "GET",
        &format!("{BULK}/lapsed"),
        Value::Null,
    )
    .await;
    assert_eq!(
        outcome_events(&pool, lapsed).await,
        vec![json!({"count":1,"items":[{"id":a,"outcome":"unattempted"}]})]
    );
    assert_eq!(revision(&pool, a).await, (true, 1));
    // A lapsed operation is completed by a replay too, never applied.
    let replayed = interrupted(&pool, f.admin.user_id, "lapsed-replay", a, true).await;
    let (status, out) = call(
        &f.s,
        &f.admin,
        "POST",
        BULK,
        body("lapsed-replay", &[(a, "1")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["items"][0]["outcome"], "unattempted");
    assert_eq!(outcome_events(&pool, replayed).await.len(), 1);
    assert_eq!(revision(&pool, a).await, (true, 1));
    // Once completed, an item that lost the race cannot be decided or applied:
    // the history link needs an undecided item of an incomplete operation.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('omg.configuration_bulk',$1,true)")
        .bind(lapsed.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE deployments SET enabled=false WHERE id=$1")
            .bind(a)
            .execute(&mut *tx)
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_requests_with_one_key_apply_once(pool: PgPool) {
    let f = fixture(&pool).await;
    let targets: Vec<Uuid> = {
        let mut v = Vec::new();
        for n in 0..5 {
            v.push(route(&f, &pool, &format!("race-{n}")).await);
        }
        v
    };
    let items: Vec<(Uuid, &str)> = targets.iter().map(|t| (*t, "1")).collect();
    let request = body("race", &items);
    let (one, two) = tokio::join!(
        call(&f.s, &f.admin, "POST", BULK, request.clone()),
        call(&f.s, &f.admin, "POST", BULK, request.clone()),
    );
    let fresh = [&one, &two]
        .iter()
        .filter(|(status, out)| *status == StatusCode::OK && out["replayed"] == false)
        .count();
    assert_eq!(fresh, 1, "{one:?} {two:?}");
    for (status, out) in [&one, &two] {
        assert!(
            *status == StatusCode::OK
                || (*status == StatusCode::CONFLICT && out["error"]["reason"] == "bulk_incomplete"),
            "{status} {out}"
        );
    }
    for t in &targets {
        assert_eq!(revision(&pool, *t).await, (false, 2));
    }
    let changes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM configuration_changes WHERE bulk_operation_id IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(changes, 5);
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='configuration.bulk_disabled'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events, 1);
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
    let pending = interrupted(&pool, f.admin.user_id, "forgery", b, false).await;
    for sql in [
        "UPDATE configuration_bulk_items SET outcome='refused',reason='forbidden',change_id=NULL",
        // Created decided, complete, or beyond the declared count.
        "INSERT INTO configuration_bulk_operations(id,actor_user_id,idempotency_key,payload_digest,action,item_count,completed_at) SELECT gen_random_uuid(),actor_user_id,'done',payload_digest,action,1,now() FROM configuration_bulk_operations LIMIT 1",
        "INSERT INTO configuration_bulk_items(operation_id,position,deployment_id,expected_revision,outcome,decided_at) SELECT id,1,gen_random_uuid(),1,'unattempted',now() FROM configuration_bulk_operations WHERE idempotency_key='forgery'",
        "INSERT INTO configuration_bulk_items(operation_id,position,deployment_id,expected_revision) SELECT id,1,gen_random_uuid(),1 FROM configuration_bulk_operations WHERE idempotency_key='forgery'",
        // Committed without its own change: an unrelated change, wrong revision.
        "UPDATE configuration_bulk_items SET outcome='committed',revision=2,change_id=(SELECT id FROM configuration_changes LIMIT 1),decided_at=now() WHERE deployment_id IN (SELECT deployment_id FROM configuration_bulk_items i JOIN configuration_bulk_operations o ON o.id=i.operation_id WHERE o.idempotency_key='forgery')",
        "UPDATE configuration_bulk_operations SET completed_at=now()",
        "DELETE FROM configuration_bulk_items",
        "DELETE FROM configuration_bulk_operations",
        "TRUNCATE configuration_bulk_items",
        "UPDATE configuration_changes SET bulk_operation_id=NULL",
        "UPDATE configuration_bulk_operations SET lease_until=now() WHERE completed_at IS NOT NULL",
        // A pending item carrying a change, an update that decides nothing, a
        // lease held far ahead.
        "UPDATE configuration_bulk_items SET change_id=gen_random_uuid() WHERE outcome IS NULL",
        "UPDATE configuration_bulk_items SET expected_revision=expected_revision WHERE outcome IS NULL",
        "UPDATE configuration_bulk_operations SET lease_until=clock_timestamp()+interval '1 hour' WHERE completed_at IS NULL",
    ] {
        assert!(
            sqlx::query(sql).execute(&pool).await.is_err(),
            "accepted {sql}"
        );
    }
    // A bulk link needs a pending item of that operation for that target, at
    // the next revision, recorded by the operation's actor, and only a
    // disable: a completed operation, another target, an enable, another
    // actor (no configuration actor) all fail.
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
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('omg.configuration_bulk',$1,true)")
        .bind(pending.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE deployments SET enabled=false WHERE id=$1")
            .bind(b)
            .execute(&mut *tx)
            .await
            .is_err(),
        "linked a change without the operation's actor"
    );
    drop(tx);
    // The control: the same change by the operation's actor links.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('omg.configuration_bulk',$1,true),set_config('omg.configuration_actor',$2,true)")
        .bind(pending.to_string())
        .bind(f.admin.user_id.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE deployments SET enabled=false WHERE id=$1")
        .bind(b)
        .execute(&mut *tx)
        .await
        .unwrap();
    // At commit the linked change must be the item's committed outcome:
    // recording anything else for it fails the whole transaction.
    sqlx::query("UPDATE configuration_bulk_items SET outcome='unattempted' WHERE operation_id=$1")
        .bind(pending)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        tx.commit().await.is_err(),
        "a linked change recorded as unattempted committed"
    );
    assert_eq!(revision(&pool, b).await, (true, 1));
    // Its committed outcome commits, on the server's clock whatever the
    // caller sends; completion too.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('omg.configuration_bulk',$1,true),set_config('omg.configuration_actor',$2,true)")
        .bind(pending.to_string())
        .bind(f.admin.user_id.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE deployments SET enabled=false WHERE id=$1")
        .bind(b)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE configuration_bulk_items SET outcome='committed',revision=2,change_id=(SELECT id FROM configuration_changes WHERE deployment_id=$2 AND revision=2),decided_at='1999-01-01' WHERE operation_id=$1")
        .bind(pending)
        .bind(b)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE configuration_bulk_operations SET completed_at='2001-01-01' WHERE id=$1")
        .bind(pending)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (decided, completed): (bool, bool) = sqlx::query_as("SELECT i.decided_at>now()-interval '1 minute',o.completed_at>now()-interval '1 minute' FROM configuration_bulk_items i JOIN configuration_bulk_operations o ON o.id=i.operation_id WHERE o.id=$1")
        .bind(pending)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(decided && completed, "caller-chosen clocks recorded");
}
