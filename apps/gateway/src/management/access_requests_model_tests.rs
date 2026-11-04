//! #83 slice B: model-selection requests on real PostgreSQL. A member of a
//! shared workspace requests a catalog-eligible model; an owner/admin of that
//! workspace (never platform authority alone, never the requester) approves,
//! which selects the model from the workspace's catalog, atomically.
use super::*;

const MINE: &str = "/api/v1/me/access-requests";

/// A model in a catalog offered to Teams, so it is eligible for `f.team`.
async fn eligible_model(pool: &PgPool, name: &str) -> Uuid {
    let m = model(pool, name).await;
    let catalog = Uuid::new_v4();
    sqlx::query("INSERT INTO catalogs(id,name) VALUES($1,$2)")
        .bind(catalog)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalog_models(catalog_id,model_id) VALUES($1,$2)")
        .bind(catalog)
        .bind(m)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_type_catalogs(kind,catalog_id) VALUES('team',$1)")
        .bind(catalog)
        .execute(pool)
        .await
        .unwrap();
    m
}
async fn ask(
    f: &Fixture,
    actor: &BrowserPrincipal,
    ws: Uuid,
    model: Uuid,
    key: &str,
) -> (StatusCode, Value) {
    call(
        &f.s,
        actor,
        "POST",
        MINE,
        json!({"workspace_id":ws,"model_id":model,"reason":"Need it","idempotency_key":key}),
    )
    .await
}
async fn decide(
    f: &Fixture,
    actor: &BrowserPrincipal,
    id: &Value,
    action: &str,
    platform: bool,
) -> (StatusCode, Value) {
    let id = id.as_str().unwrap();
    let path = if platform {
        format!("/api/v1/platform/access-requests/{id}/{action}")
    } else {
        format!(
            "/api/v1/workspaces/{}/access-requests/{id}/{action}",
            f.team
        )
    };
    call(&f.s, actor, "POST", &path, Value::Null).await
}
async fn selected(pool: &PgPool, ws: Uuid, model: Uuid) -> bool {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_model_grants WHERE workspace_id=$1 AND model_id=$2 AND source='catalog')")
        .bind(ws)
        .bind(model)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn status_of(pool: &PgPool, id: &Value) -> String {
    sqlx::query_scalar("SELECT status FROM access_requests WHERE id=$1::uuid")
        .bind(id.as_str().unwrap())
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_member_requests_a_model_and_a_workspace_admin_selects_it(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = eligible_model(&pool, "company/smart").await;
    let (status, req) = ask(&f, &f.member, f.team, m, "model-1").await;
    assert_eq!(status, StatusCode::OK, "{req}");
    assert_eq!(req["target_kind"], "model_selection");
    assert_eq!(req["model_id"], m.to_string());
    assert_eq!(req["model_name_at_request"], "company/smart");
    assert_eq!(req["requested_role"], Value::Null);
    assert_eq!(req["status"], "pending");
    // Replay returns the same request; another payload under the key is 409.
    let (status, again) = ask(&f, &f.member, f.team, m, "model-1").await;
    assert_eq!(
        (status, again["id"].clone()),
        (StatusCode::OK, req["id"].clone())
    );
    let other = eligible_model(&pool, "company/fast").await;
    assert_eq!(
        ask(&f, &f.member, f.team, other, "model-1").await.0,
        StatusCode::CONFLICT
    );
    // One pending request per model: a second key for the same model is 409.
    assert_eq!(
        ask(&f, &f.member, f.team, m, "model-2").await.0,
        StatusCode::CONFLICT
    );
    // The workspace queue shows it to its owner, not to a plain member.
    let (status, queue) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("/api/v1/workspaces/{}/access-requests", f.team),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(queue["data"][0]["model_id"], m.to_string());
    // The requester cannot approve their own request.
    let peer = user(&pool, "peer-admin", "user").await;
    member(&pool, f.team, peer.user_id, "admin").await;
    let (status, own) = ask(&f, &peer, f.team, other, "peer-own").await;
    assert_eq!(status, StatusCode::OK, "{own}");
    assert_eq!(
        decide(&f, &peer, &own["id"], "approve", false).await.0,
        StatusCode::FORBIDDEN
    );
    // The owner approves: the model is selected from the catalog, linked and audited.
    let (status, done) = decide(&f, &f.owner, &req["id"], "approve", false).await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["status"], "approved");
    assert!(selected(&pool, f.team, m).await);
    let linked: Option<Uuid> = sqlx::query_scalar(
        "SELECT model_id FROM access_request_applications WHERE request_id=$1::uuid",
    )
    .bind(req["id"].as_str().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(linked, Some(m));
    let audits: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_events WHERE workspace_id=$1 AND action IN ('model.granted','access_request.applied','access_request.approved') ORDER BY created_at,action")
        .bind(f.team)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(audits.len(), 3, "{audits:?}");
    // Approving again returns the recorded outcome, even after the model is
    // deselected: nothing is selected again.
    sqlx::query("DELETE FROM workspace_model_grants WHERE workspace_id=$1 AND model_id=$2")
        .bind(f.team)
        .bind(m)
        .execute(&pool)
        .await
        .unwrap();
    let (status, again) = decide(&f, &f.owner, &req["id"], "approve", false).await;
    assert_eq!(
        (status, again["status"].clone()),
        (StatusCode::OK, json!("approved"))
    );
    assert!(!selected(&pool, f.team, m).await);
    // The requester sees its own request with the frozen model name.
    let (_, mine) = call(&f.s, &f.member, "GET", MINE, Value::Null).await;
    assert!(
        mine["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["model_name_at_request"] == "company/smart")
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn only_members_request_eligible_unselected_models(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = eligible_model(&pool, "company/smart").await;
    let hidden = model(&pool, "company/not-in-catalog").await;
    // Not a member, a personal workspace, an unknown workspace, an unknown or
    // ineligible model: indistinguishable 404s.
    for (actor, ws, model) in [
        (&f.outsider, f.team, m),
        (&f.owner, f.personal, m),
        (&f.member, Uuid::new_v4(), m),
        (&f.member, f.team, Uuid::new_v4()),
        (&f.member, f.team, hidden),
        (&f.member, f.project, m),
    ] {
        let (status, body) = ask(&f, actor, ws, model, &format!("k-{ws}-{model}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{ws} {model} {body}");
    }
    // A non-member Platform Admin is not a member either.
    assert_eq!(
        ask(&f, &f.admin, f.team, m, "admin").await.0,
        StatusCode::NOT_FOUND
    );
    // A disabled model is not requestable.
    let off = eligible_model(&pool, "company/off").await;
    sqlx::query("UPDATE models SET enabled=false WHERE id=$1")
        .bind(off)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        ask(&f, &f.member, f.team, off, "off").await.0,
        StatusCode::NOT_FOUND
    );
    // Already selected (catalog or direct): 409.
    let chosen = eligible_model(&pool, "company/chosen").await;
    sqlx::query(
        "INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'catalog')",
    )
    .bind(f.team)
    .bind(chosen)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        ask(&f, &f.member, f.team, chosen, "chosen").await.0,
        StatusCode::CONFLICT
    );
    let assigned = model(&pool, "company/assigned").await;
    direct(&pool, f.team, assigned).await;
    assert_eq!(
        ask(&f, &f.member, f.team, assigned, "assigned").await.0,
        StatusCode::CONFLICT
    );
    // Unknown fields are refused; a membership request body still works as before.
    let (status, _) = call(
        &f.s,
        &f.member,
        "POST",
        MINE,
        json!({"workspace_id":f.team,"model_id":m,"idempotency_key":"x","role":"admin"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let created: i64 = sqlx::query_scalar("SELECT count(*) FROM access_requests")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(created, 0);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn approval_needs_workspace_authority_and_rechecks_everything(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = eligible_model(&pool, "company/smart").await;
    let (_, req) = ask(&f, &f.member, f.team, m, "recheck").await;
    // Platform authority alone cannot select a workspace's models: the
    // platform path and a non-member Platform Admin on the workspace path are
    // refused; the auditor and a plain member too.
    assert_eq!(
        decide(&f, &f.admin, &req["id"], "approve", true).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        decide(&f, &f.admin, &req["id"], "deny", true).await.0,
        StatusCode::FORBIDDEN
    );
    for actor in [&f.admin, &f.auditor, &f.outsider] {
        let status = decide(&f, actor, &req["id"], "approve", false).await.0;
        assert!(
            matches!(status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND),
            "{status}"
        );
    }
    let other_member = user(&pool, "other-member", "user").await;
    member(&pool, f.team, other_member.user_id, "member").await;
    assert_eq!(
        decide(&f, &other_member, &req["id"], "approve", false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    // Platform Auditors see it in the platform history.
    let (status, history) = call(
        &f.s,
        &f.auditor,
        "GET",
        "/api/v1/platform/access-requests",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(history["data"][0]["target_kind"], "model_selection");
    assert_eq!(status_of(&pool, &req["id"]).await, "pending");
    // The model leaves the catalog: approval is 409 and the request stays pending.
    sqlx::query("DELETE FROM workspace_type_catalogs WHERE kind='team'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        decide(&f, &f.owner, &req["id"], "approve", false).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(status_of(&pool, &req["id"]).await, "pending");
    assert!(!selected(&pool, f.team, m).await);
    // Back in the catalog, but the requester left the workspace: 409, pending.
    let m2 = eligible_model(&pool, "company/second").await;
    let (_, req2) = ask(&f, &f.member, f.team, m2, "recheck-2").await;
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id=$2").bind(f.team).bind(f.member.user_id).execute(&pool).await.unwrap();
    assert_eq!(
        decide(&f, &f.owner, &req2["id"], "approve", false).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(status_of(&pool, &req2["id"]).await, "pending");
    assert!(!selected(&pool, f.team, m2).await);
    // An admin selected it meanwhile: 409, pending, nothing duplicated.
    member(&pool, f.team, f.member.user_id, "member").await;
    sqlx::query(
        "INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'catalog')",
    )
    .bind(f.team)
    .bind(m2)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        decide(&f, &f.owner, &req2["id"], "approve", false).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(status_of(&pool, &req2["id"]).await, "pending");
    // The owner may deny it on the workspace path.
    let (status, denied_req) = decide(&f, &f.owner, &req2["id"], "deny", false).await;
    assert_eq!(
        (status, denied_req["status"].clone()),
        (StatusCode::OK, json!("denied"))
    );
    // A model disabled after submission: 409.
    let m3 = eligible_model(&pool, "company/third").await;
    let (_, req3) = ask(&f, &f.member, f.team, m3, "recheck-3").await;
    sqlx::query("UPDATE models SET enabled=false WHERE id=$1")
        .bind(m3)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        decide(&f, &f.owner, &req3["id"], "approve", false).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(status_of(&pool, &req3["id"]).await, "pending");
    // Withdrawal works as for membership requests.
    let (status, w) = call(
        &f.s,
        &f.member,
        "POST",
        &format!("{MINE}/{}/withdraw", req3["id"].as_str().unwrap()),
        Value::Null,
    )
    .await;
    assert_eq!(
        (status, w["status"].clone()),
        (StatusCode::OK, json!("withdrawn"))
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn model_requests_are_bound_in_the_database(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = eligible_model(&pool, "company/smart").await;
    // The kind, role and frozen names derive from the target, whatever is supplied.
    let id: Uuid = sqlx::query_scalar("INSERT INTO access_requests(requester,workspace_id,model_id,target_kind,requested_role,model_name_at_request,idempotency_digest,payload_digest) VALUES($1,$2,$3,'workspace_membership','member','forged',decode(repeat('ab',32),'hex'),decode(repeat('cd',32),'hex')) RETURNING id")
        .bind(f.member.user_id).bind(f.team).bind(m).fetch_one(&pool).await.unwrap();
    let row: (String, Option<String>, String) = sqlx::query_as(
        "SELECT target_kind,requested_role,model_name_at_request FROM access_requests WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        ("model_selection".into(), None, "company/smart".into())
    );
    // A non-member's model request is refused by the backstop.
    assert!(sqlx::query("INSERT INTO access_requests(requester,workspace_id,model_id,idempotency_digest,payload_digest) VALUES($1,$2,$3,decode(repeat('ac',32),'hex'),decode(repeat('cd',32),'hex'))")
        .bind(f.outsider.user_id).bind(f.team).bind(m).execute(&pool).await.is_err());
    // Approval without the catalog selection, or naming another model or a
    // membership grant, fails at commit.
    let other = eligible_model(&pool, "company/other").await;
    for (model_id, select) in [(Some(m), false), (Some(other), true)] {
        let mut tx = pool.begin().await.unwrap();
        if select {
            sqlx::query("INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'catalog')").bind(f.team).bind(other).execute(&mut *tx).await.unwrap();
        }
        sqlx::query("INSERT INTO access_request_applications(request_id,workspace_id,requester,model_id) VALUES($1,$2,$3,$4)").bind(id).bind(f.team).bind(f.member.user_id).bind(model_id).execute(&mut *tx).await.unwrap();
        sqlx::query("UPDATE access_requests SET status='approved',decided_by=$2 WHERE id=$1")
            .bind(id)
            .bind(f.owner.user_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(tx.commit().await.is_err(), "{model_id:?} {select}");
    }
    // The control: selected, matching model, commits.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'catalog')",
    )
    .bind(f.team)
    .bind(m)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO access_request_applications(request_id,workspace_id,requester,model_id) VALUES($1,$2,$3,$4)").bind(id).bind(f.team).bind(f.member.user_id).bind(m).execute(&mut *tx).await.unwrap();
    sqlx::query("UPDATE access_requests SET status='approved',decided_by=$2 WHERE id=$1")
        .bind(id)
        .bind(f.owner.user_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // An application naming both targets is refused.
    assert!(sqlx::query("INSERT INTO access_request_applications(request_id,workspace_id,requester,model_id,membership_grant_id) VALUES($1,$2,$3,$4,gen_random_uuid())").bind(id).bind(f.team).bind(f.member.user_id).bind(m).execute(&pool).await.is_err());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_approvals_select_once(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = eligible_model(&pool, "company/smart").await;
    let (_, req) = ask(&f, &f.member, f.team, m, "race").await;
    let second = user(&pool, "second-admin", "user").await;
    member(&pool, f.team, second.user_id, "admin").await;
    let (one, two) = tokio::join!(
        decide(&f, &f.owner, &req["id"], "approve", false),
        decide(&f, &second, &req["id"], "deny", false)
    );
    let outcomes = [one.0, two.0];
    assert!(outcomes.contains(&StatusCode::OK), "{one:?} {two:?}");
    let final_status = status_of(&pool, &req["id"]).await;
    assert_eq!(selected(&pool, f.team, m).await, final_status == "approved");
    let applications: i64 = sqlx::query_scalar("SELECT count(*) FROM access_request_applications")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(applications, i64::from(final_status == "approved"));
}
