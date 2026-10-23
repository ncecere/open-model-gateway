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
    sqlx::query(
        "INSERT INTO platform_role_grants(id,user_id,role,source) VALUES($1,$2,'admin','manual')",
    )
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
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT secret_hash FROM admin_api_tokens WHERE id=$1")
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
        Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidName
        ))
    ));
    assert!(matches!(
        store
            .create_admin_token(
                owner,
                &"x".repeat(121),
                &scopes(&["platform:read"]),
                30,
                None
            )
            .await,
        Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidName
        ))
    ));
    assert!(matches!(
        store
            .create_admin_token(owner, "n", &scopes(&[]), 30, None)
            .await,
        Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidScopes
        ))
    ));
    assert!(matches!(
        store
            .create_admin_token(owner, "n", &scopes(&["platform:admin"]), 30, None)
            .await,
        Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidScopes
        ))
    ));
    assert!(matches!(
        store
            .create_admin_token(
                owner,
                "n",
                &scopes(&["platform:read", "platform:read"]),
                30,
                None
            )
            .await,
        Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidScopes
        ))
    ));
    for days in [0, 91, -1] {
        assert!(matches!(
            store
                .create_admin_token(owner, "n", &scopes(&["platform:read"]), days, None)
                .await,
            Err(CreateAdminTokenError::Invalid(
                NewAdminTokenError::InvalidExpiry
            ))
        ));
    }
    assert!(matches!(
        store
            .create_admin_token(owner, "n", &scopes(&["platform:read"]), 30, Some(&[]))
            .await,
        Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidAllowlist
        ))
    ));
    let too_many: Vec<String> = (0..33).map(|i| format!("10.0.{i}.0/24")).collect();
    assert!(matches!(
        store
            .create_admin_token(owner, "n", &scopes(&["platform:read"]), 30, Some(&too_many))
            .await,
        Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidAllowlist
        ))
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
    let stored: Vec<String> =
        sqlx::query_scalar("SELECT ip_allowlist::text[] FROM admin_api_tokens WHERE id=$1")
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
        (
            "unknown scope",
            insert("ARRAY['wizard:cast']", "interval '10 days'", None),
        ),
        (
            "empty scopes",
            insert("ARRAY[]::text[]", "interval '10 days'", None),
        ),
        (
            "duplicate scopes",
            insert(
                "ARRAY['platform:read','platform:read']",
                "interval '10 days'",
                None,
            ),
        ),
        (
            "expiry too short",
            insert("ARRAY['platform:read']", "interval '12 hours'", None),
        ),
        (
            "expiry too long",
            insert("ARRAY['platform:read']", "interval '91 days'", None),
        ),
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
        assert!(
            result.is_err(),
            "{label} should be rejected by the database"
        );
    }
    // 33 allowlist entries, built with generate_series (cardinality bound).
    let too_many = "(SELECT array_agg(('10.0.'||g||'.0/24')::cidr) FROM generate_series(1,33) g)";
    let sql = insert(
        "ARRAY['platform:read']",
        "interval '10 days'",
        Some(too_many),
    );
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
        (
            "name",
            "UPDATE admin_api_tokens SET name='renamed' WHERE id=$1",
        ),
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
    let raw = GeneratedSecret::generate().unwrap();
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
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(owner)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO platform_role_grants(id,user_id,role,source) VALUES($1,$2,'user','manual')",
    )
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
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(manual)
    .execute(&pool)
    .await
    .unwrap();
    assert!(revoked(&pool, t1.id).await);
    sqlx::query(
        "INSERT INTO platform_role_grants(id,user_id,role,source) VALUES($1,$2,'admin','manual')",
    )
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
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND source='group'",
    )
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
        .create_admin_token(
            scim_deactivated,
            "t4",
            &scopes(&["platform:read"]),
            30,
            None,
        )
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
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND source='manual'",
    )
    .bind(dual)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        !revoked(&pool, t6.id).await,
        "one of two remaining admin grants keeps the token active"
    );
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND source='bootstrap'",
    )
    .bind(dual)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        revoked(&pool, t6.id).await,
        "losing the last remaining admin grant revokes the token"
    );
}

/// Coordinator review of #37 PR1: two admin grants of different
/// provenance (manual + group) can be revoked by concurrent transactions
/// that share no lock of their own in `omg_admin_tokens_role_lost`. Under
/// READ COMMITTED, if T1 revokes one grant while T2 concurrently revokes
/// the other, each trigger's `EXISTS` can still see the other's grant as
/// active (neither has committed yet from the other's point of view), so
/// neither revokes the token; if the user later regains admin, the token
/// comes back to life, which violates finality.
///
/// Migration 0027's `omg_authority_platform_grants` trigger already takes
/// a transaction-scoped advisory lock (class 72419512, keyed by user) on
/// any `platform_role_grants.revoked_at` update for a user, which happens
/// to already serialize exactly this scenario today (verified by hand with
/// two real `psql` sessions before writing this test). That is a different
/// migration's incidental coverage, not a property `admin_api_tokens` owns
/// or that migration 0050 asserts anywhere. This test disables that one
/// trigger for its duration so it exercises `omg_admin_tokens_role_lost`'s
/// own correctness in isolation, independent of whether 0027's lock
/// continues to cover this case.
async fn write_skew_scenario(pool: &PgPool, first_source: &str) {
    let second_source = if first_source == "manual" {
        "group"
    } else {
        "manual"
    };
    let store = Store::new(pool.clone());
    let owner = admin(pool, &format!("writeskew-{first_source}@test.invalid")).await;
    let mapping = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,'https://issuer.test','admins','platform','admin')")
        .bind(mapping)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO platform_role_grants(id,user_id,role,source,mapping_id) VALUES($1,$2,'admin','group',$3)",
    )
    .bind(Uuid::new_v4())
    .bind(owner)
    .bind(mapping)
    .execute(pool)
    .await
    .unwrap();
    let token = store
        .create_admin_token(owner, "t", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();

    sqlx::query("ALTER TABLE platform_role_grants DISABLE TRIGGER omg_authority_platform_grants")
        .execute(pool)
        .await
        .unwrap();

    let mut tx1 = pool.begin().await.unwrap();
    sqlx::query(&format!(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND source='{first_source}'"
    ))
    .bind(owner)
    .execute(&mut *tx1)
    .await
    .unwrap();

    let pool2 = pool.clone();
    let second_sql = format!(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND source='{second_source}'"
    );
    let handle = tokio::spawn(async move {
        let mut tx2 = pool2.begin().await.unwrap();
        sqlx::query(&second_sql)
            .bind(owner)
            .execute(&mut *tx2)
            .await
            .unwrap();
        tx2.commit().await.unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let blocked_while_first_open = !handle.is_finished();
    tx1.commit().await.unwrap();
    handle.await.unwrap();

    let revoked: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM admin_api_tokens WHERE id=$1")
            .bind(token.id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(
        revoked,
        "two admin grants revoked by concurrent, unsynchronized writers (second blocked while first was open={blocked_while_first_open}) must still revoke the token"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_revocation_of_manual_then_group_admin_grants_still_revokes_the_token(
    pool: PgPool,
) {
    write_skew_scenario(&pool, "manual").await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_revocation_of_group_then_manual_admin_grants_still_revokes_the_token(
    pool: PgPool,
) {
    write_skew_scenario(&pool, "group").await;
}

fn assert_owner_not_admin<T>(result: &Result<T, CreateAdminTokenError>) {
    match result {
        Err(CreateAdminTokenError::OwnerNotAdmin) => {}
        Err(other) => panic!("expected OwnerNotAdmin, got a different error: {other:?}"),
        Ok(_) => panic!("expected OwnerNotAdmin, got Ok"),
    }
}

/// Coordinator review of #37 PR1, issue 2: creation was never bound to
/// live admin status in the database itself (only PR2's future handler was
/// expected to check it). If the owner is demoted between that check and
/// the INSERT, the role-loss trigger has already run (nothing left to
/// revoke), so the new token survives and would come back to life if the
/// owner regains admin later.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn create_fails_when_the_owner_is_not_currently_an_effective_admin(pool: PgPool) {
    let store = Store::new(pool.clone());

    let non_admin = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
        .bind(non_admin)
        .bind("plain@test.invalid")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO platform_role_grants(id,user_id,role,source) VALUES($1,$2,'user','manual')",
    )
    .bind(Uuid::new_v4())
    .bind(non_admin)
    .execute(&pool)
    .await
    .unwrap();
    let result = store
        .create_admin_token(non_admin, "n", &scopes(&["platform:read"]), 30, None)
        .await;
    assert_owner_not_admin(&result);

    let disabled = admin(&pool, "disabled-create@test.invalid").await;
    sqlx::query("UPDATE users SET disabled_at=now(),disable_reason='admin_suspension' WHERE id=$1")
        .bind(disabled)
        .execute(&pool)
        .await
        .unwrap();
    let result = store
        .create_admin_token(disabled, "n", &scopes(&["platform:read"]), 30, None)
        .await;
    assert_owner_not_admin(&result);

    let cleaned = admin(&pool, "cleaned-create@test.invalid").await;
    sqlx::query("UPDATE users SET disabled_at=now(),cleaned_at=now(),email=NULL WHERE id=$1")
        .bind(cleaned)
        .execute(&pool)
        .await
        .unwrap();
    let result = store
        .create_admin_token(cleaned, "n", &scopes(&["platform:read"]), 30, None)
        .await;
    assert_owner_not_admin(&result);

    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admin_api_tokens")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0,
        "nothing was ever inserted for any of these refused owners"
    );
}

/// A concurrent demotion racing a concurrent creation: the create must
/// either see the owner still admin (and succeed, atomically with respect
/// to the demotion) or block until the demotion commits and then fail,
/// never insert a token for an owner who has already lost admin.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_demotion_blocks_and_then_fails_a_pending_create(pool: PgPool) {
    let store = Store::new(pool.clone());
    let owner = admin(&pool, "race-create@test.invalid").await;

    let mut tx1 = pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(owner)
    .execute(&mut *tx1)
    .await
    .unwrap();
    // tx1's role-loss trigger now holds the owner's users row FOR UPDATE.

    let store2 = store.clone();
    let handle = tokio::spawn(async move {
        store2
            .create_admin_token(owner, "race", &["platform:read".to_string()], 30, None)
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let blocked_while_demotion_open = !handle.is_finished();
    tx1.commit().await.unwrap();
    let result = handle.await.unwrap();
    if !matches!(result, Err(CreateAdminTokenError::OwnerNotAdmin)) {
        panic!("blocked_while_demotion_open={blocked_while_demotion_open}, expected OwnerNotAdmin");
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM admin_api_tokens WHERE owner_user_id=$1 AND revoked_at IS NULL",
    )
    .bind(owner)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        count, 0,
        "no active token exists for the now-demoted owner, blocked_while_demotion_open={blocked_while_demotion_open}"
    );
}

/// Coordinator review of #37 PR1, issue 3: `created_at`/`expires_at` came
/// from the app's `Utc::now()`, but expiry is enforced against the
/// database's `clock_timestamp()`. A skewed app clock could let a token
/// outlive 90 real days, or be created already expired. Both timestamps
/// must come from one database instant.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn created_at_and_expires_at_come_from_the_database_clock_not_the_app(pool: PgPool) {
    let store = Store::new(pool.clone());
    let owner = admin(&pool, "clock@test.invalid").await;
    let before: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let created = store
        .create_admin_token(owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    let after: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        created.created_at >= before - chrono::Duration::seconds(1),
        "created_at={:?} before={before:?}",
        created.created_at
    );
    assert!(
        created.created_at <= after + chrono::Duration::seconds(1),
        "created_at={:?} after={after:?}",
        created.created_at
    );
    assert_eq!(
        created.expires_at - created.created_at,
        chrono::Duration::days(30)
    );
    // The returned timestamps are exactly what the row stores, not a
    // separately-computed app-side echo of them.
    let (db_created, db_expires): (DateTime<Utc>, DateTime<Utc>) =
        sqlx::query_as("SELECT created_at,expires_at FROM admin_api_tokens WHERE id=$1")
            .bind(created.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (db_created, db_expires),
        (created.created_at, created.expires_at)
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
    assert_eq!(
        (second_at, second_addr),
        (first_at, "203.0.113.5".to_string())
    );
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
