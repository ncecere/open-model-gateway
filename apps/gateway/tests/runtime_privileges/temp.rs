//! Called by the existing runtime ACL test on its fresh migrated database,
//! under its role-setup lock. Never reset or migrate an existing installation.
//! The gateway database must be dedicated: revoking PUBLIC TEMP affects every
//! role relying on that grant in this database, not just gateway_runtime.
//! Owner/migrator privileges and all unrelated databases must be preserved.
use open_model_gateway::store::Store;
use sqlx::PgPool;

pub(super) async fn check_runtime_temp_grants(pool: &PgPool) {
    // PUBLIC-only default and upgrade from explicit runtime + PUBLIC TEMP.
    for explicit_runtime_grant in [false, true] {
        check_runtime_temp_grants_case(pool.clone(), explicit_runtime_grant).await;
    }
}

async fn check_runtime_temp_grants_case(pool: PgPool, explicit_runtime_grant: bool) {
    // Target only the database SQLx created for this test. PUBLIC TEMP models a
    // normal PostgreSQL database, unlike staging's already-hardened initializer.
    sqlx::raw_sql("DO $$ BEGIN EXECUTE format('REVOKE TEMPORARY ON DATABASE %I FROM gateway_runtime', current_database()); EXECUTE format('GRANT TEMPORARY ON DATABASE %I TO PUBLIC', current_database()); END $$;")
        .execute(&pool)
        .await
        .unwrap();
    if explicit_runtime_grant {
        sqlx::raw_sql("DO $$ BEGIN EXECUTE format('GRANT TEMPORARY ON DATABASE %I TO gateway_runtime', current_database()); END $$;")
            .execute(&pool)
            .await
            .unwrap();
    }
    runtime_temp_table(&pool)
        .await
        .expect("fixture must expose runtime TEMP before grants apply");
    let unrelated_before = unrelated_database_acls(&pool).await;
    let owner_grants_before = owner_database_grants(&pool).await;
    let store = Store::new(pool.clone());
    let mut first_acl = None;
    for application in 1..=2 {
        // Exercise the embedded SQL through the actual grants apply mechanism,
        // including schema preflight and migration serialization, not a SQL copy.
        store.apply_runtime_grants().await.unwrap();
        assert_eq!(
            unrelated_database_acls(&pool).await,
            unrelated_before,
            "grants apply must not alter any other database's ACL"
        );
        // Check owner ACLs explicitly too: a superuser test connection could
        // otherwise mask an accidental revocation of the owner's own privileges.
        assert_eq!(owner_database_grants(&pool).await, owner_grants_before);
        // The fixture's connecting role owns its database and runs its migrations.
        // It must retain TEMP even though PUBLIC and runtime both lose it.
        let owner_is_migrator: bool = sqlx::query_scalar(
            "SELECT datdba=(SELECT oid FROM pg_roles WHERE rolname=current_user) FROM pg_database WHERE datname=current_database()",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(owner_is_migrator, "fixture must run as its database owner");
        let mut owner_tx = pool.begin().await.unwrap();
        sqlx::query("CREATE TEMP TABLE omg_owner_temp_probe(id integer) ON COMMIT DROP")
            .execute(&mut *owner_tx)
            .await
            .expect("database owner/migrator must retain TEMP");
        owner_tx.rollback().await.unwrap();

        let error = runtime_temp_table(&pool)
            .await
            .expect_err("grants apply must deny runtime CREATE TEMP TABLE");
        assert_eq!(
            error.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("42501"),
            "runtime TEMP probe must fail with insufficient_privilege"
        );
        let (runtime_temp, public_temp): (bool, bool) = sqlx::query_as(
            "SELECT has_database_privilege('gateway_runtime',current_database(),'TEMPORARY'), EXISTS(SELECT FROM pg_database d CROSS JOIN LATERAL aclexplode(coalesce(d.datacl,acldefault('d',d.datdba))) a WHERE d.datname=current_database() AND a.grantee=0 AND a.privilege_type='TEMPORARY')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            !runtime_temp,
            "runtime must have no effective TEMP privilege"
        );
        assert!(!public_temp, "PUBLIC must not restore runtime TEMP");
        let acl: Option<String> = sqlx::query_scalar(
            "SELECT datacl::text FROM pg_database WHERE datname=current_database()",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if application == 1 {
            first_acl = Some(acl);
        } else {
            assert_eq!(Some(acl), first_acl, "grants apply must be idempotent");
        }
    }
}

async fn unrelated_database_acls(pool: &PgPool) -> Vec<(String, Option<String>)> {
    sqlx::query_as(
        "SELECT datname::text,datacl::text FROM pg_database WHERE datname<>current_database() ORDER BY datname",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn owner_database_grants(pool: &PgPool) -> Vec<(String, bool)> {
    sqlx::query_as(
        "SELECT a.privilege_type,a.is_grantable FROM pg_database d CROSS JOIN LATERAL aclexplode(coalesce(d.datacl,acldefault('d',d.datdba))) a WHERE d.datname=current_database() AND a.grantee=d.datdba ORDER BY a.privilege_type,a.is_grantable",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn runtime_temp_table(pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL ROLE gateway_runtime")
        .execute(&mut *tx)
        .await?;
    let result = sqlx::query("CREATE TEMP TABLE omg_runtime_temp_probe(id integer) ON COMMIT DROP")
        .execute(&mut *tx)
        .await;
    // Even an unexpected successful CREATE leaves no object or role change.
    tx.rollback().await?;
    result.map(|_| ())
}
