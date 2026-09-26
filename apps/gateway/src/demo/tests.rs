use super::*;
use serde_json::Value;
use sqlx::PgPool;

// Include every table touched by provisioning and every table that must stay empty.
const TABLES: &[&str] = &[
    "organizations",
    "users",
    "organization_memberships",
    "workspaces",
    "workspace_memberships",
    "api_keys",
    "service_accounts",
    "provider_connections",
    "models",
    "deployments",
    "workspace_model_grants",
    "deployment_prices",
    "governance_policies",
    "platform_organization_policies",
    "organization_model_grants",
    "user_model_grants",
    "oidc_identities",
    "login_attempts",
    "browser_sessions",
    "invitations",
    "audit_events",
    "inference_executions",
    "governance_reservations",
    "monetary_ledger",
    "model_routing_policies",
    "deployment_routing",
    "deployment_route_health",
];

async fn snapshot(pool: &PgPool) -> Vec<Vec<Value>> {
    let mut result = Vec::new();
    for table in TABLES {
        result.push(
            sqlx::query_scalar(&format!(
                "SELECT to_jsonb(t) FROM {table} t ORDER BY to_jsonb(t)::text"
            ))
            .fetch_all(pool)
            .await
            .unwrap(),
        );
    }
    result
}

async fn count(pool: &PgPool, query: &str) -> i64 {
    sqlx::query_scalar(query).fetch_one(pool).await.unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn personas_have_exact_roles_and_private_ownership(pool: PgPool) {
    assert!(seed_inner(&Store::new(pool.clone())).await.unwrap());
    let organization: (String, String) = sqlx::query_as("SELECT slug,name FROM organizations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(organization, ("gateway-demo".into(), "Gateway demo".into()));
    let personas: Vec<(String, bool, bool, String, String)> = sqlx::query_as(
        "SELECT u.email,u.platform_admin,u.oidc_link_allowed,om.role,wm.role
         FROM users u JOIN organization_memberships om ON om.user_id=u.id
         JOIN workspace_memberships wm ON wm.user_id=u.id AND wm.organization_id=om.organization_id
         JOIN workspaces w ON w.id=wm.workspace_id WHERE w.name='Product' AND w.kind='team'
         ORDER BY u.email",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        personas,
        vec![
            (
                "alex@demo.invalid".into(),
                false,
                true,
                "member".into(),
                "admin".into()
            ),
            (
                "blair@demo.invalid".into(),
                false,
                true,
                "member".into(),
                "member".into()
            ),
            (
                "operator@demo.invalid".into(),
                true,
                true,
                "owner".into(),
                "owner".into()
            ),
            (
                "orgadmin@demo.invalid".into(),
                false,
                true,
                "admin".into(),
                "member".into()
            ),
        ]
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM users").await, 4);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM organization_memberships").await,
        4
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM workspace_memberships").await,
        4
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM workspaces WHERE kind='personal'"
        )
        .await,
        4
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(DISTINCT owner_user_id) FROM workspaces WHERE kind='personal'"
        )
        .await,
        4
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM workspace_memberships wm JOIN workspaces w ON w.id=wm.workspace_id WHERE w.kind='personal'").await, 0);
    assert_eq!(count(&pool, "SELECT count(*) FROM api_keys k JOIN workspaces w ON w.id=k.workspace_id WHERE w.kind='personal' AND k.issued_to_user_id IS DISTINCT FROM w.owner_user_id").await, 0);
    assert_eq!(count(&pool, "SELECT count(*) FROM service_accounts sa JOIN workspaces w ON w.id=sa.workspace_id WHERE w.kind<>'team'").await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn configuration_is_disabled_and_examples_are_not_usage(pool: PgPool) {
    assert!(seed_inner(&Store::new(pool.clone())).await.unwrap());
    let providers: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT provider,credential_ref,enabled FROM provider_connections ORDER BY provider",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        providers,
        vec![
            ("anthropic".into(), "env:ANTHROPIC_API_KEY".into(), false),
            ("openai".into(), "env:OPENAI_API_KEY".into(), false),
        ]
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM provider_connections WHERE endpoint IS NOT NULL OR region IS NOT NULL").await, 0);
    let models: Vec<(String, bool, bool)> = sqlx::query_as(
        "SELECT m.public_name,m.enabled,g.personal_enabled FROM models m JOIN organization_model_grants g ON g.model_id=m.id ORDER BY m.public_name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        models,
        vec![
            ("demo/fast".into(), true, true),
            ("demo/smart".into(), true, true)
        ]
    );
    for table in [
        "models",
        "provider_connections",
        "deployments",
        "deployment_prices",
    ] {
        assert_eq!(
            count(
                &pool,
                &format!("SELECT count(*) FROM {table} WHERE organization_id IS NOT NULL")
            )
            .await,
            0
        );
    }
    assert_eq!(
        count(&pool, "SELECT count(*) FROM organization_model_grants").await,
        2
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM platform_organization_policies WHERE requests_per_minute=120"
        )
        .await,
        1
    );
    let upstreams: Vec<String> =
        sqlx::query_scalar("SELECT upstream_model FROM deployments ORDER BY upstream_model")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        upstreams,
        vec![
            "claude-3-5-haiku-20241022",
            "claude-sonnet-4-20250514",
            "gpt-4.1",
            "gpt-4.1-mini"
        ]
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM deployments WHERE enabled").await,
        0
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM workspace_model_grants").await,
        10
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM workspaces w CROSS JOIN models m WHERE NOT EXISTS (SELECT 1 FROM workspace_model_grants g WHERE g.workspace_id=w.id AND g.model_id=m.id)").await, 0);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM deployment_prices").await,
        4
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM deployment_prices p JOIN deployments d ON d.id=p.deployment_id JOIN models m ON m.id=d.model_id WHERE m.display_name LIKE '%example prices, not vendor rates%'").await, 4);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM governance_policies").await,
        6
    );
    assert!(
        sqlx::query("UPDATE deployment_prices SET input_microusd_per_million=0")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM deployment_prices")
            .execute(&pool)
            .await
            .is_err()
    );
    for table in [
        "oidc_identities",
        "login_attempts",
        "browser_sessions",
        "invitations",
        "audit_events",
        "inference_executions",
        "governance_reservations",
        "monetary_ledger",
        "deployment_route_health",
    ] {
        assert_eq!(
            count(&pool, &format!("SELECT count(*) FROM {table}")).await,
            0,
            "{table}"
        );
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn keys_store_only_hashes_and_belong_to_each_persona(pool: PgPool) {
    assert!(seed_inner(&Store::new(pool.clone())).await.unwrap());
    let keys: Vec<(Uuid, Vec<u8>, Uuid, Option<Uuid>)> =
        sqlx::query_as("SELECT id,secret_hash,governance_key_id,issued_to_user_id FROM api_keys")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(keys.len(), 8);
    let mut hashes = std::collections::HashSet::new();
    for (id, hash, lineage, user) in keys {
        assert_eq!(hash.len(), 32);
        assert!(hashes.insert(hash));
        assert_eq!(id, lineage);
        assert!(user.is_some());
    }
    let ownership: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT u.email,count(*) FILTER (WHERE w.kind='personal'),count(*) FILTER (WHERE w.name='Product')
         FROM users u JOIN api_keys k ON k.issued_to_user_id=u.id
         JOIN workspaces w ON w.id=k.workspace_id GROUP BY u.email ORDER BY u.email",
    ).fetch_all(&pool).await.unwrap();
    assert_eq!(
        ownership,
        vec![
            ("alex@demo.invalid".into(), 1, 1),
            ("blair@demo.invalid".into(), 1, 1),
            ("operator@demo.invalid".into(), 1, 1),
            ("orgadmin@demo.invalid".into(), 1, 1),
        ]
    );
    // The token prefix must not appear in any persisted string/JSON field.
    assert!(
        !serde_json::to_string(&snapshot(&pool).await)
            .unwrap()
            .contains("omg_")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn repeats_preserve_user_edits_revocations_and_linking_state(pool: PgPool) {
    let store = Store::new(pool.clone());
    assert!(seed_inner(&store).await.unwrap());
    sqlx::query("UPDATE organizations SET name='My edited demo',disabled_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET oidc_link_allowed=false,platform_admin=false,disabled_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE api_keys SET revoked_at=now(),name='Revoked by user'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE provider_connections SET enabled=true")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE models SET enabled=false,personal_enabled=false")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM workspace_model_grants")
        .execute(&pool)
        .await
        .unwrap();
    let before = snapshot(&pool).await;
    assert!(!seed_inner(&store).await.unwrap());
    assert_eq!(before, snapshot(&pool).await);
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_seeds_create_only_once(pool: PgPool) {
    let store = Store::new(pool.clone());
    let (first, second) = tokio::join!(seed_inner(&store), seed_inner(&store));
    assert_ne!(first.unwrap(), second.unwrap());
    assert_eq!(count(&pool, "SELECT count(*) FROM organizations").await, 1);
    assert_eq!(count(&pool, "SELECT count(*) FROM users").await, 4);
    assert_eq!(count(&pool, "SELECT count(*) FROM api_keys").await, 8);
}

#[sqlx::test(migrations = "./migrations")]
async fn public_entry_rejects_production_and_sqlx_database_names(pool: PgPool) {
    let store = Store::new(pool.clone());
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_ne!(database, "gateway_demo");
    assert!(
        seed(&store, Environment::Production)
            .await
            .unwrap_err()
            .to_string()
            .contains("development")
    );
    assert!(
        seed(&store, Environment::Development)
            .await
            .unwrap_err()
            .to_string()
            .contains("gateway_demo")
    );
    assert!(
        add_missing_personas(&store, Environment::Production)
            .await
            .unwrap_err()
            .to_string()
            .contains("development")
    );
    assert!(
        add_missing_personas(&store, Environment::Development)
            .await
            .unwrap_err()
            .to_string()
            .contains("gateway_demo")
    );
    assert!(snapshot(&pool).await.iter().all(Vec::is_empty));
}

#[sqlx::test(migrations = "./migrations")]
async fn refuses_existing_organization_without_mutating_it(pool: PgPool) {
    sqlx::query("INSERT INTO organizations(id,slug,name) VALUES($1,'real','Real organization')")
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    let before = snapshot(&pool).await;
    assert!(seed_inner(&Store::new(pool.clone())).await.is_err());
    assert_eq!(before, snapshot(&pool).await);
}

#[sqlx::test(migrations = "./migrations")]
async fn refuses_existing_users_including_demo_email_collisions(pool: PgPool) {
    for email in ["real@example.test", "operator@demo.invalid"] {
        sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
            .bind(Uuid::new_v4())
            .bind(email)
            .execute(&pool)
            .await
            .unwrap();
        let before = snapshot(&pool).await;
        assert!(seed_inner(&Store::new(pool.clone())).await.is_err());
        assert_eq!(before, snapshot(&pool).await);
        sqlx::query("DELETE FROM users")
            .execute(&pool)
            .await
            .unwrap();
    }
}

// Reconstruct the previous three-persona seed only in disposable SQLx databases.
async fn legacy_seed(pool: &PgPool) -> Store {
    let store = Store::new(pool.clone());
    assert!(seed_inner(&store).await.unwrap());
    let user: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE email='orgadmin@demo.invalid'")
        .fetch_one(pool)
        .await
        .unwrap();
    for query in [
        "DELETE FROM api_keys WHERE issued_to_user_id=$1",
        "DELETE FROM governance_policies WHERE workspace_id IN (SELECT id FROM workspaces WHERE owner_user_id=$1)",
        "DELETE FROM workspace_model_grants WHERE workspace_id IN (SELECT id FROM workspaces WHERE owner_user_id=$1)",
        "DELETE FROM workspace_memberships WHERE user_id=$1",
        "DELETE FROM workspaces WHERE owner_user_id=$1",
        "DELETE FROM organization_memberships WHERE user_id=$1",
        "DELETE FROM users WHERE id=$1",
    ] {
        sqlx::query(query).bind(user).execute(pool).await.unwrap();
    }
    store
}

fn assert_only_additions(before: &[Vec<Value>], after: &[Vec<Value>]) {
    for ((table, before), after) in TABLES.iter().zip(before).zip(after) {
        for row in before {
            assert!(
                after.contains(row),
                "existing row changed in {table}: {row}"
            );
        }
        if ![
            "users",
            "organization_memberships",
            "workspaces",
            "workspace_memberships",
            "api_keys",
            "workspace_model_grants",
            "governance_policies",
        ]
        .contains(table)
        {
            assert_eq!(before, after, "unexpected additions in {table}");
        }
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn upgrade_preserves_edits_disabled_users_revocations_and_privileges(pool: PgPool) {
    let store = legacy_seed(&pool).await;
    sqlx::raw_sql(
        "UPDATE organizations SET name='Edited organization';
         UPDATE workspaces SET name='Renamed team' WHERE kind='team';
         UPDATE users SET oidc_link_allowed=false,platform_admin=false,disabled_at=now() WHERE email='operator@demo.invalid';
         UPDATE users SET platform_admin=true WHERE email='alex@demo.invalid';
         INSERT INTO oidc_identities(issuer,subject,user_id) SELECT 'http://127.0.0.1:18084','alex',id FROM users WHERE email='alex@demo.invalid';
         UPDATE organization_memberships SET role='member',disabled_at=now() WHERE user_id=(SELECT id FROM users WHERE email='operator@demo.invalid');
         UPDATE workspace_memberships SET role='member',disabled_at=now() WHERE user_id=(SELECT id FROM users WHERE email='alex@demo.invalid');
         UPDATE api_keys SET revoked_at=now(),name='Revoked by user';
         UPDATE provider_connections SET enabled=true;
         UPDATE organization_model_grants SET personal_enabled=false WHERE public_name='demo/fast';
         DELETE FROM workspace_model_grants;
         UPDATE governance_policies SET monthly_budget_microusd=1000000;",
    ).execute(&pool).await.unwrap();
    let before = snapshot(&pool).await;
    // Default remains a strict no-op, even when the fourth persona is absent.
    assert!(!seed_inner(&store).await.unwrap());
    assert_eq!(before, snapshot(&pool).await);
    assert!(add_missing_personas_inner(&store).await.unwrap());
    let after = snapshot(&pool).await;
    assert_only_additions(&before, &after);
    let persona: (bool, bool, String, String) = sqlx::query_as(
        "SELECT u.platform_admin,u.oidc_link_allowed,om.role,wm.role FROM users u
         JOIN organization_memberships om ON om.user_id=u.id
         JOIN workspace_memberships wm ON wm.user_id=u.id AND wm.organization_id=om.organization_id
         WHERE u.email='orgadmin@demo.invalid'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(persona, (false, true, "admin".into(), "member".into()));
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM api_keys WHERE revoked_at IS NULL"
        )
        .await,
        2
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM workspace_model_grants").await,
        1
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM workspace_memberships wm JOIN workspaces w ON w.id=wm.workspace_id WHERE w.kind='personal'").await, 0);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM governance_policies WHERE monthly_budget_microusd=5000000"
        )
        .await,
        1
    );
    assert!(!serde_json::to_string(&after).unwrap().contains("omg_"));
    assert!(!add_missing_personas_inner(&store).await.unwrap());
    assert_eq!(after, snapshot(&pool).await);
    // Once added, even disabling/demoting the new persona never gets undone.
    sqlx::raw_sql("UPDATE users SET disabled_at=now(),oidc_link_allowed=false WHERE email='orgadmin@demo.invalid';
        UPDATE organization_memberships SET role='member',disabled_at=now() WHERE user_id=(SELECT id FROM users WHERE email='orgadmin@demo.invalid');
        UPDATE api_keys SET revoked_at=now();").execute(&pool).await.unwrap();
    let disabled = snapshot(&pool).await;
    assert!(!add_missing_personas_inner(&store).await.unwrap());
    assert_eq!(disabled, snapshot(&pool).await);
}

#[sqlx::test(migrations = "./migrations")]
async fn upgrade_email_collision_never_adopts_or_grants_privileges(pool: PgPool) {
    let store = legacy_seed(&pool).await;
    sqlx::raw_sql("INSERT INTO organizations(id,slug,name) VALUES('11111111-1111-1111-1111-111111111111','other','Other tenant');
        INSERT INTO users(id,email,oidc_link_allowed,disabled_at) VALUES('22222222-2222-2222-2222-222222222222','OrgAdmin@DEMO.invalid',false,now());
        INSERT INTO oidc_identities(issuer,subject,user_id) VALUES('https://existing.example','original-subject','22222222-2222-2222-2222-222222222222');
        INSERT INTO organization_memberships(organization_id,user_id,role) VALUES('11111111-1111-1111-1111-111111111111','22222222-2222-2222-2222-222222222222','member');")
        .execute(&pool).await.unwrap();
    let before = snapshot(&pool).await;
    assert!(!add_missing_personas_inner(&store).await.unwrap());
    assert_eq!(before, snapshot(&pool).await);
    // Active accounts are not adopted either.
    sqlx::query("UPDATE users SET disabled_at=NULL WHERE lower(email)='orgadmin@demo.invalid'")
        .execute(&pool)
        .await
        .unwrap();
    let before = snapshot(&pool).await;
    assert!(!add_missing_personas_inner(&store).await.unwrap());
    assert_eq!(before, snapshot(&pool).await);
}

#[sqlx::test(migrations = "./migrations")]
async fn upgrade_requires_existing_active_demo_organization(pool: PgPool) {
    let store = Store::new(pool.clone());
    assert!(add_missing_personas_inner(&store).await.is_err());
    assert!(snapshot(&pool).await.iter().all(Vec::is_empty));
    let store = legacy_seed(&pool).await;
    sqlx::query("UPDATE organizations SET disabled_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    let before = snapshot(&pool).await;
    assert!(add_missing_personas_inner(&store).await.is_err());
    assert_eq!(before, snapshot(&pool).await);
    sqlx::query("UPDATE organizations SET disabled_at=NULL,slug='renamed'")
        .execute(&pool)
        .await
        .unwrap();
    let before = snapshot(&pool).await;
    assert!(add_missing_personas_inner(&store).await.is_err());
    assert_eq!(before, snapshot(&pool).await);
}

#[sqlx::test(migrations = "./migrations")]
async fn upgrade_without_active_team_only_adds_private_workspace(pool: PgPool) {
    let store = legacy_seed(&pool).await;
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE kind='team'")
        .execute(&pool)
        .await
        .unwrap();
    let before = snapshot(&pool).await;
    assert!(add_missing_personas_inner(&store).await.unwrap());
    assert_only_additions(&before, &snapshot(&pool).await);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM workspace_memberships").await,
        3
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM api_keys").await, 7);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM workspaces WHERE kind='team'").await,
        1
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_upgrades_create_only_one_persona(pool: PgPool) {
    let store = legacy_seed(&pool).await;
    let (first, second, seed) = tokio::join!(
        add_missing_personas_inner(&store),
        add_missing_personas_inner(&store),
        seed_inner(&store)
    );
    assert_ne!(first.unwrap(), second.unwrap());
    assert!(!seed.unwrap());
    assert_eq!(count(&pool, "SELECT count(*) FROM users").await, 4);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM organization_memberships").await,
        4
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM workspace_memberships").await,
        4
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM workspaces").await, 5);
    assert_eq!(count(&pool, "SELECT count(*) FROM api_keys").await, 8);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM workspace_model_grants").await,
        10
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM governance_policies").await,
        6
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_email_insert_is_never_adopted(pool: PgPool) {
    let store = legacy_seed(&pool).await;
    let outsider = Uuid::new_v4();
    let (upgrade, insert) = tokio::join!(
        add_missing_personas_inner(&store),
        sqlx::query_scalar::<_, Uuid>("INSERT INTO users(id,email) VALUES($1,'ORGADMIN@demo.invalid') ON CONFLICT DO NOTHING RETURNING id")
            .bind(outsider).fetch_optional(&pool),
    );
    let inserted = insert.unwrap().is_some();
    assert_eq!(upgrade.unwrap(), !inserted);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM users WHERE lower(email)='orgadmin@demo.invalid'"
        )
        .await,
        1
    );
    let memberships: i64 =
        sqlx::query_scalar("SELECT count(*) FROM organization_memberships WHERE user_id=$1")
            .bind(outsider)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(memberships, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn failed_upgrade_rolls_back_all_additions(pool: PgPool) {
    let store = legacy_seed(&pool).await;
    sqlx::raw_sql(
        "CREATE FUNCTION reject_demo_policy() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN RAISE EXCEPTION 'injected upgrade failure'; END; $$;
        CREATE TRIGGER reject_demo_policy BEFORE INSERT ON governance_policies
        FOR EACH ROW EXECUTE FUNCTION reject_demo_policy();",
    )
    .execute(&pool)
    .await
    .unwrap();
    let before = snapshot(&pool).await;
    assert!(
        add_missing_personas_inner(&store)
            .await
            .unwrap_err()
            .to_string()
            .contains("injected upgrade failure")
    );
    assert_eq!(before, snapshot(&pool).await);
}

#[sqlx::test(migrations = "./migrations")]
async fn failures_roll_back_every_seeded_row(pool: PgPool) {
    sqlx::raw_sql(
        "CREATE FUNCTION reject_demo_deployment() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'injected failure'; END; $$;
         CREATE TRIGGER reject_demo_deployment BEFORE INSERT ON deployments
         FOR EACH ROW EXECUTE FUNCTION reject_demo_deployment();",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(seed_inner(&Store::new(pool.clone())).await.is_err());
    assert!(snapshot(&pool).await.iter().all(Vec::is_empty));
}
