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

/// Like [`mint`], but for the two new real-credential tests below: returns
/// the one-time plaintext token too, so they authenticate through the real
/// `authenticate_management` middleware with a genuine `Authorization:
/// Bearer` header instead of an injected `Extension<ManagementPrincipal>`.
async fn mint_with_secret(pool: &PgPool, owner: Uuid, scopes: &[&str]) -> (Uuid, String) {
    let mut tx = crate::db::begin(pool).await.unwrap();
    locks::exclusive(&mut tx, [locks::Scope::User(owner)])
        .await
        .unwrap();
    let scopes: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
    let created = admin_tokens::create_admin_token(&mut tx, owner, "t", &scopes, 30, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (created.id, created.token)
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

/// Which `pg_locks` wait this test expects to find the waiter blocked on
/// (#37 PR2 review: the previous `wait_until_blocked_by` accepted *any*
/// backend blocked by *any* other backend, so it could not actually tell
/// a token-row wait from an unrelated advisory wait, or even from a
/// different test's leftover connection -- a guard could be deleted
/// entirely and this helper would still report "blocked" as long as
/// something, anything, was waiting on the holder for any reason).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExpectedWait {
    /// A plain row lock conflict on `admin_api_tokens` (the token's own
    /// `FOR SHARE`, blocking the explicit revoke's `UPDATE`): PostgreSQL
    /// reports a waiting `UPDATE` as blocked with `pg_locks.locktype`
    /// `transactionid` (waiting on the holder's xid) once the tuple-level
    /// conflict resolves to "wait for this transaction", so both
    /// `transactionid` and `tuple` are accepted here depending on the
    /// exact instant polled.
    TokenRow,
    /// `governance::locks::shared`/`exclusive`
    /// (`pg_advisory_xact_lock[_shared]`): `locktype` `advisory`.
    Authority,
}
impl ExpectedWait {
    fn locktypes(self) -> &'static [&'static str] {
        match self {
            Self::TokenRow => &["transactionid", "tuple"],
            Self::Authority => &["advisory"],
        }
    }
}

/// Polls (via a separate probe connection, never the two transactions under
/// test) until `waiter` -- an exact backend pid, captured from *inside*
/// that backend's own transaction before it ever issued the blocking
/// statement -- is reported blocked specifically by `blocker`
/// (`pg_blocking_pids`), with `wait_event_type='Lock'` and an actual
/// `pg_locks` wait row of exactly `expected`'s kind. Panics with the
/// mismatched row (if any) after a bounded 5 s timeout; only the poll
/// *interval* sleeps, never anything that decides ordering.
async fn wait_until_blocked_by(probe: &PgPool, waiter: i32, blocker: i32, expected: ExpectedWait) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        // `wait_event_type` is nullable (not waiting at all is a normal,
        // frequent transient state while polling), so it is filtered in
        // SQL rather than decoded as a non-null `String`: a snapshot taken
        // mid-transition (blocked-pid set already updated, wait state not
        // yet) must never panic the probe, only make this iteration find
        // nothing and poll again.
        let locktype: Option<String> = sqlx::query_scalar(
            "SELECT l.locktype FROM pg_stat_activity a \
             JOIN pg_locks l ON l.pid=a.pid AND NOT l.granted \
             WHERE a.pid=$1 AND a.wait_event_type='Lock' AND $2=ANY(pg_blocking_pids(a.pid)) LIMIT 1",
        )
        .bind(waiter)
        .bind(blocker)
        .fetch_optional(probe)
        .await
        .unwrap();
        if locktype
            .as_deref()
            .is_some_and(|l| expected.locktypes().contains(&l))
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "backend {waiter} never showed the expected {expected:?} wait on blocker {blocker} within 5s (last seen locktype: {locktype:?})"
            );
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
    let (pid_tx, pid_rx) = tokio::sync::oneshot::channel();
    let revoke_task = tokio::spawn(async move {
        let mut tx_b = crate::db::begin(&pool_b).await.unwrap();
        // Captured from inside B's own transaction, before it ever issues
        // the blocking `UPDATE` below, so the probe polls for exactly this
        // backend rather than "any blocked backend" (#37 PR2 review).
        let pid_b = backend_pid(&mut tx_b).await;
        let _ = pid_tx.send(pid_b);
        admin_tokens::revoke_admin_token(&mut tx_b, token_id)
            .await
            .unwrap();
        tx_b.commit().await.unwrap();
    });

    let pid_b = pid_rx.await.unwrap();
    wait_until_blocked_by(&pool, pid_b, pid_a, ExpectedWait::TokenRow).await;
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
    let (pid_tx, pid_rx) = tokio::sync::oneshot::channel();
    let demote_task = tokio::spawn(async move {
        let mut tx_b = crate::db::begin(&pool_b).await.unwrap();
        // Captured before the blocking advisory-lock acquisition below.
        let pid_b = backend_pid(&mut tx_b).await;
        let _ = pid_tx.send(pid_b);
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

    let pid_b = pid_rx.await.unwrap();
    wait_until_blocked_by(&pool, pid_b, pid_a, ExpectedWait::Authority).await;
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

/// A real Bearer-token request against the full served router
/// (`management::router`, real `authenticate_management` +
/// `require_route_access` middleware) -- never an injected principal --
/// so these two new tests exercise genuine credential authentication, not
/// just the handler-level helpers the tests above target.
async fn bearer_delete(app: &Router, path: &str, token: &str) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// All `role.revoked` audit rows for `target`: actor, token and role.
/// Fetch every row so exact-one assertions also reject duplicate auditing.
async fn role_revoked_audit(pool: &PgPool, target: Uuid) -> Vec<(Uuid, Option<Uuid>, String)> {
    sqlx::query_as(
        "SELECT actor_user_id,admin_token_id,metadata->>'role' FROM audit_events \
         WHERE action='role.revoked' AND resource_type='user' AND resource_id=$1 \
         ORDER BY created_at DESC",
    )
    .bind(target)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn is_live_admin(pool: &PgPool, user_id: Uuid) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM effective_platform_roles WHERE user_id=$1 AND role='admin')",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn bounded_bearer_delete(app: &Router, path: &str, token: &str) -> StatusCode {
    tokio::time::timeout(Duration::from_secs(5), bearer_delete(app, path, token))
        .await
        .expect("request never completed within 5s")
}

/// Self-demotion through the real router with the actor's own real token
/// (minted via the secret helper, never an injected principal): a
/// currently effective Admin may revoke their own 'admin' grant (another
/// live Admin exists, so `protect_admin`'s last-administrator guard is
/// not the reason this could fail). The request itself commits, is
/// correctly audited as the token's own action against the demoted
/// target, and migration 0051's deferred trigger then revokes the very
/// token that authenticated it -- proven not just by the database row,
/// but by a second real request with that same (now-dead) token failing
/// authentication (401), exactly as `authz_admin_token_tests.rs` asserts
/// for every other revoked-token case. Both requests are bounded by a
/// timeout, like every other concurrency assertion in this file.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn self_demotion_via_own_token_commits_then_the_token_is_revoked_next_request_401(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    // A second live Admin: self-demotion must not be refused as "the last
    // platform administrator".
    let _safety_admin = user(&pool, "safety_admin", "admin").await;
    let (token_id, token) = mint_with_secret(&pool, f.admin.user_id, &["access:write"]).await;

    let identity = IdentityState::new(f.s.clone(), None).await.unwrap();
    let app = router(identity).with_state(f.s.clone());
    let path = format!("/api/v1/platform/users/{}/roles/admin", f.admin.user_id);

    let first = bounded_bearer_delete(&app, &path, &token).await;
    assert_eq!(
        first,
        StatusCode::OK,
        "self-demotion must commit: another Admin remains"
    );
    assert!(
        token_revoked(&pool, token_id).await,
        "losing one's own last manual admin grant must revoke the authenticating token too (migration 0051)"
    );
    assert!(
        !is_live_admin(&pool, f.admin.user_id).await,
        "the demoted actor must no longer be a currently effective Admin"
    );
    let audit = role_revoked_audit(&pool, f.admin.user_id).await;
    assert_eq!(
        audit,
        vec![(f.admin.user_id, Some(token_id), "admin".to_string())],
        "the audit row must attribute this self-demotion to the actor's own token, against the actor as its own target"
    );
    let second = bounded_bearer_delete(&app, &path, &token).await;
    assert_eq!(
        second,
        StatusCode::UNAUTHORIZED,
        "the same now-revoked token must fail a subsequent real request's authentication, not just the database row"
    );
}

/// Required behaviour: cross-admin *role* demotion (the platform-role
/// counterpart of `cross_admin_membership_changes_via_tokens_do_not_deadlock`'s
/// workspace-membership case) through the full router with two real
/// tokens, each demoting the other's 'admin' grant simultaneously. This
/// handler (`directory::revoke_role`) serializes every management write on
/// the singleton installation row (`resources::catalog_tx`), so the two
/// requests cannot deadlock by construction. Both requests authenticate
/// before either transaction necessarily commits (middleware preflight is
/// a lock-free snapshot), so the loser may be refused either way: 401 if
/// its own request's later re-authentication already sees its token
/// revoked, or 403 if the handler's own transaction-level revalidation
/// (`resources::platform_write`) catches the loss once its write runs --
/// either is a correct refusal; neither specific one is what this test
/// depends on. What must hold regardless of which: exactly one of the two
/// owners loses Admin and exactly that owner's own token is revoked (the
/// survivor stays Admin with a live token), and exactly one
/// token-attributed `role.revoked` audit row exists, correctly
/// attributing the winner's token against the loser as its target.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn cross_admin_role_demotion_via_tokens_is_bounded_and_the_loser_is_refused(pool: PgPool) {
    let f = fixture(&pool).await;
    let admin_b = user(&pool, "admin_b", "admin").await;
    // A third live Admin untouched by either request: `protect_admin`'s
    // last-administrator guard must never be the reason either request is
    // refused here, so this test observes only the race this is about.
    let _safety_admin = user(&pool, "safety_admin", "admin").await;
    let (token_a_id, token_a) = mint_with_secret(&pool, f.admin.user_id, &["access:write"]).await;
    let (token_b_id, token_b) = mint_with_secret(&pool, admin_b.user_id, &["access:write"]).await;

    let identity = IdentityState::new(f.s.clone(), None).await.unwrap();
    let app = router(identity).with_state(f.s.clone());
    let a_demotes_b = format!("/api/v1/platform/users/{}/roles/admin", admin_b.user_id);
    let b_demotes_a = format!("/api/v1/platform/users/{}/roles/admin", f.admin.user_id);

    let run = async {
        tokio::join!(
            bearer_delete(&app, &a_demotes_b, &token_a),
            bearer_delete(&app, &b_demotes_a, &token_b),
        )
    };
    let (r1, r2) = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("cross-admin role demotion deadlocked or hung");

    let outcomes = [r1, r2];
    let successes = outcomes.iter().filter(|s| **s == StatusCode::OK).count();
    let refusals = outcomes
        .iter()
        .filter(|s| matches!(**s, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN))
        .count();
    assert_eq!(
        successes, 1,
        "exactly one of the two concurrent demotions should commit; got {outcomes:?}"
    );
    assert_eq!(
        refusals, 1,
        "the loser must be refused once its own owner lost admin -- not succeed with a dead credential; got {outcomes:?}"
    );

    // Request status counts alone cannot tell *which* side actually lost,
    // or whether the database state is self-consistent; derive it and
    // assert the real outcome (role, token liveness, audit attribution).
    let (winner, winner_token, loser, loser_token) = if r1 == StatusCode::OK {
        (f.admin.user_id, token_a_id, admin_b.user_id, token_b_id)
    } else {
        (admin_b.user_id, token_b_id, f.admin.user_id, token_a_id)
    };
    assert!(
        is_live_admin(&pool, winner).await,
        "the winner must still be a currently effective Admin"
    );
    assert!(
        !token_revoked(&pool, winner_token).await,
        "the winner's own token must still be live"
    );
    assert!(
        !is_live_admin(&pool, loser).await,
        "the loser must have actually lost Admin, not merely received a refused response"
    );
    assert!(
        token_revoked(&pool, loser_token).await,
        "losing Admin must have revoked the loser's own token (migration 0051)"
    );
    let audit = role_revoked_audit(&pool, loser).await;
    assert_eq!(
        audit,
        vec![(winner, Some(winner_token), "admin".to_string())],
        "exactly one token-attributed role.revoked audit row, correctly attributing the winner's token against the loser as its target"
    );
}
