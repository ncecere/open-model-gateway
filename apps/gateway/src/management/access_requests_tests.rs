//! Membership requests: disposable real PostgreSQL, no mocked authorization queries.
use super::*;
#[path = "access_requests_review_tests.rs"]
mod review;

/// The Admin workspace detail page reads the toggle it just set (#83 UI slice): GET
/// /platform/workspaces/{ws} reports the live `discoverable` column, defaulting false.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_workspace_detail_reports_discoverable(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, before) = call(
        &f.s,
        &f.admin,
        "GET",
        &format!("/api/v1/platform/workspaces/{}", f.team),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(before["discoverable"], json!(false));
    let (status, set) = call(
        &f.s,
        &f.admin,
        "PUT",
        &format!("/api/v1/platform/workspaces/{}/discoverable", f.team),
        json!({"discoverable": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{set}");
    let (status, after) = call(
        &f.s,
        &f.admin,
        "GET",
        &format!("/api/v1/platform/workspaces/{}", f.team),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(after["discoverable"], json!(true));
}

async fn discover(pool: &PgPool, ws: Uuid) {
    sqlx::query("UPDATE workspaces SET discoverable=true WHERE id=$1")
        .bind(ws)
        .execute(pool)
        .await
        .unwrap();
}
async fn submit(f: &Fixture, actor: &BrowserPrincipal, ws: Uuid, key: &str) -> (StatusCode, Value) {
    call(
        &f.s,
        actor,
        "POST",
        "/api/v1/me/access-requests",
        json!({"workspace_id":ws,"reason":"Please add me","idempotency_key":key}),
    )
    .await
}
fn request_id(v: &Value) -> Uuid {
    v["id"].as_str().unwrap().parse().unwrap()
}
async fn decide(
    f: &Fixture,
    actor: &BrowserPrincipal,
    id: Uuid,
    action: &str,
    platform: bool,
) -> (StatusCode, Value) {
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
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_requests_privacy_idempotency_and_lifecycle(pool: PgPool) {
    let f = fixture(&pool).await;
    let (_, targets) = call(
        &f.s,
        &f.outsider,
        "GET",
        "/api/v1/access-request-targets",
        Value::Null,
    )
    .await;
    assert_eq!(targets["data"], json!([]));
    let mut refusals = Vec::new();
    for ws in [f.team, f.personal, Uuid::new_v4()] {
        refusals.push(submit(&f, &f.outsider, ws, &Uuid::new_v4().to_string()).await);
    }
    assert!(refusals.iter().all(|r| r == &refusals[0]));
    assert_eq!(refusals[0].0, StatusCode::NOT_FOUND);
    discover(&pool, f.team).await;
    let (_, targets) = call(
        &f.s,
        &f.outsider,
        "GET",
        "/api/v1/access-request-targets",
        Value::Null,
    )
    .await;
    assert_eq!(
        targets["data"],
        json!([{"id":f.team,"name":"Team","kind":"team"}])
    );
    let (status, r) = submit(&f, &f.outsider, f.team, "first-request").await;
    assert_eq!(status, StatusCode::OK, "{r}");
    let id = request_id(&r);
    assert_eq!(submit(&f, &f.outsider, f.team, "first-request").await.1, r);
    assert_eq!(
        submit(&f, &f.outsider, f.team, "different-request").await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        submit(&f, &f.outsider, f.project, "first-request").await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &f.s,
            &f.member,
            "GET",
            &format!("/api/v1/me/access-requests/{id}"),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        decide(&f, &f.owner, id, "approve", false).await.0,
        StatusCode::OK
    );
    let grant: Uuid = sqlx::query_scalar(
        "SELECT membership_grant_id FROM access_request_applications WHERE request_id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let role: (String, String) =
        sqlx::query_as("SELECT role,source FROM workspace_membership_grants WHERE id=$1")
            .bind(grant)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(role, ("member".into(), "manual".into()));
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE id=$1")
        .bind(grant)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        decide(&f, &f.owner, id, "approve", false).await.0,
        StatusCode::OK
    );
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM effective_workspace_memberships WHERE workspace_id=$1 AND user_id=$2").bind(f.team).bind(f.outsider.user_id).fetch_one(&pool).await.unwrap(),0);
    assert_eq!(
        call(
            &f.s,
            &f.outsider,
            "POST",
            &format!("/api/v1/me/access-requests/{id}/withdraw"),
            Value::Null
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert!(
        sqlx::query("UPDATE access_requests SET status='pending' WHERE id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .is_err()
    );
    let events: Vec<(String, Value)> = sqlx::query_as(
        "SELECT action,metadata FROM audit_events WHERE resource_id=$1 ORDER BY action",
    )
    .bind(id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(events.len(), 3);
    assert!(
        events
            .iter()
            .all(|(_, v)| !v.to_string().contains("Please"))
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_requests_live_authority_and_self_approval(pool: PgPool) {
    let f = fixture(&pool).await;
    discover(&pool, f.team).await;
    let ws_admin = user(&pool, "request-admin", "user").await;
    member(&pool, f.team, ws_admin.user_id, "admin").await;
    let (_, r) = submit(&f, &f.outsider, f.team, "authority").await;
    let id = request_id(&r);
    for actor in [&f.outsider, &f.member, &f.admin, &f.auditor] {
        assert_eq!(
            decide(&f, actor, id, "approve", false).await.0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        decide(&f, &f.auditor, id, "approve", true).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f.s,
            &f.auditor,
            "GET",
            "/api/v1/platform/access-requests",
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id=$2").bind(f.team).bind(ws_admin.user_id).execute(&pool).await.unwrap();
    assert_eq!(
        decide(&f, &ws_admin, id, "approve", false).await.0,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(f.outsider.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        decide(&f, &f.admin, id, "approve", true).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM access_requests WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "pending"
    );
    let (_, r) = submit(&f, &f.admin, f.team, "self-admin").await;
    let id = request_id(&r);
    assert_eq!(
        decide(&f, &f.admin, id, "approve", true).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        decide(&f, &f.owner, id, "approve", false).await.0,
        StatusCode::OK
    );
    // Independent platform authority works for a non-member; workspace path does not.
    let extra = user(&pool, "new-requester", "user").await;
    let (_, r) = submit(&f, &extra, f.team, "platform-independent").await;
    assert_eq!(
        decide(&f, &f.admin, request_id(&r), "approve", true)
            .await
            .0,
        StatusCode::OK
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_requests_concurrent_decisions_and_stale_membership(pool: PgPool) {
    let f = fixture(&pool).await;
    discover(&pool, f.team).await;
    let (_, r) = submit(&f, &f.outsider, f.team, "race").await;
    let id = request_id(&r);
    let (a, b) = tokio::join!(
        decide(&f, &f.owner, id, "approve", false),
        decide(&f, &f.admin, id, "approve", true)
    );
    assert_eq!((a.0, b.0), (StatusCode::OK, StatusCode::OK));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM access_request_applications WHERE request_id=$1"
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    let extra = user(&pool, "race-withdraw", "user").await;
    let (_, r) = submit(&f, &extra, f.team, "race-withdraw").await;
    let id = request_id(&r);
    let path = format!("/api/v1/me/access-requests/{id}/withdraw");
    let (a, b) = tokio::join!(
        decide(&f, &f.owner, id, "approve", false),
        call(&f.s, &extra, "POST", &path, Value::Null)
    );
    assert!(matches!(
        (a.0, b.0),
        (StatusCode::OK, StatusCode::CONFLICT) | (StatusCode::CONFLICT, StatusCode::OK)
    ));
    let extra = user(&pool, "stale-member", "user").await;
    let (_, r) = submit(&f, &extra, f.team, "stale").await;
    member(&pool, f.team, extra.user_id, "admin").await;
    assert_eq!(
        decide(&f, &f.owner, request_id(&r), "approve", false)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT role FROM effective_workspace_memberships WHERE user_id=$1 AND workspace_id=$2"
        )
        .bind(extra.user_id)
        .bind(f.team)
        .fetch_one(&pool)
        .await
        .unwrap(),
        "admin"
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_requests_database_guards_and_expiry(pool: PgPool) {
    let f = fixture(&pool).await;
    discover(&pool, f.team).await;
    assert!(
        sqlx::query("UPDATE workspaces SET discoverable=true WHERE id=$1")
            .bind(f.personal)
            .execute(&pool)
            .await
            .is_err()
    );
    let (_, r) = submit(&f, &f.outsider, f.team, "expiry").await;
    let id = request_id(&r);
    let duration: i64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM expires_at-created_at)::bigint FROM access_requests WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(duration, 604800);
    assert!(sqlx::query("UPDATE access_requests SET status='approved',decided_by=$2,decided_at=clock_timestamp() WHERE id=$1").bind(id).bind(f.outsider.user_id).execute(&pool).await.is_err());
    // Owner-only fixture clock adjustment: runtime cannot rewrite clocks.
    sqlx::raw_sql("ALTER TABLE access_requests DISABLE TRIGGER access_requests_guard")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE access_requests SET created_at=clock_timestamp()-interval '8 days',expires_at=clock_timestamp()-interval '1 day' WHERE id=$1").bind(id).execute(&pool).await.unwrap();
    sqlx::raw_sql("ALTER TABLE access_requests ENABLE TRIGGER access_requests_guard")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        decide(&f, &f.owner, id, "approve", false).await.0,
        StatusCode::CONFLICT
    );
    let (_, r) = call(
        &f.s,
        &f.outsider,
        "GET",
        &format!("/api/v1/me/access-requests/{id}"),
        Value::Null,
    )
    .await;
    assert_eq!(r["status"], "expired");
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM audit_events WHERE resource_id=$1 AND action='access_request.expired'").bind(id).fetch_one(&pool).await.unwrap(),1);
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_requests_submission_cap_is_replica_safe(pool: PgPool) {
    let f = fixture(&pool).await;
    let mut targets = Vec::new();
    for n in 0..6 {
        let ws = workspace(&pool, &format!("Cap {n}"), "team", f.owner.user_id).await;
        discover(&pool, ws).await;
        targets.push(ws);
    }
    let results = futures::future::join_all(targets.iter().enumerate().map(|(n, ws)| {
        let f = &f;
        async move { submit(f, &f.outsider, *ws, &format!("cap-request-{n}")).await }
    }))
    .await;
    assert_eq!(results.iter().filter(|r| r.0 == StatusCode::OK).count(), 5);
    assert_eq!(
        results
            .iter()
            .filter(|r| r.0 == StatusCode::TOO_MANY_REQUESTS)
            .count(),
        1
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_requests_pending_and_daily_caps(pool: PgPool) {
    let f = fixture(&pool).await;
    let mut targets = Vec::new();
    for n in 0..21 {
        let ws = workspace(&pool, &format!("Pending {n}"), "team", f.owner.user_id).await;
        discover(&pool, ws).await;
        targets.push(ws);
    }
    // Clock fixtures only, as database owner: never available to runtime.
    sqlx::raw_sql("ALTER TABLE access_requests DISABLE TRIGGER access_requests_guard")
        .execute(&pool)
        .await
        .unwrap();
    for (n, ws) in targets.iter().take(20).enumerate() {
        sqlx::query("INSERT INTO access_requests(requester,workspace_id,idempotency_digest,payload_digest,created_at,expires_at) VALUES($1,$2,sha256(convert_to($3,'UTF8')),sha256('payload'::bytea),clock_timestamp()-interval '2 hours',clock_timestamp()+interval '6 days')")
            .bind(f.outsider.user_id).bind(ws).bind(format!("pending-{n}")).execute(&pool).await.unwrap();
    }
    for n in 0..50 {
        sqlx::query("INSERT INTO access_requests(requester,workspace_id,idempotency_digest,payload_digest,status,decided_by,decided_at,created_at,expires_at) VALUES($1,$2,sha256(convert_to($3,'UTF8')),sha256('payload'::bytea),'denied',$4,clock_timestamp()-interval '1 hour',clock_timestamp()-interval '2 hours',clock_timestamp()+interval '6 days')")
            .bind(f.member.user_id).bind(targets[20]).bind(format!("daily-{n}")).bind(f.owner.user_id).execute(&pool).await.unwrap();
    }
    sqlx::raw_sql("ALTER TABLE access_requests ENABLE TRIGGER access_requests_guard")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        submit(&f, &f.outsider, targets[20], "over-pending").await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        submit(&f, &f.member, targets[20], "over-daily").await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_requests_reason_export_cleanup_and_holds(pool: PgPool) {
    let f = fixture(&pool).await;
    discover(&pool, f.team).await;
    let (_, r) = submit(&f, &f.outsider, f.team, "subject-reason").await;
    let id = request_id(&r);
    let (_, export) = call(&f.s, &f.outsider, "GET", "/api/v1/me/export", Value::Null).await;
    assert_eq!(
        export["access_requests"]["items"][0]["reason"],
        "Please add me"
    );
    let hold = Uuid::new_v4();
    sqlx::query("INSERT INTO legal_holds(id,user_id,reference) VALUES($1,$2,'REQUEST-HOLD')")
        .bind(hold)
        .bind(f.outsider.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = crate::db::begin(&pool).await.unwrap();
    crate::lifecycle::lock(&mut tx).await.unwrap();
    crate::lifecycle::lock_users(&mut tx, &[f.outsider.user_id])
        .await
        .unwrap();
    assert!(
        !crate::lifecycle::cleanup_user(&mut tx, f.outsider.user_id)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, Option<String>>("SELECT reason FROM access_requests WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .as_deref(),
        Some("Please add me")
    );
    sqlx::query("UPDATE legal_holds SET released_at=now(),released_by_user_id=$2 WHERE id=$1")
        .bind(hold)
        .bind(f.admin.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = crate::db::begin(&pool).await.unwrap();
    crate::lifecycle::lock(&mut tx).await.unwrap();
    crate::lifecycle::lock_users(&mut tx, &[f.outsider.user_id])
        .await
        .unwrap();
    assert!(
        crate::lifecycle::cleanup_user(&mut tx, f.outsider.user_id)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, Option<String>>("SELECT reason FROM access_requests WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        None
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_requests_denial_withdrawal_and_discovery_revocation(pool: PgPool) {
    let f = fixture(&pool).await;
    discover(&pool, f.team).await;
    let (_, r) = submit(&f, &f.outsider, f.team, "deny-request").await;
    let id = request_id(&r);
    assert_eq!(
        decide(&f, &f.owner, id, "deny", false).await.0,
        StatusCode::OK
    );
    assert_eq!(
        decide(&f, &f.owner, id, "deny", false).await.0,
        StatusCode::OK
    );
    assert_eq!(
        decide(&f, &f.admin, id, "approve", true).await.0,
        StatusCode::CONFLICT
    );
    let (_, r) = submit(&f, &f.outsider, f.team, "withdraw-request").await;
    let id = request_id(&r);
    let path = format!("/api/v1/me/access-requests/{id}/withdraw");
    assert_eq!(
        call(&f.s, &f.outsider, "POST", &path, Value::Null).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f.s, &f.outsider, "POST", &path, Value::Null).await.0,
        StatusCode::OK
    );
    assert_eq!(
        decide(&f, &f.owner, id, "approve", false).await.0,
        StatusCode::CONFLICT
    );
    let (_, r) = submit(&f, &f.outsider, f.team, "target-revoked").await;
    let id = request_id(&r);
    assert_eq!(
        call(
            &f.s,
            &f.member,
            "PUT",
            &format!("/api/v1/platform/workspaces/{}/discoverable", f.team),
            json!({"discoverable":false})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PUT",
            &format!("/api/v1/platform/workspaces/{}/discoverable", f.team),
            json!({"discoverable":false})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        decide(&f, &f.owner, id, "approve", false).await.0,
        StatusCode::CONFLICT
    );
    let (_, record) = call(
        &f.s,
        &f.outsider,
        "GET",
        &format!("/api/v1/me/access-requests/{id}"),
        Value::Null,
    )
    .await;
    assert_eq!(record["status"], "pending");
    assert!(record.get("name").is_none());
    let actions:Vec<String>=sqlx::query_scalar("SELECT action FROM audit_events WHERE action IN ('access_request.denied','access_request.withdrawn') ORDER BY action")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(
        actions,
        vec!["access_request.denied", "access_request.withdrawn"]
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_requests_membership_admins_and_maintenance(pool: PgPool) {
    let f = fixture(&pool).await;
    discover(&pool, f.team).await;
    let ws_admin = user(&pool, "request-live-admin", "user").await;
    member(&pool, f.team, ws_admin.user_id, "admin").await;
    let (_, r) = submit(&f, &f.outsider, f.team, "admin-approve").await;
    assert_eq!(
        decide(&f, &ws_admin, request_id(&r), "approve", false)
            .await
            .0,
        StatusCode::OK
    );
    member(&pool, f.team, f.admin.user_id, "admin").await;
    let requester = user(&pool, "admin-member-requester", "user").await;
    let (_, r) = submit(&f, &requester, f.team, "platform-member").await;
    assert_eq!(
        decide(&f, &f.admin, request_id(&r), "approve", false)
            .await
            .0,
        StatusCode::OK
    );
    let requester = user(&pool, "expire-maintenance", "user").await;
    let (_, r) = submit(&f, &requester, f.team, "maintenance-expire").await;
    let id = request_id(&r);
    sqlx::raw_sql("ALTER TABLE access_requests DISABLE TRIGGER access_requests_guard")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE access_requests SET created_at=clock_timestamp()-interval '8 days',expires_at=clock_timestamp()-interval '1 day' WHERE id=$1").bind(id).execute(&pool).await.unwrap();
    sqlx::raw_sql("ALTER TABLE access_requests ENABLE TRIGGER access_requests_guard")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        crate::lifecycle::expire_access_requests_fenced(&f.s, None)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        crate::lifecycle::expire_access_requests_fenced(&f.s, None)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM access_requests WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "expired"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_requests_served_router_csrf_retry_after_and_token_denial(pool: PgPool) {
    use sha2::{Digest, Sha256};
    let f = fixture(&pool).await;
    discover(&pool, f.team).await;
    let cookie = crate::entropy::hex_token().unwrap();
    let csrf = crate::entropy::hex_token().unwrap();
    sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email,last_seen_at) VALUES($1,$2,$3,now()+interval '1 hour',$4,now())")
        .bind(Sha256::digest(cookie.as_bytes()).as_slice()).bind(f.outsider.user_id).bind(Sha256::digest(csrf.as_bytes()).as_slice()).bind(&f.outsider.email).execute(&pool).await.unwrap();
    let mock = crate::identity::tests::MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let app = router(identity).with_state(f.s.clone());
    let body = json!({"workspace_id":f.team,"reason":"Join","idempotency_key":"served-request"});
    for (origin, send_csrf) in [
        ("https://wrong.example", true),
        ("http://127.0.0.1:3000", false),
    ] {
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/v1/me/access-requests")
            .header("content-type", "application/json")
            .header("origin", origin)
            .header("cookie", format!("omg_session={cookie}"));
        if send_csrf {
            req = req.header("x-csrf-token", &csrf);
        }
        assert_eq!(
            app.clone()
                .oneshot(req.body(Body::from(body.to_string())).unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    for n in 0..6 {
        let ws = workspace(&pool, &format!("Served {n}"), "team", f.owner.user_id).await;
        discover(&pool, ws).await;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/me/access-requests")
                    .header("content-type", "application/json")
                    .header("origin", "http://127.0.0.1:3000")
                    .header("cookie", format!("omg_session={cookie}"))
                    .header("x-csrf-token", &csrf)
                    .body(Body::from(
                        json!({"workspace_id":ws,"idempotency_key":format!("served-{n}")})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if n < 5 {
                StatusCode::OK
            } else {
                StatusCode::TOO_MANY_REQUESTS
            }
        );
        if n == 5 {
            assert!(
                response.headers()["retry-after"]
                    .to_str()
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
                    > 0
            );
        }
    }
    let mut tx = crate::db::begin(&pool).await.unwrap();
    locks::exclusive(&mut tx, [locks::Scope::User(f.admin.user_id)])
        .await
        .unwrap();
    let token = crate::admin_tokens::create_admin_token(
        &mut tx,
        f.admin.user_id,
        "request probe",
        &["platform:read".to_owned(), "access:write".to_owned()],
        30,
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    for route in crate::route_policy::ROUTES
        .iter()
        .filter(|r| r.path.contains("access-request") || r.path.ends_with("/discoverable"))
    {
        assert_eq!(route.token_policy, crate::route_policy::TokenPolicy::Denied);
        for method in route.methods.split_whitespace() {
            let path = route
                .path
                .replace("{ws}", &f.team.to_string())
                .replace("{id}", &Uuid::new_v4().to_string());
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("authorization", format!("Bearer {}", token.token))
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{method} {}",
                route.path
            );
        }
    }
}
