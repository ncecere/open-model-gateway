//! #37 PR2 item 1: transaction-level admin-token and owner revalidation.
//!
//! Deterministic two-connection concurrency tests: no sleep is ever used to
//! *decide* ordering. Each blocking assertion polls `pg_stat_activity` /
//! `pg_blocking_pids` for the real wait state, bounded by a timeout, and
//! only the poll *interval* uses a short sleep. These exercise the
//! transaction-level revalidation `resources::platform_write`/
//! `revalidate_admin_token` must perform (middleware preflight is a
//! lock-free snapshot taken before the handler's own transaction, and is
//! therefore not enough on its own).
use super::*;
use crate::{admin_tokens, governance::locks, identity::AdminTokenContext};
use std::time::Duration;

async fn mint(pool: &PgPool, owner: Uuid, scopes: &[&str]) -> Uuid {
    let mut tx = crate::db::begin(pool).await.unwrap();
    locks::exclusive(&mut tx, [locks::Scope::User(owner)])
        .await
        .unwrap();
    let scopes: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
    let created = admin_tokens::create_admin_token(&mut tx, owner, "t", &scopes, 30, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    created.id
}

fn token_principal(token_id: Uuid, owner: Uuid, scopes: &[&str]) -> ManagementPrincipal {
    ManagementPrincipal::AdminToken(AdminTokenContext {
        token_id,
        owner_user_id: owner,
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
        ip_allowlist: None,
        client_address: None,
    })
}

async fn backend_pid(tx: &mut Transaction<'_, Postgres>) -> i32 {
    sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut **tx)
        .await
        .unwrap()
}

/// Polls (via a separate pool connection, never the two transactions under
/// test) until some other backend is blocked waiting on a lock `blocker`
/// holds, or panics after a bounded 5 s timeout. Works for both row locks
/// (`FOR SHARE`) and advisory locks: `pg_blocking_pids` reports both.
async fn wait_until_blocked_by(probe: &PgPool, blocker: i32) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let found: Option<i32> = sqlx::query_scalar(
            "SELECT pid FROM pg_stat_activity WHERE pid<>$1 AND $1=ANY(pg_blocking_pids(pid)) LIMIT 1",
        )
        .bind(blocker)
        .fetch_optional(probe)
        .await
        .unwrap();
        if found.is_some() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("backend {blocker} never blocked a concurrent transaction within 5s");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn call_token(
    s: &Store,
    principal: &ManagementPrincipal,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = routes()
        .layer(Extension(principal.clone()))
        .with_state(s.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
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

async fn token_revoked(pool: &PgPool, token_id: Uuid) -> bool {
    sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM admin_api_tokens WHERE id=$1")
        .bind(token_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Required behaviour: "token revoked ... after preflight but before
/// handler transaction => no write." A token already revoked and committed
/// before the handler's own transaction starts must fail transaction-level
/// revalidation -- middleware preflight alone cannot see this.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn revoked_token_fails_handler_revalidation_once_committed(pool: PgPool) {
    let f = fixture(&pool).await;
    let token_id = mint(&pool, f.admin.user_id, &["catalog:write"]).await;
    let mut revoke_tx = crate::db::begin(&pool).await.unwrap();
    assert!(
        admin_tokens::revoke_admin_token(&mut revoke_tx, token_id)
            .await
            .unwrap()
    );
    revoke_tx.commit().await.unwrap();

    let principal = token_principal(token_id, f.admin.user_id, &["catalog:write"]);
    let mut tx = crate::db::begin(&pool).await.unwrap();
    let result = resources::platform_write(&mut tx, &principal).await;
    assert!(
        result.is_err(),
        "a revoked token must fail handler-transaction revalidation, not just middleware preflight"
    );
}

/// Required behaviour: "owner demoted after preflight but before handler
/// transaction => no write," in the ordering where the demotion already
/// committed. Migration 0051's deferred trigger also revokes the token.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn owner_demotion_committed_before_the_check_fails_the_mutation(pool: PgPool) {
    let f = fixture(&pool).await;
    let token_id = mint(&pool, f.admin.user_id, &["catalog:write"]).await;
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin' AND revoked_at IS NULL",
    )
    .bind(f.admin.user_id)
    .execute(&pool)
    .await
    .unwrap();

    let principal = token_principal(token_id, f.admin.user_id, &["catalog:write"]);
    let mut tx = crate::db::begin(&pool).await.unwrap();
    let result = resources::platform_write(&mut tx, &principal).await;
    assert!(result.is_err());
    assert!(
        token_revoked(&pool, token_id).await,
        "losing the owner's last admin grant must have revoked the token too (migration 0051)"
    );
}

/// Required behaviour: "authorized mutation holding appropriate authority
/// locks may finish BEFORE revocation commits, otherwise fails." A handler
/// transaction that already passed revalidation (holding the token's `FOR
/// SHARE` row lock taken by `revalidate_admin_token`) must let a
/// concurrent, in-flight explicit revoke block until it commits, then
/// finish and commit *before* the revoke can.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_mutation_holding_its_locks_finishes_before_a_concurrent_explicit_revoke_commits(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let token_id = mint(&pool, f.admin.user_id, &["catalog:write"]).await;
    let principal = token_principal(token_id, f.admin.user_id, &["catalog:write"]);

    let mut tx_a = crate::db::begin(&pool).await.unwrap();
    resources::platform_write(&mut tx_a, &principal)
        .await
        .unwrap();
    let pid_a = backend_pid(&mut tx_a).await;
    // The "write" this handler transaction performs while still holding its
    // revalidation locks: a marker any later reader can see only once A commits.
    sqlx::query("UPDATE users SET display_name='mutated-by-a' WHERE id=$1")
        .bind(f.admin.user_id)
        .execute(&mut *tx_a)
        .await
        .unwrap();

    let pool_b = pool.clone();
    let revoke_task = tokio::spawn(async move {
        let mut tx_b = crate::db::begin(&pool_b).await.unwrap();
        admin_tokens::revoke_admin_token(&mut tx_b, token_id)
            .await
            .unwrap();
        tx_b.commit().await.unwrap();
    });

    wait_until_blocked_by(&pool, pid_a).await;
    // A finishes (commits) first, by construction: the revoke could not
    // have proceeded past this point while blocked.
    tx_a.commit().await.unwrap();
    revoke_task.await.unwrap();

    assert!(
        token_revoked(&pool, token_id).await,
        "the revoke must still complete once A released its lock"
    );
    let name: String = sqlx::query_scalar("SELECT display_name FROM users WHERE id=$1")
        .bind(f.admin.user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        name, "mutated-by-a",
        "A's write committed; it was not rolled back by the later revoke"
    );
}

/// Required behaviour, the owner-demotion counterpart of the previous test:
/// a demotion (which takes the same owner user-scope lock exclusive,
/// migration 0027/0051) blocks on a handler transaction already holding it
/// shared, and only proceeds -- revoking the token -- after that handler
/// transaction commits.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_mutation_holding_its_locks_finishes_before_a_concurrent_demotion_commits(pool: PgPool) {
    let f = fixture(&pool).await;
    let token_id = mint(&pool, f.admin.user_id, &["catalog:write"]).await;
    let principal = token_principal(token_id, f.admin.user_id, &["catalog:write"]);

    let mut tx_a = crate::db::begin(&pool).await.unwrap();
    resources::platform_write(&mut tx_a, &principal)
        .await
        .unwrap();
    let pid_a = backend_pid(&mut tx_a).await;

    let pool_b = pool.clone();
    let owner = f.admin.user_id;
    let demote_task = tokio::spawn(async move {
        let mut tx_b = crate::db::begin(&pool_b).await.unwrap();
        locks::exclusive(&mut tx_b, [locks::Scope::User(owner)])
            .await
            .unwrap();
        sqlx::query(
            "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin' AND revoked_at IS NULL",
        )
        .bind(owner)
        .execute(&mut *tx_b)
        .await
        .unwrap();
        tx_b.commit().await.unwrap();
    });

    wait_until_blocked_by(&pool, pid_a).await;
    tx_a.commit().await.unwrap();
    demote_task.await.unwrap();

    assert!(
        token_revoked(&pool, token_id).await,
        "the demotion's deferred trigger must have revoked the token once A finished"
    );
}

/// Required behaviour: "Include self-demotion and cross-admin operations
/// ... avoid shared->exclusive upgrade deadlocks, opposing owner/target
/// user order." Two admins managing each other's membership through their
/// own admin tokens, concurrently, must not deadlock: each handler merges
/// its own owner's user-scope lock into the same `locks::exclusive` call as
/// the target member's user-scope lock (ascending canonical key order, one
/// mode, one round trip per transaction), never a separate earlier shared
/// acquisition that could be requested in the opposite order by the other
/// transaction.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn cross_admin_membership_changes_via_tokens_do_not_deadlock(pool: PgPool) {
    let f = fixture(&pool).await;
    let admin_b = user(&pool, "admin_b", "admin").await;
    let token_a = mint(&pool, f.admin.user_id, &["access:write"]).await;
    let token_b = mint(&pool, admin_b.user_id, &["access:write"]).await;
    let principal_a = token_principal(token_a, f.admin.user_id, &["access:write"]);
    let principal_b = token_principal(token_b, admin_b.user_id, &["access:write"]);
    let team = f.team;
    let target_b = admin_b.user_id;
    let target_a = f.admin.user_id;

    let path = format!("/api/v1/platform/workspaces/{team}/members");
    let run = async {
        tokio::join!(
            call_token(
                &f.s,
                &principal_a,
                "POST",
                &path,
                json!({"user_id": target_b, "role": "member"}),
            ),
            call_token(
                &f.s,
                &principal_b,
                "POST",
                &path,
                json!({"user_id": target_a, "role": "member"}),
            ),
        )
    };
    let (r1, r2) = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("cross-admin membership changes deadlocked or hung");
    assert_eq!(r1.0, StatusCode::OK, "{:?}", r1.1);
    assert_eq!(r2.0, StatusCode::OK, "{:?}", r2.1);
}
