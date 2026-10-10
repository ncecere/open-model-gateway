//! rand 0.10: the OS random number generator is fallible. Secret creation
//! (inference keys, rotation, invitations) fails closed with 503 and stores
//! nothing; it never panics or falls back to another generator.
use super::*;
use crate::entropy::seam::fail_on_this_thread;

async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_and_invitation_creation_fail_closed_when_the_os_rng_fails(pool: PgPool) {
    let f = fixture(&pool).await;
    let keys = count(&pool, "SELECT count(*) FROM api_keys").await;
    {
        let _fail = fail_on_this_thread();
        let (status, body) = key(&f, &f.owner, f.personal, Value::Null).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["error"]["message"], ENTROPY_UNAVAILABLE);
        assert!(body.get("token").is_none());
    }
    assert_eq!(count(&pool, "SELECT count(*) FROM api_keys").await, keys);
    // Rotation creates a new secret too: refused, the old key stays valid.
    let (status, created) = key(&f, &f.owner, f.personal, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let kid = created["id"].as_str().unwrap().to_owned();
    {
        let _fail = fail_on_this_thread();
        let (status, body) = call(
            &f.s,
            &f.owner,
            "POST",
            &format!("/api/v1/workspaces/{}/keys/{kid}/rotate", f.personal),
            json!({"expires_in_days":2}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    }
    assert!(
        f.s.authenticate(created["token"].as_str().unwrap())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM api_keys").await,
        keys + 1
    );
    // Invitations: no row, no token.
    {
        let _fail = fail_on_this_thread();
        let (status, body) = call(
            &f.s,
            &f.owner,
            "POST",
            &format!("/api/v1/workspaces/{}/invitations", f.project),
            json!({"email":"new@example.test","role":"member"}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(body.get("token").is_none());
    }
    assert_eq!(
        count(&pool, "SELECT count(*) FROM workspace_invitations").await,
        0
    );
    // The same calls succeed once the RNG works again.
    let (status, body) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{}/invitations", f.project),
        json!({"email":"new@example.test","role":"member"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["token"].as_str().map(str::len), Some(64));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn development_bootstrap_exits_with_an_error_when_the_os_rng_fails(pool: PgPool) {
    let store = Store::new(pool.clone());
    {
        let _fail = fail_on_this_thread();
        let error = crate::bootstrap::seed(&store, crate::config::Environment::Development)
            .await
            .err()
            .expect("bootstrap must fail closed");
        assert!(
            error.to_string().contains("random number generator"),
            "{error}"
        );
    }
    // Nothing was written (one transaction), so a later run seeds normally.
    assert_eq!(count(&pool, "SELECT count(*) FROM api_keys").await, 0);
    assert!(
        crate::bootstrap::seed(&store, crate::config::Environment::Development)
            .await
            .unwrap()
            .is_some()
    );
}
