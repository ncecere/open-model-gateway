//! #37 PR1 real-PostgreSQL coverage: schema constraints, immutability and
//! no-delete, revocation finality, owner transitions, lookup-for-
//! authorization, the parser's no-DB-access refusal and the last-use
//! throttle. Runtime-role ACL probes live in `tests/runtime_privileges.rs`.
use super::*;
use sqlx::PgPool;

async fn admin(pool: &PgPool, email: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
        .bind(id)
        .bind(email)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(id,user_id,role,source) VALUES($1,$2,'admin','manual')")
        .bind(Uuid::new_v4())
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    id
}

fn scopes(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| s.to_string()).collect()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn create_returns_plaintext_once_and_authenticate_round_trips(pool: PgPool) {
    let store = Store::new(pool.clone());
    let owner = admin(&pool, "owner@test.invalid").await;
    let created = store
        .create_admin_token(owner, "CI token", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    assert!(created.token.starts_with("omg_admin_"));
    let found = store
        .authenticate_admin_token(&created.token)
        .await
        .unwrap()
        .expect("a live token authenticates");
    assert_eq!(found.token_id, created.id);
    assert_eq!(found.owner_user_id, owner);
    assert_eq!(found.scopes, vec!["platform:read".to_string()]);
    // The stored row never carries the plaintext or anything derived from it
    // other than the digest.
    let stored: Vec<u8> = sqlx::query_scalar("SELECT secret_hash FROM admin_api_tokens WHERE id=$1")
        .bind(created.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, created.digest.to_vec());
    // A wrong secret with the right id does not authenticate.
    let tampered = format!("{}0", &created.token[..created.token.len() - 1]);
    assert!(
        store
            .authenticate_admin_token(&tampered)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn create_rejects_invalid_input_before_touching_the_database(pool: PgPool) {
    let store = Store::new(pool.clone());
    let owner = admin(&pool, "owner@test.invalid").await;
    assert!(matches!(
        store
            .create_admin_token(owner, "", &scopes(&["platform:read"]), 30, None)
            .await,
        Err(CreateAdminTokenError::Invalid(NewAdminTokenError::InvalidName))
    ));
    assert!(matches!(
        store
            .create_admin_token(owner, &"x".repeat(121), &scopes(&["platform:read"]), 30, None)
            .await,
        Err(CreateAdminTokenError::Invalid(NewAdminTokenError::InvalidName))
    ));
    assert!(matches!(
        store
            .create_admin_token(owner, "n", &scopes(&[]), 30, None)
            .await,
        Err(CreateAdminTokenError::Invalid(NewAdminTokenError::InvalidScopes))
    ));
    assert!(matches!(
        store
            .create_admin_token(owner, "n", &scopes(&["platform:admin"]), 30, None)
            .await,
        Err(CreateAdminTokenError::Invalid(NewAdminTokenError::InvalidScopes))
    ));
    assert!(matches!(
        store
            .create_admin_token(owner, "n", &scopes(&["platform:read", "platform:read"]), 30, None)
            .await,
        Err(CreateAdminTokenError::Invalid(NewAdminTokenError::InvalidScopes))
    ));
    for days in [0, 91, -1] {
        assert!(matches!(
            store
                .create_admin_token(owner, "n", &scopes(&["platform:read"]), days, None)
                .await,
            Err(CreateAdminTokenError::Invalid(NewAdminTokenError::InvalidExpiry))
        ));
    }
    assert!(matches!(
        store
            .create_admin_token(owner, "n", &scopes(&["platform:read"]), 30, Some(&[]))
            .await,
        Err(CreateAdminTokenError::Invalid(NewAdminTokenError::InvalidAllowlist))
    ));
    let too_many: Vec<String> = (0..33).map(|i| format!("10.0.{i}.0/24")).collect();
    assert!(matches!(
        store
            .create_admin_token(owner, "n", &scopes(&["platform:read"]), 30, Some(&too_many))
            .await,
        Err(CreateAdminTokenError::Invalid(NewAdminTokenError::InvalidAllowlist))
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admin_api_tokens")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0,
        "nothing was ever inserted for a refused creation"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn create_accepts_a_valid_cidr_allowlist(pool: PgPool) {
    let store = Store::new(pool.clone());
    let owner = admin(&pool, "owner@test.invalid").await;
    let list = vec!["10.0.0.0/24".to_string(), "2001:db8::/32".to_string()];
    let created = store
        .create_admin_token(owner, "n", &scopes(&["platform:read"]), 30, Some(&list))
        .await
        .unwrap();
    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT ip_allowlist::text[] FROM admin_api_tokens WHERE id=$1",
    )
    .bind(created.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored.len(), 2);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn database_level_schema_constraints_hold_even_bypassing_the_rust_validator(pool: PgPool) {
    // These exercise migration 0050's CHECK constraints directly: the Rust
    // validator in create_admin_token is a fail-fast gate, not the only one.
    let owner = admin(&pool, "owner@test.invalid").await;
    let make_hash = |b: u8| vec![b; 32];
    let insert = |scopes_sql: &str, expires_sql: &str, allowlist_sql: Option<&str>| {
        format!(
            "INSERT INTO admin_api_tokens(id,owner_user_id,name,secret_hash,scopes,ip_allowlist,expires_at)
             VALUES(gen_random_uuid(),'{owner}','probe',decode('{hash}','hex'),{scopes_sql},{allowlist},now()+{expires_sql})",
            hash = hex::encode(make_hash(0x11)),
            allowlist = allowlist_sql.unwrap_or("NULL")
        )
    };
    for (label, sql) in [
        ("unknown scope", insert("ARRAY['wizard:cast']", "interval '10 days'", None)),
        ("empty scopes", insert("ARRAY[]::text[]", "interval '10 days'", None)),
        (
            "duplicate scopes",
            insert(
                "ARRAY['platform:read','platform:read']",
                "interval '10 days'",
                None,
            ),
        ),
        ("expiry too short", insert("ARRAY['platform:read']", "interval '12 hours'", None)),
        ("expiry too long", insert("ARRAY['platform:read']", "interval '91 days'", None)),
        (
            "empty allowlist",
            insert(
                "ARRAY['platform:read']",
                "interval '10 days'",
                Some("ARRAY[]::cidr[]"),
            ),
        ),
    ] {
        let result = sqlx::query(&sql).execute(&pool).await;
        assert!(result.is_err(), "{label} should be rejected by the database");
    }
    // 33 allowlist entries, built with generate_series (cardinality bound).
    let too_many = "(SELECT array_agg(('10.0.'||g||'.0/24')::cidr) FROM generate_series(1,33) g)";
    let sql = insert("ARRAY['platform:read']", "interval '10 days'", Some(too_many));
    assert!(
        sqlx::query(&sql).execute(&pool).await.is_err(),
        "33 allowlist entries should be rejected"
    );
    // A fully valid row (the positive control; schema constraints did not
    // over-reject anything else).
    let sql = insert("ARRAY['platform:read']", "interval '10 days'", None);
    sqlx::query(&sql).execute(&pool).await.unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn identity_digest_scopes_expiry_and_allowlist_are_immutable_and_rows_are_never_deleted(
    pool: PgPool,
) {
    let store = Store::new(pool.clone());
    let owner = admin(&pool, "owner@test.invalid").await;
    let list = vec!["10.0.0.0/24".to_string()];
    let created = store
        .create_admin_token(owner, "n", &scopes(&["platform:read"]), 30, Some(&list))
        .await
        .unwrap();
    for (label, sql) in [
        ("name", "UPDATE admin_api_tokens SET name='renamed' WHERE id=$1"),
        (
            "owner",
            "UPDATE admin_api_tokens SET owner_user_id=gen_random_uuid() WHERE id=$1",
        ),
        (
            "digest",
            "UPDATE admin_api_tokens SET secret_hash=decode(repeat('99',32),'hex') WHERE id=$1",
        ),
        (
            "scopes",
            "UPDATE admin_api_tokens SET scopes=ARRAY['settings:write'] WHERE id=$1",
        ),
        (
            "expiry",
            "UPDATE admin_api_tokens SET expires_at=now()+interval '1 day' WHERE id=$1",
        ),
        (
            "allowlist",
            "UPDATE admin_api_tokens SET ip_allowlist=ARRAY['1.1.1.0/24']::cidr[] WHERE id=$1",
        ),
    ] {
        let result = sqlx::query(sql).bind(created.id).execute(&pool).await;
        assert!(result.is_err(), "{label} must be immutable");
    }
    assert!(
        sqlx::query("DELETE FROM admin_api_tokens WHERE id=$1")
            .bind(created.id)
            .execute(&pool)
            .await
            .is_err(),
        "no row may ever be deleted"
    );
    assert!(
        sqlx::query("TRUNCATE admin_api_tokens")
            .execute(&pool)
            .await
            .is_err(),
        "the table may never be truncated"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn revocation_is_final_and_revoke_is_idempotent(pool: PgPool) {
    let store = Store::new(pool.clone());
    let owner = admin(&pool, "owner@test.invalid").await;
    let created = store
        .create_admin_token(owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    assert!(store.revoke_admin_token(created.id).await.unwrap());
    // Idempotent: revoking again is not an error and does not move the time.
    let first: DateTime<Utc> =
        sqlx::query_scalar("SELECT revoked_at FROM admin_api_tokens WHERE id=$1")
            .bind(created.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(store.revoke_admin_token(created.id).await.unwrap());
    let second: DateTime<Utc> =
        sqlx::query_scalar("SELECT revoked_at FROM admin_api_tokens WHERE id=$1")
            .bind(created.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(first, second);
    // Revoking a token that never existed is not an error either.
    assert!(!store.revoke_admin_token(Uuid::new_v4()).await.unwrap());
    // Cannot be un-revoked, by anyone, ever.
    assert!(
        sqlx::query("UPDATE admin_api_tokens SET revoked_at=NULL WHERE id=$1")
            .bind(created.id)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        store
            .authenticate_admin_token(&created.token)
            .await
            .unwrap()
            .is_none(),
        "a revoked token never authenticates"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn lookup_rejects_expired_revoked_and_owner_not_admin(pool: PgPool) {
    let store = Store::new(pool.clone());
    let owner = admin(&pool, "owner@test.invalid").await;

    // expires_at is immutable (by design); an expired fixture is inserted
    // directly with both created_at and expires_at in the past (satisfying
    // the 1-90 day bound measured from created_at).
    let raw = NewAdminToken::generate().unwrap();
    sqlx::query("INSERT INTO admin_api_tokens(id,owner_user_id,name,secret_hash,scopes,created_at,expires_at) VALUES($1,$2,'expired',$3,ARRAY['platform:read'],now()-interval '10 days',now()-interval '1 minute')")
        .bind(raw.id)
        .bind(owner)
        .bind(raw.digest.as_slice())
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        store
            .authenticate_admin_token(&raw.token)
            .await
            .unwrap()
            .is_none(),
        "expired tokens never authenticate"
    );

    let revoked = store
        .create_admin_token(owner, "revoked", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    assert!(store.revoke_admin_token(revoked.id).await.unwrap());
    assert!(
        store
            .authenticate_admin_token(&revoked.token)
            .await
            .unwrap()
            .is_none()
    );

    // Owner demoted to a non-admin role: the token no longer authenticates,
    // even though it is itself unrevoked and unexpired.
    let demoted = store
        .create_admin_token(owner, "demoted", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(id,user_id,role,source) VALUES($1,$2,'user','manual')")
        .bind(Uuid::new_v4())
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    // Migration 0050's trigger already revoked it on the role-loss UPDATE
    // above (owner-invalidation); the lookup refuses it either way.
    assert!(
        store
            .authenticate_admin_token(&demoted.token)
            .await
            .unwrap()
            .is_none()
    );
    let revoked_by_trigger: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM admin_api_tokens WHERE id=$1")
            .bind(demoted.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        revoked_by_trigger,
        "losing the last admin grant revokes owned tokens immediately"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn owner_transitions_revoke_tokens_and_regaining_admin_never_restores_them(pool: PgPool) {
    let store = Store::new(pool.clone());

    // Manual demotion of the only admin grant.
    let manual = admin(&pool, "manual@test.invalid").await;
    let t1 = store
        .create_admin_token(manual, "t1", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'")
        .bind(manual)
        .execute(&pool)
        .await
        .unwrap();
    assert!(revoked(&pool, t1.id).await);
    sqlx::query("INSERT INTO platform_role_grants(id,user_id,role,source) VALUES($1,$2,'admin','manual')")
        .bind(Uuid::new_v4())
        .bind(manual)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        revoked(&pool, t1.id).await,
        "regaining admin after manual demotion does not restore the token"
    );

    // Group-derived admin removal.
    let grouped = admin(&pool, "grouped@test.invalid").await; // seeds a manual grant too
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(grouped)
        .execute(&pool)
        .await
        .unwrap(); // start from zero grants
    let mapping = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,'https://issuer.test','admins','platform','admin')")
        .bind(mapping)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(id,user_id,role,source,mapping_id) VALUES($1,$2,'admin','group',$3)")
        .bind(Uuid::new_v4())
        .bind(grouped)
        .bind(mapping)
        .execute(&pool)
        .await
        .unwrap();
    let t2 = store
        .create_admin_token(grouped, "t2", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND source='group'")
        .bind(grouped)
        .execute(&pool)
        .await
        .unwrap();
    assert!(revoked(&pool, t2.id).await);

    // Suspension.
    let suspended = admin(&pool, "suspended@test.invalid").await;
    let t3 = store
        .create_admin_token(suspended, "t3", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET disabled_at=now(),disable_reason='admin_suspension' WHERE id=$1")
        .bind(suspended)
        .execute(&pool)
        .await
        .unwrap();
    assert!(revoked(&pool, t3.id).await);
    sqlx::query("UPDATE users SET disabled_at=NULL,disable_reason=NULL WHERE id=$1")
        .bind(suspended)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        revoked(&pool, t3.id).await,
        "reinstating a suspended owner does not restore the token"
    );

    // SCIM deactivation uses the same disabled_at column (scim.rs).
    let scim_deactivated = admin(&pool, "scim@test.invalid").await;
    let t4 = store
        .create_admin_token(scim_deactivated, "t4", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET disabled_at=now(),disable_reason='scim_deactivated' WHERE id=$1")
        .bind(scim_deactivated)
        .execute(&pool)
        .await
        .unwrap();
    assert!(revoked(&pool, t4.id).await);

    // Cleanup.
    let cleaned = admin(&pool, "cleaned@test.invalid").await;
    let t5 = store
        .create_admin_token(cleaned, "t5", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET disabled_at=now(),cleaned_at=now(),email=NULL WHERE id=$1")
        .bind(cleaned)
        .execute(&pool)
        .await
        .unwrap();
    assert!(revoked(&pool, t5.id).await);

    // Two active admin grants: revoking only one must not revoke the token.
    let dual = admin(&pool, "dual@test.invalid").await; // manual grant #1
    sqlx::query("INSERT INTO platform_role_grants(id,user_id,role,source) VALUES($1,$2,'admin','bootstrap')")
        .bind(Uuid::new_v4())
        .bind(dual)
        .execute(&pool)
        .await
        .unwrap();
    let t6 = store
        .create_admin_token(dual, "t6", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND source='manual'")
        .bind(dual)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        !revoked(&pool, t6.id).await,
        "one of two remaining admin grants keeps the token active"
    );
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND source='bootstrap'")
        .bind(dual)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        revoked(&pool, t6.id).await,
        "losing the last remaining admin grant revokes the token"
    );
}

async fn revoked(pool: &PgPool, id: Uuid) -> bool {
    sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM admin_api_tokens WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn last_use_is_throttled_to_once_per_five_minutes(pool: PgPool) {
    let store = Store::new(pool.clone());
    let owner = admin(&pool, "owner@test.invalid").await;
    let created = store
        .create_admin_token(owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    store
        .touch_admin_token_use(created.id, Some("203.0.113.5"))
        .await
        .unwrap();
    let (first_at, first_addr): (DateTime<Utc>, String) = sqlx::query_as(
        "SELECT last_used_at,host(last_used_address) FROM admin_api_tokens WHERE id=$1",
    )
    .bind(created.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(first_addr, "203.0.113.5");
    // A second use moments later, with a different address, does not write.
    store
        .touch_admin_token_use(created.id, Some("198.51.100.9"))
        .await
        .unwrap();
    let (second_at, second_addr): (DateTime<Utc>, String) = sqlx::query_as(
        "SELECT last_used_at,host(last_used_address) FROM admin_api_tokens WHERE id=$1",
    )
    .bind(created.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((second_at, second_addr), (first_at, "203.0.113.5".to_string()));
    // Once the window has passed, the next use writes again.
    sqlx::query("UPDATE admin_api_tokens SET last_used_at=now()-interval '6 minutes' WHERE id=$1")
        .bind(created.id)
        .execute(&pool)
        .await
        .unwrap();
    store
        .touch_admin_token_use(created.id, Some("198.51.100.9"))
        .await
        .unwrap();
    let third_addr: String =
        sqlx::query_scalar("SELECT host(last_used_address) FROM admin_api_tokens WHERE id=$1")
            .bind(created.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(third_addr, "198.51.100.9");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn malformed_tokens_never_reach_the_database(pool: PgPool) {
    let store = Store::new(pool.clone());
    pool.close().await;
    // A closed pool would error on any query; a structurally invalid token
    // must still return `Ok(None)` because the parser refuses it first.
    for bad in ["", "omg_admin_not-a-token", "omg_ ", "Bearer x"] {
        assert!(store.authenticate_admin_token(bad).await.unwrap().is_none());
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn owner_restrict_prevents_removing_a_user_with_live_tokens(pool: PgPool) {
    let store = Store::new(pool.clone());
    let owner = admin(&pool, "owner@test.invalid").await;
    let created = store
        .create_admin_token(owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    assert!(
        sqlx::query("DELETE FROM users WHERE id=$1")
            .bind(owner)
            .execute(&pool)
            .await
            .is_err(),
        "owner FK is RESTRICT: a referenced user cannot be removed"
    );
    let _ = created.digest; // silence unused-field lints on this struct's other field
}
