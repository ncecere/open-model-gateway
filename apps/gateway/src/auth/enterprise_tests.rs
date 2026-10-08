use super::*;
use crate::lifecycle;

async fn key(
    pool: &sqlx::PgPool,
    ws: Uuid,
    user: Option<Uuid>,
    service: Option<Uuid>,
    lineage: Option<Uuid>,
) -> NewApiKey {
    let key = NewApiKey::generate();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,service_account_id,name,secret_hash,governance_key_id) VALUES($1,$2,$3,$4,'Test',$5,$6)")
        .bind(key.id).bind(ws).bind(user).bind(service).bind(key.digest.as_slice()).bind(lineage).execute(pool).await.unwrap();
    key
}
async fn admission(store: &Store, principal: &Principal) -> Option<Uuid> {
    let mut tx = store.pool.begin().await.unwrap();
    lifecycle::lock(&mut tx).await.unwrap();
    let value = revalidate(&mut tx, principal).await.unwrap();
    tx.commit().await.unwrap();
    value
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn global_admin_still_requires_actual_shared_membership_and_live_entitlement(
    pool: sqlx::PgPool,
) {
    let store = Store::new(pool.clone());
    let user = Uuid::new_v4();
    let ws = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'admin@test.invalid')")
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'admin','manual')",
    )
    .bind(user)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Team','team')")
        .bind(ws)
        .execute(&pool)
        .await
        .unwrap();
    let issued = key(&pool, ws, Some(user), None, None).await;
    let principal = Principal {
        key_id: issued.id,
        workspace_id: ws,
        user_id: Some(user),
    };
    assert!(store.authenticate(&issued.token).await.unwrap().is_none());
    assert_eq!(admission(&store, &principal).await, None);
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,'member','manual')").bind(ws).bind(user).execute(&pool).await.unwrap();
    assert!(store.authenticate(&issued.token).await.unwrap().is_some());
    assert_eq!(admission(&store, &principal).await, Some(issued.id));
    let fake = format!(
        "{}{}{}",
        &issued.token[..37],
        if &issued.token[37..38] == "a" {
            "b"
        } else {
            "a"
        },
        &issued.token[38..]
    );
    assert_eq!(token_id(&fake), Some(issued.id));
    assert!(store.authenticate(&fake).await.unwrap().is_none());
    let rotated = key(&pool, ws, Some(user), None, Some(issued.id)).await;
    let rotated_principal = Principal {
        key_id: rotated.id,
        ..principal
    };
    assert_eq!(admission(&store, &rotated_principal).await, Some(issued.id));
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.authenticate(&rotated.token).await.unwrap().is_none());
    assert_eq!(admission(&store, &rotated_principal).await, None);
    sqlx::query("UPDATE platform_role_grants SET revoked_at=NULL WHERE user_id=$1")
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(admission(&store, &principal).await, None);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn workspace_foreign_keys_and_personal_privacy_are_enforced(pool: sqlx::PgPool) {
    let store = Store::new(pool.clone());
    let owner = Uuid::new_v4();
    let outsider = Uuid::new_v4();
    let personal = Uuid::new_v4();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let sa = Uuid::new_v4();
    for user in [owner, outsider] {
        sqlx::query("INSERT INTO users(id) VALUES($1)")
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'admin','manual')",
        )
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES($1,'Private','personal',$2)",
    )
    .bind(personal)
    .bind(owner)
    .execute(&pool)
    .await
    .unwrap();
    for ws in [a, b] {
        sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Shared','project')")
            .bind(ws)
            .execute(&pool)
            .await
            .unwrap();
    }
    assert!(
        sqlx::query("INSERT INTO service_accounts(id,workspace_id,name) VALUES($1,$2,'Forbidden')")
            .bind(Uuid::new_v4())
            .bind(personal)
            .execute(&pool)
            .await
            .is_err()
    );
    sqlx::query("INSERT INTO service_accounts(id,workspace_id,name) VALUES($1,$2,'Worker')")
        .bind(sa)
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();
    let service = key(&pool, a, None, Some(sa), None).await;
    let digest = NewApiKey::generate();
    assert!(sqlx::query("INSERT INTO api_keys(id,workspace_id,service_account_id,name,secret_hash) VALUES($1,$2,$3,'Wrong',$4)").bind(digest.id).bind(b).bind(sa).bind(digest.digest.as_slice()).execute(&pool).await.is_err());
    assert!(sqlx::query("INSERT INTO api_keys(id,workspace_id,service_account_id,name,secret_hash,governance_key_id) VALUES($1,$2,$3,'Wrong lineage',$4,$5)").bind(digest.id).bind(a).bind(sa).bind(digest.digest.as_slice()).bind(Uuid::new_v4()).execute(&pool).await.is_err());
    let foreign = key(&pool, personal, Some(outsider), None, None).await;
    assert!(store.authenticate(&foreign.token).await.unwrap().is_none());
    assert_eq!(
        admission(
            &store,
            &Principal {
                key_id: foreign.id,
                workspace_id: personal,
                user_id: Some(outsider)
            }
        )
        .await,
        None
    );
    let human = key(&pool, personal, Some(owner), None, None).await;
    assert!(store.authenticate(&human.token).await.unwrap().is_some());
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.authenticate(&human.token).await.unwrap().is_none());
    assert!(store.authenticate(&service.token).await.unwrap().is_some());
    sqlx::query("UPDATE service_accounts SET disabled_at=now() WHERE id=$1")
        .bind(sa)
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.authenticate(&service.token).await.unwrap().is_none());
    assert_eq!(
        admission(
            &store,
            &Principal {
                key_id: service.id,
                workspace_id: a,
                user_id: None
            }
        )
        .await,
        None
    );
}
