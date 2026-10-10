use super::*;
use crate::auth::NewApiKey;

async fn sync(store: &Store, user: Uuid, groups: &[String]) -> bool {
    let mut tx = store.pool.begin().await.unwrap();
    lock(&mut tx).await.unwrap();
    let active = synchronize_groups(&mut tx, user, "https://id.test", groups)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    active
}
async fn setup(pool: &sqlx::PgPool) -> (Store, Uuid, Uuid, Uuid) {
    let user = Uuid::new_v4();
    let shared = Uuid::new_v4();
    let mapping = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'one@example.test')")
        .bind(user)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Project','project')")
        .bind(shared)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,'https://id.test','access','platform','auditor')").bind(mapping).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,workspace_id,workspace_role) VALUES($1,'https://id.test','access','workspace',$2,'admin')").bind(Uuid::new_v4()).bind(shared).execute(pool).await.unwrap();
    let store = Store::new(pool.clone());
    assert!(sync(&store, user, &["access".into()]).await);
    (store, user, shared, mapping)
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn independent_provenance_manual_membership_and_roles_survive_group_changes(
    pool: sqlx::PgPool,
) {
    let (store, user, shared, _) = setup(&pool).await;
    sqlx::query("INSERT INTO workspace_membership_grants(user_id,workspace_id,role,source) VALUES($1,$2,'member','manual')").bind(user).bind(shared).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'user','manual')")
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    assert!(sync(&store, user, &[]).await);
    let role: String =
        sqlx::query_scalar("SELECT role FROM effective_platform_roles WHERE user_id=$1")
            .bind(user)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(role, "user");
    let role: String = sqlx::query_scalar(
        "SELECT role FROM effective_workspace_memberships WHERE workspace_id=$1 AND user_id=$2",
    )
    .bind(shared)
    .bind(user)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(role, "member");
    assert!(sync(&store, user, &["access".into()]).await);
    let role: String = sqlx::query_scalar(
        "SELECT role FROM effective_workspace_memberships WHERE workspace_id=$1 AND user_id=$2",
    )
    .bind(shared)
    .bind(user)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(role, "admin");
    // Disabling/deleting mappings retires only their active grants on the next signin.
    sqlx::query("DELETE FROM oidc_group_mappings")
        .execute(&pool)
        .await
        .unwrap();
    assert!(sync(&store, user, &["access".into()]).await);
    let active_groups: i64=sqlx::query_scalar("SELECT count(*) FROM platform_role_grants WHERE user_id=$1 AND source='group' AND revoked_at IS NULL").bind(user).fetch_one(&pool).await.unwrap();
    assert_eq!(active_groups, 0);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn loss_reactivation_cleanup_preserves_history_and_service_credentials(pool: sqlx::PgPool) {
    let (store, user, shared, _) = setup(&pool).await;
    let personal: Uuid = sqlx::query_scalar("SELECT id FROM workspaces WHERE owner_user_id=$1")
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
    let human = NewApiKey::generate();
    let service = NewApiKey::generate();
    let account = Uuid::new_v4();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'Human',$4)").bind(human.id).bind(personal).bind(user).bind(human.digest.as_slice()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO service_accounts(id,workspace_id,name) VALUES($1,$2,'Robot')")
        .bind(account)
        .bind(shared)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,service_account_id,name,secret_hash) VALUES($1,$2,$3,'Service',$4)").bind(service.id).bind(shared).bind(account).bind(service.digest.as_slice()).execute(&pool).await.unwrap();
    let token = vec![1u8; 32];
    sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email) VALUES($1,$2,$1,now()+interval '1 day','one@example.test')").bind(token).bind(user).execute(&pool).await.unwrap();
    let provider = Uuid::new_v4();
    let model = Uuid::new_v4();
    let deployment = Uuid::new_v4();
    let execution = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Cloud','openai','env:TEST_KEY')").bind(provider).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO models(id,public_name) VALUES($1,'test')")
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,'test')").bind(deployment).bind(model).bind(provider).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id) VALUES($1,$2,$3,$4,'test','openai',false,'started',$1)").bind(execution).bind(personal).bind(human.id).bind(deployment).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state) SELECT $1,$2,$3,$4,e.started_at,now(),now(),now(),'unknown' FROM inference_executions e WHERE e.id=$1").bind(execution).bind(personal).bind(human.id).bind(deployment).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO monetary_ledger(id,execution_id,kind,admitted_at) SELECT $1,$2,'unknown',admitted_at FROM governance_reservations WHERE execution_id=$2")
        .bind(Uuid::new_v4())
        .bind(execution)
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.authenticate(&human.token).await.unwrap().is_some());
    assert!(!sync(&store, user, &[]).await);
    let days: bool = sqlx::query_scalar(
        "SELECT cleanup_due_at >= disabled_at+interval '30 days' FROM users WHERE id=$1",
    )
    .bind(user)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(days);
    assert!(store.authenticate(&human.token).await.unwrap().is_none());
    assert!(store.authenticate(&service.token).await.unwrap().is_some());
    assert!(sync(&store, user, &["access".into()]).await);
    assert!(store.authenticate(&human.token).await.unwrap().is_none()); // revoked keys never return
    let active_sessions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM browser_sessions WHERE user_id=$1 AND revoked_at IS NULL",
    )
    .bind(user)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active_sessions, 0);
    // An admin suspension is not overridden by matching groups.
    sqlx::query("UPDATE users SET disabled_at=now(),disable_reason='admin_suspension',cleanup_due_at=now()+interval '30 days' WHERE id=$1").bind(user).execute(&pool).await.unwrap();
    assert!(!sync(&store, user, &["access".into()]).await);
    sqlx::query("UPDATE users SET disable_reason='entitlement_loss',cleanup_due_at=now()-interval '1 second' WHERE id=$1").bind(user).execute(&pool).await.unwrap();
    assert_eq!(cleanup_inactive_accounts(&store).await.unwrap(), 1);
    assert_eq!(cleanup_inactive_accounts(&store).await.unwrap(), 0);
    let tombstone: (Option<String>, bool) =
        sqlx::query_as("SELECT email,cleaned_at IS NOT NULL FROM users WHERE id=$1")
            .bind(user)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(tombstone, (None, true));
    let sessions: Vec<(bool, Option<String>)> = sqlx::query_as(
        "SELECT revoked_at IS NOT NULL,verified_email FROM browser_sessions WHERE user_id=$1",
    )
    .bind(user)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(sessions, vec![(true, None)]);
    assert!(store.authenticate(&service.token).await.unwrap().is_some());
    let history: (i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM inference_executions),(SELECT count(*) FROM governance_reservations),(SELECT count(*) FROM monetary_ledger)").fetch_one(&pool).await.unwrap();
    assert_eq!(history, (1, 1, 1));
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workspace_membership_grants WHERE user_id=$1 AND revoked_at IS NULL",
    )
    .bind(user)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active, 0);
    assert!(
        sqlx::query("DELETE FROM monetary_ledger")
            .execute(&pool)
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn shared_group_membership_loss_retires_keys_without_restoring_them_on_return(
    pool: sqlx::PgPool,
) {
    let (store, user, shared, _) = setup(&pool).await;
    sqlx::query("INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'user','manual')")
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    let issued = NewApiKey::generate();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'Shared human',$4)").bind(issued.id).bind(shared).bind(user).bind(issued.digest.as_slice()).execute(&pool).await.unwrap();
    assert!(store.authenticate(&issued.token).await.unwrap().is_some());
    assert!(sync(&store, user, &[]).await); // manual platform entitlement is independent
    assert!(store.authenticate(&issued.token).await.unwrap().is_none());
    assert!(sync(&store, user, &["access".into()]).await);
    assert!(store.authenticate(&issued.token).await.unwrap().is_none());
    let member: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM effective_workspace_memberships WHERE workspace_id=$1 AND user_id=$2)").bind(shared).bind(user).fetch_one(&pool).await.unwrap();
    assert!(member);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_includes_user_without_separate_grant(pool: sqlx::PgPool) {
    let (store, user, _, _) = setup(&pool).await;
    let personal: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workspaces WHERE kind='personal' AND owner_user_id=$1",
    )
    .bind(user)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(personal, 1);
    assert!(sync(&store, user, &["access".into()]).await);
    let personal: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workspaces WHERE kind='personal' AND owner_user_id=$1",
    )
    .bind(user)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(personal, 1);
    let grants: Vec<String> = sqlx::query_scalar(
        "SELECT role FROM platform_role_grants WHERE user_id=$1 AND revoked_at IS NULL",
    )
    .bind(user)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(grants, ["auditor"]);
}
