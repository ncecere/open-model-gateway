//! #79: "Your sessions" and the admin revoke, on PostgreSQL.
use super::*;
use crate::identity::CurrentSession;

/// A live session row for `u`; returns its public ID.
async fn session(pool: &PgPool, u: &BrowserPrincipal, label: Option<&str>) -> Uuid {
    sqlx::query_scalar("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email,client_label,last_seen_at) VALUES(sha256(gen_random_uuid()::text::bytea),$1,sha256(gen_random_uuid()::text::bytea),now()+interval '12 hours',$2,$3,now()) RETURNING id")
        .bind(u.user_id)
        .bind(&u.email)
        .bind(label)
        .fetch_one(pool)
        .await
        .unwrap()
}
/// `call`, as a request arriving on session `current`.
async fn call_on(
    s: &Store,
    u: &BrowserPrincipal,
    current: Uuid,
    method: &str,
    path: &str,
) -> (StatusCode, Value) {
    let response = routes()
        .layer(Extension(u.clone()))
        .layer(Extension(CurrentSession(current)))
        .with_state(s.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
async fn live(pool: &PgPool, id: Uuid) -> bool {
    sqlx::query_scalar("SELECT revoked_at IS NULL FROM browser_sessions WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn audits(pool: &PgPool, action: &str) -> Vec<(Option<Uuid>, Value)> {
    sqlx::query_as(
        "SELECT resource_id,metadata FROM audit_events WHERE action=$1 ORDER BY created_at,id",
    )
    .bind(action)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_user_lists_only_their_own_live_sessions(pool: PgPool) {
    let f = fixture(&pool).await;
    let here = session(&pool, &f.owner, Some("Firefox on macOS")).await;
    let phone = session(&pool, &f.owner, Some("Safari on iOS")).await;
    let ended = session(&pool, &f.owner, None).await;
    let expired = session(&pool, &f.owner, None).await;
    sqlx::query("UPDATE browser_sessions SET revoked_at=now() WHERE id=$1")
        .bind(ended)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE browser_sessions SET expires_at=now()-interval '1 second' WHERE id=$1")
        .bind(expired)
        .execute(&pool)
        .await
        .unwrap();
    let theirs = session(&pool, &f.outsider, Some("Chrome on Windows")).await;
    let (status, body) = call_on(&f.s, &f.owner, here, "GET", "/api/v1/me/sessions").await;
    assert_eq!(status, StatusCode::OK);
    let rows = body["data"].as_array().unwrap();
    let ids: Vec<_> = rows
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ids,
        vec![here.to_string(), phone.to_string()],
        "current first; no ended, expired or other user's"
    );
    assert_eq!(rows[0]["current"], true);
    assert_eq!(rows[1]["current"], false);
    assert_eq!(rows[1]["client"], "Safari on iOS");
    // Public fields only: never the token or CSRF hash, the email or an address.
    let mut keys: Vec<_> = rows[0].as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "client",
            "created_at",
            "current",
            "expires_at",
            "id",
            "last_seen_at"
        ]
    );
    assert!(!body.to_string().contains(&theirs.to_string()));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn revoking_one_session_is_limited_to_your_own(pool: PgPool) {
    let f = fixture(&pool).await;
    let (here, laptop) = (
        session(&pool, &f.owner, None).await,
        session(&pool, &f.owner, None).await,
    );
    let theirs = session(&pool, &f.outsider, None).await;
    let path = |id: Uuid| format!("/api/v1/me/sessions/{id}");
    let keys_revoked = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM api_keys WHERE revoked_at IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let keys_before = keys_revoked().await;
    assert_eq!(
        call_on(&f.s, &f.owner, here, "DELETE", &path(laptop)).await,
        (StatusCode::OK, json!({"revoked":1}))
    );
    assert!(!live(&pool, laptop).await && live(&pool, here).await);
    // Already ended, another user's, or unknown: 404, and nothing changes.
    for id in [laptop, theirs, Uuid::new_v4()] {
        assert_eq!(
            call_on(&f.s, &f.owner, here, "DELETE", &path(id)).await.0,
            StatusCode::NOT_FOUND
        );
    }
    assert!(
        live(&pool, theirs).await,
        "another user's session is untouched"
    );
    // The current one too: that is signing this browser out.
    assert_eq!(
        call_on(&f.s, &f.owner, here, "DELETE", &path(here)).await.0,
        StatusCode::OK
    );
    assert!(!live(&pool, here).await);
    assert_eq!(
        keys_revoked().await,
        keys_before,
        "sessions only: inference keys are separate"
    );
    assert_eq!(
        audits(&pool, "session.revoked").await,
        vec![
            (Some(f.owner.user_id), json!({"count":1})),
            (Some(f.owner.user_id), json!({"count":1}))
        ]
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn signing_out_other_sessions_keeps_this_one(pool: PgPool) {
    let f = fixture(&pool).await;
    let here = session(&pool, &f.owner, None).await;
    let others = [
        session(&pool, &f.owner, None).await,
        session(&pool, &f.owner, None).await,
    ];
    let theirs = session(&pool, &f.outsider, None).await;
    let path = "/api/v1/me/sessions/revoke-others";
    assert_eq!(
        call_on(&f.s, &f.owner, here, "POST", path).await,
        (StatusCode::OK, json!({"revoked":2}))
    );
    assert!(live(&pool, here).await && live(&pool, theirs).await);
    for id in others {
        assert!(!live(&pool, id).await);
    }
    // Nothing left to sign out: no audit row for a no-op.
    assert_eq!(
        call_on(&f.s, &f.owner, here, "POST", path).await,
        (StatusCode::OK, json!({"revoked":0}))
    );
    assert_eq!(
        audits(&pool, "session.others_revoked").await,
        vec![(Some(f.owner.user_id), json!({"count":2}))]
    );
    // Without a known current session it refuses instead of signing out everything.
    assert_eq!(
        call(&f.s, &f.owner, "POST", path, json!({})).await.0,
        StatusCode::BAD_REQUEST
    );
    assert!(live(&pool, here).await);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_revocations_do_not_conflict(pool: PgPool) {
    let f = fixture(&pool).await;
    let here = session(&pool, &f.owner, None).await;
    let mut others = Vec::new();
    for _ in 0..5 {
        others.push(session(&pool, &f.owner, None).await);
    }
    let s = &f.s;
    let owner = &f.owner;
    let paths: Vec<String> = others
        .iter()
        .map(|id| format!("/api/v1/me/sessions/{id}"))
        .collect();
    let calls: Vec<_> = paths
        .iter()
        .map(|path| call_on(s, owner, here, "DELETE", path))
        .collect();
    let deletes = futures::future::join_all(calls);
    let (deletes, sweep) = tokio::join!(
        deletes,
        call_on(s, owner, here, "POST", "/api/v1/me/sessions/revoke-others")
    );
    let deleted: u64 = deletes
        .iter()
        .map(|(status, body)| {
            assert!(
                matches!(*status, StatusCode::OK | StatusCode::NOT_FOUND),
                "{status}"
            );
            body["revoked"].as_u64().unwrap_or(0)
        })
        .sum();
    assert_eq!(sweep.0, StatusCode::OK);
    assert_eq!(
        deleted + sweep.1["revoked"].as_u64().unwrap(),
        5,
        "each session revoked exactly once"
    );
    assert!(live(&pool, here).await);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_platform_admin_revokes_a_persons_sessions_and_sees_only_a_count(pool: PgPool) {
    let f = fixture(&pool).await;
    for _ in 0..2 {
        session(&pool, &f.outsider, Some("Chrome on Windows")).await;
    }
    let mine = session(&pool, &f.owner, None).await;
    let detail = format!("/api/v1/platform/users/{}", f.outsider.user_id);
    let (_, user) = call(&f.s, &f.admin, "GET", &detail, json!({})).await;
    assert_eq!(user["active_sessions"], 2);
    assert!(
        !user.to_string().contains("Chrome on Windows"),
        "no device details for admins"
    );
    let path = format!(
        "/api/v1/platform/users/{}/sessions/revoke",
        f.outsider.user_id
    );
    for actor in [&f.auditor, &f.member, &f.outsider] {
        assert_eq!(
            call(&f.s, actor, "POST", &path, json!({})).await.0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        call(&f.s, &f.admin, "POST", &path, json!({})).await,
        (StatusCode::OK, json!({"revoked":2}))
    );
    assert_eq!(
        call(&f.s, &f.admin, "GET", &detail, json!({})).await.1["active_sessions"],
        0
    );
    assert!(live(&pool, mine).await, "only that person's sessions");
    // The user keeps their account: a fresh sign-in works (not a suspension).
    let disabled: bool =
        sqlx::query_scalar("SELECT disabled_at IS NOT NULL FROM users WHERE id=$1")
            .bind(f.outsider.user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!disabled);
    assert_eq!(
        audits(&pool, "user.sessions_revoked").await,
        vec![(Some(f.outsider.user_id), json!({"count":2}))]
    );
    let unknown = format!("/api/v1/platform/users/{}/sessions/revoke", Uuid::new_v4());
    assert_eq!(
        call(&f.s, &f.admin, "POST", &unknown, json!({})).await.0,
        StatusCode::NOT_FOUND
    );
}
