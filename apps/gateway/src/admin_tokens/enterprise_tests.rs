//! #37 PR1 real-PostgreSQL coverage: schema constraints, immutability and
//! no-delete, revocation finality, owner transitions, lookup-for-
//! authorization, the parser's no-DB-access refusal and the last-use
//! throttle. Runtime-role ACL probes live in `tests/runtime_privileges.rs`.
use super::*;
use sqlx::PgPool;

/// Test convenience: `create_admin_token` on a transaction of its own
/// (begin, call, commit/rollback). Most tests below don't care about the
/// lock-holding contract PR2 must follow (see the dedicated tests that
/// do, further down); they just want a token.
async fn create(
    pool: &PgPool,
    owner: Uuid,
    name: &str,
    scopes: &[String],
    days: i32,
    allowlist: Option<&[String]>,
) -> Result<NewAdminToken, CreateAdminTokenError> {
    let mut tx = crate::db::begin(pool).await.unwrap();
    match create_admin_token(&mut tx, owner, name, scopes, days, allowlist).await {
        Ok(token) => {
            tx.commit().await.unwrap();
            Ok(token)
        }
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

/// Test convenience: `revoke_admin_token` on a transaction of its own.
async fn revoke(pool: &PgPool, id: Uuid) -> bool {
    let mut tx = crate::db::begin(pool).await.unwrap();
    let result = revoke_admin_token(&mut tx, id).await.unwrap();
    tx.commit().await.unwrap();
    result
}

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
    let created = create(
        &pool,
        owner,
        "CI token",
        &scopes(&["platform:read"]),
        30,
        None,
    )
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
    // A wrong secret with the right id does not authenticate. Flip the
    // last hex digit to a value it provably isn't (pre-existing flake,
    // found incidentally while running this suite for #37 PR1's
    // adversarial re-review: replacing it with a fixed '0' was a no-op,
    // and so a false pass, on the 1-in-16 run where that digit already
    // was '0').
    let last = created.token.chars().next_back().unwrap();
    let flipped = if last == '0' { '1' } else { '0' };
    let tampered = format!("{}{flipped}", &created.token[..created.token.len() - 1]);
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
    let owner = admin(&pool, "owner@test.invalid").await;
    assert!(matches!(
        create(&pool, owner, "", &scopes(&["platform:read"]), 30, None).await,
        Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidName
        ))
    ));
    assert!(matches!(
        create(
            &pool,
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
        create(&pool, owner, "n", &scopes(&[]), 30, None).await,
        Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidScopes
        ))
    ));
    assert!(matches!(
        create(&pool, owner, "n", &scopes(&["platform:admin"]), 30, None).await,
        Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidScopes
        ))
    ));
    assert!(matches!(
        create(
            &pool,
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
            create(&pool, owner, "n", &scopes(&["platform:read"]), days, None).await,
            Err(CreateAdminTokenError::Invalid(
                NewAdminTokenError::InvalidExpiry
            ))
        ));
    }
    assert!(matches!(
        create(
            &pool,
            owner,
            "n",
            &scopes(&["platform:read"]),
            30,
            Some(&[])
        )
        .await,
        Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidAllowlist
        ))
    ));
    let too_many: Vec<String> = (0..33).map(|i| format!("10.0.{i}.0/24")).collect();
    assert!(matches!(
        create(
            &pool,
            owner,
            "n",
            &scopes(&["platform:read"]),
            30,
            Some(&too_many)
        )
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
    let owner = admin(&pool, "owner@test.invalid").await;
    let list = vec!["10.0.0.0/24".to_string(), "2001:db8::/32".to_string()];
    let created = create(
        &pool,
        owner,
        "n",
        &scopes(&["platform:read"]),
        30,
        Some(&list),
    )
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

/// Coordinator review of #37 PR1, issue 5b: each allowlist entry must be
/// `addr/len`, a parseable IP, `len<=32` (v4) or `len<=128` (v6), with no
/// host bits set and no IPv4-mapped IPv6 network, checked in Rust before
/// any database round trip. PostgreSQL's own `cidr` type would reject
/// most of these too, but as a raw `22P02` `Database` error, not the
/// typed `InvalidAllowlist` a caller can match on.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn create_rejects_malformed_allowlist_entries_before_touching_the_database(pool: PgPool) {
    let owner = admin(&pool, "cidr@test.invalid").await;
    for bad in [
        "not-an-ip",
        "10.0.0.1/33",
        "10.0.0.5/24",
        "2001:db8::/129",
        "::ffff:10.0.0.0/120",
    ] {
        let result = create(
            &pool,
            owner,
            "n",
            &scopes(&["platform:read"]),
            30,
            Some(&[bad.to_string()]),
        )
        .await;
        // NewAdminToken deliberately carries no Debug (it holds the
        // plaintext token), so match on the error shape only.
        assert!(
            matches!(
                result,
                Err(CreateAdminTokenError::Invalid(
                    NewAdminTokenError::InvalidAllowlist
                ))
            ),
            "{bad:?} should be InvalidAllowlist"
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admin_api_tokens")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0,
        "nothing was ever inserted for any of these malformed entries"
    );
}

/// Every stored entry is the canonical `addr/len` form, not whatever
/// capitalization/representation the caller happened to send.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn create_canonicalizes_allowlist_entries(pool: PgPool) {
    let owner = admin(&pool, "cidr-canon@test.invalid").await;
    let list = vec!["10.0.0.0/24".to_string(), "2001:DB8::/32".to_string()];
    let created = create(
        &pool,
        owner,
        "n",
        &scopes(&["platform:read"]),
        30,
        Some(&list),
    )
    .await
    .unwrap();
    let mut stored: Vec<String> =
        sqlx::query_scalar("SELECT ip_allowlist::text[] FROM admin_api_tokens WHERE id=$1")
            .bind(created.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    stored.sort();
    assert_eq!(
        stored,
        vec!["10.0.0.0/24".to_string(), "2001:db8::/32".to_string()]
    );
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
    let owner = admin(&pool, "owner@test.invalid").await;
    let list = vec!["10.0.0.0/24".to_string()];
    let created = create(
        &pool,
        owner,
        "n",
        &scopes(&["platform:read"]),
        30,
        Some(&list),
    )
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
    let created = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    assert!(revoke(&pool, created.id).await);
    // Idempotent: revoking again is not an error and does not move the time.
    let first: DateTime<Utc> =
        sqlx::query_scalar("SELECT revoked_at FROM admin_api_tokens WHERE id=$1")
            .bind(created.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(revoke(&pool, created.id).await);
    let second: DateTime<Utc> =
        sqlx::query_scalar("SELECT revoked_at FROM admin_api_tokens WHERE id=$1")
            .bind(created.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(first, second);
    // Revoking a token that never existed is not an error either.
    assert!(!revoke(&pool, Uuid::new_v4()).await);
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

    let revoked = create(
        &pool,
        owner,
        "revoked",
        &scopes(&["platform:read"]),
        30,
        None,
    )
    .await
    .unwrap();
    assert!(revoke(&pool, revoked.id).await);
    assert!(
        store
            .authenticate_admin_token(&revoked.token)
            .await
            .unwrap()
            .is_none()
    );

    // Owner demoted to a non-admin role: the token no longer authenticates,
    // even though it is itself unrevoked and unexpired.
    let demoted = create(
        &pool,
        owner,
        "demoted",
        &scopes(&["platform:read"]),
        30,
        None,
    )
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
    // Manual demotion of the only admin grant.
    let manual = admin(&pool, "manual@test.invalid").await;
    let t1 = create(&pool, manual, "t1", &scopes(&["platform:read"]), 30, None)
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
    let t2 = create(&pool, grouped, "t2", &scopes(&["platform:read"]), 30, None)
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
    let t3 = create(
        &pool,
        suspended,
        "t3",
        &scopes(&["platform:read"]),
        30,
        None,
    )
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
    let t4 = create(
        &pool,
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
    let t5 = create(&pool, cleaned, "t5", &scopes(&["platform:read"]), 30, None)
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
    let t6 = create(&pool, dual, "t6", &scopes(&["platform:read"]), 30, None)
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
    let token = create(pool, owner, "t", &scopes(&["platform:read"]), 30, None)
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
    let result = create(&pool, non_admin, "n", &scopes(&["platform:read"]), 30, None).await;
    assert_owner_not_admin(&result);

    let disabled = admin(&pool, "disabled-create@test.invalid").await;
    sqlx::query("UPDATE users SET disabled_at=now(),disable_reason='admin_suspension' WHERE id=$1")
        .bind(disabled)
        .execute(&pool)
        .await
        .unwrap();
    let result = create(&pool, disabled, "n", &scopes(&["platform:read"]), 30, None).await;
    assert_owner_not_admin(&result);

    let cleaned = admin(&pool, "cleaned-create@test.invalid").await;
    sqlx::query("UPDATE users SET disabled_at=now(),cleaned_at=now(),email=NULL WHERE id=$1")
        .bind(cleaned)
        .execute(&pool)
        .await
        .unwrap();
    let result = create(&pool, cleaned, "n", &scopes(&["platform:read"]), 30, None).await;
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
    let owner = admin(&pool, "race-create@test.invalid").await;

    let mut tx1 = pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(owner)
    .execute(&mut *tx1)
    .await
    .unwrap();
    // tx1's role-loss trigger now holds the owner's user-scope advisory lock.

    let pool2 = pool.clone();
    let handle = tokio::spawn(async move {
        // PR2's own contract: take the lock up front in the same
        // transaction, then create on it.
        let mut tx2 = crate::db::begin(&pool2).await.unwrap();
        crate::governance::locks::exclusive(
            &mut tx2,
            [crate::governance::locks::Scope::User(owner)],
        )
        .await
        .unwrap();
        let result = create_admin_token(
            &mut tx2,
            owner,
            "race",
            &["platform:read".to_string()],
            30,
            None,
        )
        .await;
        match &result {
            Ok(_) => tx2.commit().await.unwrap(),
            Err(_) => tx2.rollback().await.unwrap(),
        }
        result
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
    let owner = admin(&pool, "clock@test.invalid").await;
    let before: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let created = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
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

fn is_deadlock(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("40P01"))
}

/// Coordinator review of #37 PR1, follow-up: the `users ... FOR UPDATE`
/// fix for issue 1 introduced a new deadlock (40P01) that did not exist
/// before migration 0050.
///
/// The manual demotion path (management/directory.rs) locks the owner's
/// `users` row `FOR UPDATE` first, then updates the grant, which fires
/// 0027's `BEFORE` trigger `omg_authority_platform_grants` and takes the
/// user-scope advisory lock (class 72419512). Group-mapping deletion
/// (directory.rs) updates the grant first (taking the advisory lock
/// first), and the role-loss `AFTER` trigger then wanted the `users` row
/// `FOR UPDATE`. T1 holds the row and waits for the advisory lock; T2
/// holds the advisory lock and waits for the row: a circular wait Postgres
/// resolves by aborting one side with SQLSTATE 40P01. Before migration
/// 0050 neither trigger touched the `users` row at all, so this deadlock
/// is new.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_manual_and_group_revocation_does_not_deadlock(pool: PgPool) {
    let owner = admin(&pool, "deadlock@test.invalid").await;
    let manual_grant: Uuid = sqlx::query_scalar(
        "SELECT id FROM platform_role_grants WHERE user_id=$1 AND role='admin' AND source='manual'",
    )
    .bind(owner)
    .fetch_one(&pool)
    .await
    .unwrap();
    let mapping = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,'https://issuer.test','admins','platform','admin')")
        .bind(mapping)
        .execute(&pool)
        .await
        .unwrap();
    let group_grant = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO platform_role_grants(id,user_id,role,source,mapping_id) VALUES($1,$2,'admin','group',$3)",
    )
    .bind(group_grant)
    .bind(owner)
    .bind(mapping)
    .execute(&pool)
    .await
    .unwrap();
    let token = create(&pool, owner, "t", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();

    // T1: the manual demotion path's first step (directory.rs).
    let mut tx1 = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(owner)
        .fetch_one(&mut *tx1)
        .await
        .unwrap();

    // T2: the group-mapping deletion's grant revoke (directory.rs), spawned.
    let pool2 = pool.clone();
    let t2 = tokio::spawn(async move {
        let mut tx2 = pool2.begin().await.unwrap();
        let result = sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE id=$1")
            .bind(group_grant)
            .execute(&mut *tx2)
            .await;
        match result {
            Ok(_) => {
                tx2.commit().await.unwrap();
                Ok(())
            }
            Err(e) => {
                let _ = tx2.rollback().await;
                Err(e)
            }
        }
    });

    // Let T2 reach its blocking point (unfixed) or finish entirely (fixed).
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // T1: the manual demotion's own grant revoke.
    let result1 = sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE id=$1")
        .bind(manual_grant)
        .execute(&mut *tx1)
        .await;
    let outcome1 = match result1 {
        Ok(_) => {
            tx1.commit().await.unwrap();
            Ok(())
        }
        Err(e) => {
            let _ = tx1.rollback().await;
            Err(e)
        }
    };
    let outcome2 = t2.await.unwrap();

    for (label, outcome) in [("T1 (manual)", &outcome1), ("T2 (group)", &outcome2)] {
        if let Err(e) = outcome {
            assert!(!is_deadlock(e), "{label} hit SQLSTATE 40P01: {e}");
        }
    }
    assert!(
        outcome1.is_ok() || outcome2.is_ok(),
        "{outcome1:?} / {outcome2:?}"
    );

    let revoked: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM admin_api_tokens WHERE id=$1")
            .bind(token.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        revoked,
        "losing both admin grants must revoke the token even when neither side deadlocked"
    );
}

/// Coordinator review of #37 PR1, follow-up requirement 3: a manual
/// demotion run the way production code runs it -- `crate::db::begin`
/// (which sets `omg.scope_lock_audit=on` under `cfg(test)`) with the
/// user-scope lock pre-acquired up front via `governance::locks::exclusive`
/// -- must not trip 0027's "authority change without its scope lock taken
/// up front" audit check when the new admin_api_tokens trigger calls the
/// same `omg_scope_lock_exclusive` helper a second time (advisory locks are
/// re-entrant within a transaction, and the audit check only looks at
/// whether the lock is already held, not who is calling it).
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn manual_demotion_under_scope_lock_audit_mode_does_not_trip_the_audit_check(pool: PgPool) {
    let owner = admin(&pool, "audit@test.invalid").await;
    let token = create(&pool, owner, "t", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();

    let mut tx = crate::db::begin(&pool).await.unwrap();
    crate::governance::locks::exclusive(&mut tx, [crate::governance::locks::Scope::User(owner)])
        .await
        .unwrap();
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(owner)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let revoked: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM admin_api_tokens WHERE id=$1")
            .bind(token.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(revoked);
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
    let created = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    store
        .touch_admin_token_use(created.id, Some("203.0.113.5".parse().unwrap()))
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
        .touch_admin_token_use(created.id, Some("198.51.100.9".parse().unwrap()))
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
        .touch_admin_token_use(created.id, Some("198.51.100.9".parse().unwrap()))
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
    let owner = admin(&pool, "owner@test.invalid").await;
    let created = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
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

/// Coordinator review of #37 PR1, issue 1 (HIGH): the creation guard's
/// `omg_scope_lock_exclusive` serializes with a concurrent demotion, but
/// does not refresh an already-established REPEATABLE READ/SERIALIZABLE
/// snapshot. Repro: S1 opens a snapshot that observes the owner as a
/// currently effective admin; S2 revokes the owner's only admin grant and
/// commits; S1 then tries to create a token inside its stale snapshot.
/// Before the fix this `INSERT` succeeds even though the owner's admin
/// status was already gone (and committed) by the time of the insert: the
/// token is never revoked, and would become usable again if Admin is
/// re-granted. Bypasses the Rust helper (raw SQL against the trigger
/// directly), since the snapshot has to be established and held on a
/// specific transaction object the test controls.
async fn isolation_guard_refuses_a_stale_snapshot(pool: &PgPool, setup: &'static str, label: &str) {
    let owner = admin(pool, &format!("stale-{label}@test.invalid")).await;
    let mut tx = crate::db::begin_with_setup(pool, setup).await.unwrap();
    // Establish the snapshot: observe the owner as currently effective admin.
    let seen_admin: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM effective_platform_roles WHERE user_id=$1 AND role='admin')",
    )
    .bind(owner)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert!(
        seen_admin,
        "the snapshot must see the owner as admin before the concurrent revoke"
    );
    // A concurrent transaction revokes the owner's only admin grant and commits.
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(owner)
    .execute(pool)
    .await
    .unwrap();
    // The stale snapshot still attempts to create a token for this now-demoted owner.
    let id = Uuid::new_v4();
    let result = sqlx::query(
        "INSERT INTO admin_api_tokens(id,owner_user_id,name,secret_hash,scopes,expires_at)
         VALUES($1,$2,'stale',decode(repeat('11',32),'hex'),ARRAY['platform:read'],now()+interval '30 days')",
    )
    .bind(id)
    .bind(owner)
    .execute(&mut *tx)
    .await;
    let _ = tx.rollback().await;
    assert!(
        result.is_err(),
        "a {label} snapshot predating a committed demotion must not create a token for the now-demoted owner"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn creation_guard_refuses_a_stale_repeatable_read_snapshot(pool: PgPool) {
    isolation_guard_refuses_a_stale_snapshot(
        &pool,
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
        "rr",
    )
    .await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn creation_guard_refuses_a_stale_serializable_snapshot(pool: PgPool) {
    isolation_guard_refuses_a_stale_snapshot(
        &pool,
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        "serializable",
    )
    .await;
}

/// The common case (the gateway's own isolation level) is unaffected: a
/// plain READ COMMITTED creation still works.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn creation_guard_allows_read_committed(pool: PgPool) {
    let owner = admin(&pool, "read-committed@test.invalid").await;
    let mut tx = crate::db::begin(&pool).await.unwrap();
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO admin_api_tokens(id,owner_user_id,name,secret_hash,scopes,expires_at)
         VALUES($1,$2,'rc',decode(repeat('11',32),'hex'),ARRAY['platform:read'],now()+interval '30 days')",
    )
    .bind(id)
    .bind(owner)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// The role-loss check (`omg_admin_tokens_role_lost`) has the same
/// snapshot problem in principle (it also does a fresh `EXISTS` after
/// taking the lock): refuse it under RR/SERIALIZABLE too, even though no
/// production code path runs a `platform_role_grants` write at an elevated
/// isolation level today (verified by grep; this is pure defense in depth).
async fn role_loss_guard_refuses_elevated_isolation(
    pool: &PgPool,
    setup: &'static str,
    label: &str,
) {
    let owner = admin(pool, &format!("role-loss-stale-{label}@test.invalid")).await;
    let mut tx = crate::db::begin_with_setup(pool, setup).await.unwrap();
    // The role-loss trigger is deferred to commit (coordinator review
    // issue 3), so its own refusal now surfaces at COMMIT, not at the
    // UPDATE statement itself.
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(owner)
    .execute(&mut *tx)
    .await
    .unwrap();
    let result = tx.commit().await;
    assert!(
        result.is_err(),
        "the role-loss guard must refuse to run under a {label} isolation level"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn role_loss_guard_refuses_repeatable_read(pool: PgPool) {
    role_loss_guard_refuses_elevated_isolation(
        &pool,
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
        "rr",
    )
    .await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn role_loss_guard_refuses_serializable(pool: PgPool) {
    role_loss_guard_refuses_elevated_isolation(
        &pool,
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        "serializable",
    )
    .await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn role_loss_guard_allows_read_committed(pool: PgPool) {
    let owner = admin(&pool, "role-loss-rc@test.invalid").await;
    let mut tx = crate::db::begin(&pool).await.unwrap();
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(owner)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// Coordinator review of #37 PR1, issue 2 (HIGH): PR2's handler is
/// documented to hold `governance::locks::exclusive(Scope::User(owner))`
/// in its own transaction before calling `create_admin_token`, exactly as
/// this test does. Before the fix this ran `create_admin_token` on a
/// *different* connection (`&self.pool`); its `INSERT`'s owner-admin guard
/// trigger then tried to take that exact same transaction-scoped advisory
/// lock, which blocked until the caller's own transaction ended -- but the
/// caller could not end its transaction until this call returned. Running
/// on the caller's own `tx`, as it does now, makes the trigger's lock
/// acquisition re-entrant (free) instead, so this returns immediately.
/// Bounded with a timeout so a regression cannot hang the whole suite.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn create_must_not_hang_when_the_caller_already_holds_the_owners_scope_lock(pool: PgPool) {
    let owner = admin(&pool, "hang-create@test.invalid").await;
    let mut tx = crate::db::begin(&pool).await.unwrap();
    crate::governance::locks::exclusive(&mut tx, [crate::governance::locks::Scope::User(owner)])
        .await
        .unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        create_admin_token(&mut tx, owner, "n", &scopes(&["platform:read"]), 30, None),
    )
    .await;
    let created = result
        .expect("create_admin_token hung while the caller's own transaction already held the owner's scope lock (PR2's documented contract)")
        .unwrap();
    tx.commit().await.unwrap();
    assert!(!revoked(&pool, created.id).await);
}

/// Coordinator review of #37 PR1, issue 2 (HIGH), companion requirement:
/// `revoke_admin_token` must run on the caller's own transaction too (so
/// it is atomic with whatever else that transaction does, e.g. audit
/// insertion, and rolls back with it), not commit on a separate
/// connection the instant it is called.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn revoke_runs_on_the_callers_transaction_and_rolls_back_with_it(pool: PgPool) {
    let owner = admin(&pool, "revoke-tx@test.invalid").await;
    let token = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    let mut tx = crate::db::begin(&pool).await.unwrap();
    assert!(revoke_admin_token(&mut tx, token.id).await.unwrap());
    tx.rollback().await.unwrap();
    assert!(
        !revoked(&pool, token.id).await,
        "revoke_admin_token must run on the caller's own transaction, so rolling it back undoes the revocation"
    );
    // The converse: committing the caller's transaction commits the revoke.
    let mut tx = crate::db::begin(&pool).await.unwrap();
    assert!(revoke_admin_token(&mut tx, token.id).await.unwrap());
    tx.commit().await.unwrap();
    assert!(revoked(&pool, token.id).await);
}

/// Coordinator review of #37 PR1, issue 3 (MEDIUM): a routine group
/// re-sync must not destroy tokens for an admin who never actually lost
/// Admin. `lifecycle::apply_group_grants` revokes stale group grants in
/// one statement, then inserts the replacements in the next -- for
/// example, an operator recreating an IdP group mapping (delete + insert
/// with a new id, same group value and role, the practical effect of
/// renaming it in the UI): the old mapping row is gone, so the grant tied
/// to its id is revoked by the first statement, and a fresh grant tied to
/// the new mapping's id is inserted by the very next statement, in the
/// same transaction. Before the fix, the immediate (`AFTER ROW`)
/// role-loss trigger fires at the end of the first statement, before the
/// replacement exists, and revokes every token the user owns, even though
/// they never lost Admin.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_group_mapping_recreation_that_replaces_the_grant_does_not_destroy_the_token(
    pool: PgPool,
) {
    let owner = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'group-resync@test.invalid')")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    let old_mapping = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,'https://issuer.test','admins','platform','admin')")
        .bind(old_mapping)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(id,user_id,role,source,mapping_id) VALUES($1,$2,'admin','group',$3)")
        .bind(Uuid::new_v4())
        .bind(owner)
        .bind(old_mapping)
        .execute(&pool)
        .await
        .unwrap();
    let token = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();

    // The operator recreates the mapping: same group value and role, a new id.
    sqlx::query("DELETE FROM oidc_group_mappings WHERE id=$1")
        .bind(old_mapping)
        .execute(&pool)
        .await
        .unwrap();
    let new_mapping = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,'https://issuer.test','admins','platform','admin')")
        .bind(new_mapping)
        .execute(&pool)
        .await
        .unwrap();

    // lifecycle.rs's exact two-statement replacement, replayed in one transaction.
    let mut tx = crate::db::begin(&pool).await.unwrap();
    crate::lifecycle::apply_group_grants(
        &mut tx,
        owner,
        "https://issuer.test",
        &["admins".to_string()],
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    assert!(
        !revoked(&pool, token.id).await,
        "a group mapping recreation that replaces the grant in the same transaction must not revoke the owner's tokens"
    );
}

/// Companion to the above: a real group grant loss with no replacement
/// (the user's claimed groups no longer include the admin group at all)
/// must still revoke the owner's tokens, once the transaction commits.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_real_group_grant_loss_with_no_replacement_still_revokes_at_commit(pool: PgPool) {
    let owner = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'group-loss@test.invalid')")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    let mapping = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,'https://issuer.test','admins','platform','admin')")
        .bind(mapping)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(id,user_id,role,source,mapping_id) VALUES($1,$2,'admin','group',$3)")
        .bind(Uuid::new_v4())
        .bind(owner)
        .bind(mapping)
        .execute(&pool)
        .await
        .unwrap();
    let token = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();

    let mut tx = crate::db::begin(&pool).await.unwrap();
    crate::lifecycle::apply_group_grants(&mut tx, owner, "https://issuer.test", &[], None)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert!(
        revoked(&pool, token.id).await,
        "losing the group grant with no replacement must still revoke the owner's tokens, at commit"
    );
}

/// Coordinator review of #37 PR1, issue 4 (MEDIUM): migration 0027 treats
/// `UPDATE OF revoked_at,user_id,role OR DELETE` on `platform_role_grants`
/// as authority changes, but 0050's role-loss trigger only watched
/// `revoked_at`. These three shapes left a token unrevoked and usable
/// again after a re-grant, even though the owner's only `admin` row was
/// gone. The runtime role itself has none of these three privileges
/// (verified in `verify-privileges.sql`); these tests run as the
/// migrator/owner role to exercise the trigger directly, the same way a
/// human operator with direct database access could.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn changing_the_role_away_from_admin_revokes_owned_tokens(pool: PgPool) {
    let owner = admin(&pool, "role-change@test.invalid").await;
    let token = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    sqlx::query("UPDATE platform_role_grants SET role='auditor' WHERE user_id=$1 AND role='admin'")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        revoked(&pool, token.id).await,
        "UPDATE ... SET role (away from admin) must revoke the owner's tokens"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn deleting_the_grant_row_revokes_owned_tokens(pool: PgPool) {
    let owner = admin(&pool, "grant-delete@test.invalid").await;
    let token = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    sqlx::query("DELETE FROM platform_role_grants WHERE user_id=$1 AND role='admin'")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        revoked(&pool, token.id).await,
        "DELETE of the owner's only admin grant row must revoke the owner's tokens"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn moving_the_grant_to_another_user_revokes_the_original_owners_tokens(pool: PgPool) {
    let owner = admin(&pool, "grant-move-from@test.invalid").await;
    // A plain user with no grant of its own, so the moved row does not
    // collide with the active_platform_grant uniqueness constraint.
    let other = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'grant-move-to@test.invalid')")
        .bind(other)
        .execute(&pool)
        .await
        .unwrap();
    let token = create(&pool, owner, "n", &scopes(&["platform:read"]), 30, None)
        .await
        .unwrap();
    sqlx::query("UPDATE platform_role_grants SET user_id=$2 WHERE user_id=$1 AND role='admin'")
        .bind(owner)
        .bind(other)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        revoked(&pool, token.id).await,
        "moving the owner's only admin grant row to another user must revoke the original owner's tokens"
    );
}
