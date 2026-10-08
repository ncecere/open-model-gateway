use super::*;
async fn count(pool: &PgPool, table: &str) -> i64 {
    // Names are test-only constants, never HTTP input.
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .unwrap()
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn entry_refuses_non_dedicated_database_and_production(pool: PgPool) {
    let store = Store::new(pool.clone());
    assert!(seed(&store, Environment::Development).await.is_err());
    assert!(seed(&store, Environment::Production).await.is_err());
    assert_eq!(count(&pool, "users").await, 0);
    assert_eq!(count(&pool, "inference_executions").await, 0);
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn enterprise_demo_has_explicit_entitlement_and_no_fabricated_usage(pool: PgPool) {
    let store = Store::new(pool.clone());
    assert!(seed_data(&store).await.unwrap());
    assert_eq!(count(&pool, "users").await, 4);
    assert_eq!(count(&pool, "workspaces").await, 2); // personal only on entitled sign-in
    assert_eq!(count(&pool, "platform_role_grants").await, 0); // signed groups supply entitlement
    assert_eq!(count(&pool, "oidc_group_mappings").await, 6);
    assert_eq!(count(&pool, "catalogs").await, 3);
    assert_eq!(count(&pool, "models").await, 4);
    assert_eq!(count(&pool, "service_accounts").await, 1);
    for table in [
        "api_keys",
        "inference_executions",
        "governance_reservations",
        "monetary_ledger",
        "browser_sessions",
    ] {
        assert_eq!(count(&pool, table).await, 0, "{table}");
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM provider_connections WHERE enabled")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM deployments WHERE enabled")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM users WHERE lower(email)='unentitled@demo.invalid'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM deployment_prices WHERE pricing_version=2"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        4
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM models WHERE supported_protocols=ARRAY['embeddings']"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM policy_budgets WHERE layer='installation')"
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn seed_repeat_preserves_edits_and_disabled_accounts(pool: PgPool) {
    let store = Store::new(pool.clone());
    assert!(seed_data(&store).await.unwrap());
    sqlx::query("UPDATE workspaces SET name='Keep my edits' WHERE id=$1")
        .bind(id(20))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET disabled_at=now(),disable_reason='admin_suspension' WHERE id=$1")
        .bind(id(4))
        .execute(&pool)
        .await
        .unwrap();
    assert!(!seed_data(&store).await.unwrap());
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT name FROM workspaces WHERE id=$1")
            .bind(id(20))
            .fetch_one(&pool)
            .await
            .unwrap(),
        "Keep my edits"
    );
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT disabled_at IS NOT NULL FROM users WHERE id=$1")
            .bind(id(4))
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    assert_eq!(count(&pool, "users").await, 4);
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn seed_never_adopts_existing_user_or_demo_email_collision(pool: PgPool) {
    let store = Store::new(pool.clone());
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'operator@demo.invalid')")
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    assert!(seed_data(&store).await.is_err());
    assert_eq!(count(&pool, "users").await, 1);
    assert_eq!(count(&pool, "platform_role_grants").await, 0);
    assert_eq!(count(&pool, "oidc_group_mappings").await, 0);
    assert_eq!(count(&pool, "workspaces").await, 0);
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_seeds_are_idempotent(pool: PgPool) {
    let store = Store::new(pool.clone());
    let (a, b) = tokio::join!(seed_data(&store), seed_data(&store));
    assert_ne!(a.unwrap(), b.unwrap());
    assert_eq!(count(&pool, "users").await, 4);
    assert_eq!(count(&pool, "audit_events").await, 1);
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn failed_seed_rolls_back_entire_fixture(pool: PgPool) {
    let store = Store::new(pool.clone());
    sqlx::raw_sql("CREATE FUNCTION reject_demo_seed() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test rejection'; END $$; CREATE TRIGGER reject_demo BEFORE INSERT ON catalogs FOR EACH ROW EXECUTE FUNCTION reject_demo_seed()")
        .execute(&pool).await.unwrap();
    assert!(seed_data(&store).await.is_err());
    for table in [
        "users",
        "workspaces",
        "workspace_membership_grants",
        "oidc_group_mappings",
        "catalogs",
        "audit_events",
    ] {
        assert_eq!(count(&pool, table).await, 0, "{table}");
    }
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT name FROM installation")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "Enterprise"
    );
}
