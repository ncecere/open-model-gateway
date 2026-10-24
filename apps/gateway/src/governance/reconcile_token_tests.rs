//! #37 PR2 coordinator review (blocking): `resolve_usage`'s caller
//! authenticates a request's credential (browser session or admin token) in
//! its own transaction, which commits before `resolve_usage` ever opens its
//! own. An admin token revoked, or expired, between those two transactions
//! must still refuse the write here -- the *second* transaction -- not
//! merely at the first (already-committed) authentication. These tests
//! exercise the real `governance::resolve_usage` path end to end (not just
//! the `management::resources` helper it now shares), per the coordinator's
//! explicit instruction not to rely solely on that helper's own test
//! coverage to stand in for this one.
use super::*;
use crate::{
    admin_tokens,
    governance::{
        locks,
        tests::db::{amounts, done, fixture, request},
    },
};
use sha2::{Digest, Sha256};
use std::time::Duration;

async fn mint(pool: &sqlx::PgPool, owner: Uuid) -> Uuid {
    let mut tx = crate::db::begin(pool).await.unwrap();
    locks::exclusive(&mut tx, [locks::Scope::User(owner)])
        .await
        .unwrap();
    let created = admin_tokens::create_admin_token(
        &mut tx,
        owner,
        "reconcile-test",
        &["governance:write".to_string()],
        30,
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    created.id
}

async fn reconciled_count(pool: &sqlx::PgPool, execution: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='usage.reconciled' AND resource_id=$1",
    )
    .bind(execution)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The blocking finding itself: an admin token revoked after the caller's
/// own authentication transaction committed, but before `resolve_usage`'s
/// write transaction, must refuse -- not silently reconcile with a
/// now-dead credential's authority. No deterministic DB-side wait is
/// needed to reproduce this: the revoke is fully committed before
/// `resolve_usage` is even called, exactly reproducing "revoked between
/// tx1's commit and tx2" with no timing dependency at all.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn revoked_token_between_authentication_and_the_write_transaction_is_refused(
    pool: sqlx::PgPool,
) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    let start = f.start();
    admit(&f.store, &start, &request(), 30).await.unwrap();
    finish(&f.store, &done(start.id, None, None)).await.unwrap();

    let token_id = mint(&pool, f.owner).await;
    // The caller's own (now-committed) authentication already resolved this
    // identity; simulate the revoke landing in the gap before the second
    // transaction.
    let mut revoke_tx = crate::db::begin(&pool).await.unwrap();
    assert!(
        admin_tokens::revoke_admin_token(&mut revoke_tx, token_id)
            .await
            .unwrap()
    );
    revoke_tx.commit().await.unwrap();

    let actor = ReconcilingActor {
        user_id: f.owner,
        token_id: Some(token_id),
    };
    let usage = Usage {
        input_tokens: Some(3),
        output_tokens: Some(2),
        billing: None,
        ..Default::default()
    };
    let result = resolve_usage(
        &f.store,
        f.principal.workspace_id,
        start.id,
        usage,
        "receipt:revoked",
        actor,
    )
    .await;
    assert_eq!(
        result,
        Err(InferenceError::Unauthenticated),
        "a revoked admin token must refuse the write, not succeed with a dead credential"
    );
    let (state, _, actual, _) = amounts(&f, start.id).await;
    assert_eq!(state, "unknown", "no reservation state change");
    assert_eq!(actual, None, "no settlement amount written");
    assert_eq!(
        reconciled_count(&pool, start.id).await,
        0,
        "no usage.reconciled audit row for a refused write"
    );
}

/// The same race, for an admin token whose expiry (not revocation) already
/// passed by the time the write transaction runs. `expires_at` is
/// immutable once a token exists (`admin_api_tokens_guard`); the only way
/// to seed an already-expired fixture is a backdated `created_at`/
/// `expires_at` pair at INSERT time, which requires disabling the
/// *creation* guard (`admin_api_tokens_owner_is_admin`, which otherwise
/// forces `created_at:=now()`) for one INSERT in one transaction -- the
/// same established pattern `admin_tokens::enterprise_tests::
/// lookup_rejects_expired_revoked_and_owner_not_admin` already uses, not a
/// bypass of the immutability/finality/no-delete triggers this fix
/// actually depends on.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn expired_token_between_authentication_and_the_write_transaction_is_refused(
    pool: sqlx::PgPool,
) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    let start = f.start();
    admit(&f.store, &start, &request(), 30).await.unwrap();
    finish(&f.store, &done(start.id, None, None)).await.unwrap();

    let raw_id = Uuid::new_v4();
    let raw_digest = Sha256::digest(raw_id.as_bytes());
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("ALTER TABLE admin_api_tokens DISABLE TRIGGER admin_api_tokens_owner_is_admin")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO admin_api_tokens(id,owner_user_id,name,secret_hash,scopes,created_at,expires_at) VALUES($1,$2,'expired',$3,ARRAY['governance:write'],now()-interval '10 days',now()-interval '1 minute')")
        .bind(raw_id)
        .bind(f.owner)
        .bind(raw_digest.as_slice())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE admin_api_tokens ENABLE TRIGGER admin_api_tokens_owner_is_admin")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let actor = ReconcilingActor {
        user_id: f.owner,
        token_id: Some(raw_id),
    };
    let usage = Usage {
        input_tokens: Some(3),
        output_tokens: Some(2),
        billing: None,
        ..Default::default()
    };
    let result = resolve_usage(
        &f.store,
        f.principal.workspace_id,
        start.id,
        usage,
        "receipt:expired",
        actor,
    )
    .await;
    assert_eq!(result, Err(InferenceError::Unauthenticated));
    let (state, _, actual, _) = amounts(&f, start.id).await;
    assert_eq!(state, "unknown");
    assert_eq!(actual, None);
    assert_eq!(reconciled_count(&pool, start.id).await, 0);
}

/// `resolve_usage`'s successful audit row carries the authenticating
/// token's id; the same call with a plain browser actor (`From<Uuid>`)
/// carries none, exactly as before this fix.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn successful_reconciliation_audit_carries_the_token_id_browser_audit_stays_null(
    pool: sqlx::PgPool,
) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    let token_id = mint(&pool, f.owner).await;

    let via_token = f.start();
    admit(&f.store, &via_token, &request(), 30).await.unwrap();
    finish(&f.store, &done(via_token.id, None, None))
        .await
        .unwrap();
    let usage = Usage {
        input_tokens: Some(3),
        output_tokens: Some(2),
        billing: None,
        ..Default::default()
    };
    resolve_usage(
        &f.store,
        f.principal.workspace_id,
        via_token.id,
        usage,
        "receipt:token",
        ReconcilingActor {
            user_id: f.owner,
            token_id: Some(token_id),
        },
    )
    .await
    .unwrap();

    let via_browser = f.start();
    admit(&f.store, &via_browser, &request(), 30).await.unwrap();
    finish(&f.store, &done(via_browser.id, None, None))
        .await
        .unwrap();
    resolve_usage(
        &f.store,
        f.principal.workspace_id,
        via_browser.id,
        usage,
        "receipt:browser",
        f.owner,
    )
    .await
    .unwrap();

    let attributed: Option<Uuid> = sqlx::query_scalar(
        "SELECT admin_token_id FROM audit_events WHERE action='usage.reconciled' AND resource_id=$1",
    )
    .bind(via_token.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(attributed, Some(token_id));
    let browser_audit: Option<Uuid> = sqlx::query_scalar(
        "SELECT admin_token_id FROM audit_events WHERE action='usage.reconciled' AND resource_id=$1",
    )
    .bind(via_browser.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        browser_audit, None,
        "a browser-session reconciliation carries no admin_token_id, exactly as before"
    );
}

/// `admin_tokens::token_row_live` -- the exact primitive `resolve_usage`
/// now holds `FOR SHARE` across its own write -- blocks a concurrent
/// explicit revoke until the holder commits, deterministically (polling
/// `pg_blocking_pids`, never a sleep used to decide ordering).
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn token_row_live_for_share_blocks_a_concurrent_explicit_revoke_until_commit(
    pool: sqlx::PgPool,
) {
    let f = fixture(pool.clone()).await;
    let token_id = mint(&pool, f.owner).await;

    let mut holder = crate::db::begin(&pool).await.unwrap();
    assert!(
        admin_tokens::token_row_live(&mut holder, token_id, f.owner)
            .await
            .unwrap()
    );
    let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holder)
        .await
        .unwrap();

    let pool2 = pool.clone();
    let revoke_task = tokio::spawn(async move {
        let mut tx = crate::db::begin(&pool2).await.unwrap();
        admin_tokens::revoke_admin_token(&mut tx, token_id)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let blocked: Option<i32> = sqlx::query_scalar(
            "SELECT pid FROM pg_stat_activity WHERE pid<>$1 AND $1=ANY(pg_blocking_pids(pid)) LIMIT 1",
        )
        .bind(holder_pid)
        .fetch_optional(&pool)
        .await
        .unwrap();
        if blocked.is_some() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the revoke never blocked on the FOR SHARE holder within 5s -- the guard may have been removed"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    holder.commit().await.unwrap();
    revoke_task.await.unwrap();
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT revoked_at IS NOT NULL FROM admin_api_tokens WHERE id=$1"
        )
        .bind(token_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        "the revoke still completes once the FOR SHARE holder releases it"
    );
}
