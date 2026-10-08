//! Entitlement grace periods retain attribution; cleanup never deletes users or history.
use crate::store::Store;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

/// Global order: catalog advisory lock, installation row, then identity/account rows.
pub(crate) async fn lock(tx: &mut Transaction<'_, Postgres>) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")
        .execute(&mut **tx)
        .await?;
    sqlx::query("SELECT lock_installation()")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(crate) async fn cleanup_user(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE browser_sessions SET revoked_at=coalesce(revoked_at,now()),verified_email=NULL WHERE user_id=$1",
    )
    .bind(user)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE api_keys SET revoked_at=coalesce(revoked_at,now()) WHERE issued_to_user_id=$1",
    )
    .bind(user)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=coalesce(revoked_at,now()) WHERE user_id=$1",
    )
    .bind(user)
    .execute(&mut **tx)
    .await?;
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=coalesce(revoked_at,now()) WHERE user_id=$1").bind(user).execute(&mut **tx).await?;
    sqlx::query("UPDATE workspaces SET disabled_at=coalesce(disabled_at,now()) WHERE kind='personal' AND owner_user_id=$1").bind(user).execute(&mut **tx).await?;
    sqlx::query("UPDATE workspace_invitations SET revoked_at=coalesce(revoked_at,now()) WHERE accepted_at IS NULL AND lower(email)=(SELECT lower(email) FROM users WHERE id=$1)").bind(user).execute(&mut **tx).await?;
    sqlx::query("UPDATE users SET email=NULL,display_name=NULL,oidc_link_allowed=false,cleaned_at=now(),disabled_at=coalesce(disabled_at,now()) WHERE id=$1 AND cleaned_at IS NULL").bind(user).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,action,resource_type,resource_id) VALUES($1,NULL,'user.cleaned','user',$2)")
        .bind(Uuid::new_v4()).bind(user).execute(&mut **tx).await?;
    Ok(())
}

/// Bounded maintenance entry point. Service accounts and immutable accounting are untouched.
pub async fn cleanup_inactive_accounts(store: &Store) -> Result<u64, sqlx::Error> {
    let mut tx = store.pool.begin().await?;
    lock(&mut tx).await?;
    let users: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM users WHERE disabled_at IS NOT NULL AND cleaned_at IS NULL AND cleanup_due_at<=now() ORDER BY cleanup_due_at,id LIMIT 100 FOR UPDATE")
        .fetch_all(&mut *tx).await?;
    for user in &users {
        cleanup_user(&mut tx, *user).await?;
    }
    tx.commit().await?;
    Ok(users.len() as u64)
}

/// Only call with a well-formed, signature-verified group claim. Empty groups are authoritative.
/// Manual grants survive group synchronization. Missing/malformed claims never call this function.
pub(crate) async fn synchronize_groups(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
    issuer: &str,
    groups: &[String],
) -> Result<bool, sqlx::Error> {
    sqlx::query("UPDATE platform_role_grants g SET revoked_at=now() WHERE g.user_id=$1 AND g.source='group' AND g.revoked_at IS NULL AND
        (NOT EXISTS(SELECT 1 FROM oidc_group_mappings m WHERE m.id=g.mapping_id) OR EXISTS(SELECT 1 FROM oidc_group_mappings m WHERE m.id=g.mapping_id AND m.issuer=$2))
        AND NOT EXISTS(SELECT 1 FROM oidc_group_mappings m WHERE m.id=g.mapping_id AND m.issuer=$2 AND m.enabled AND m.target_kind='platform' AND m.platform_role=g.role AND m.group_value=ANY($3))")
        .bind(user).bind(issuer).bind(groups).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO platform_role_grants(id,user_id,role,source,mapping_id) SELECT gen_random_uuid(),$1,m.platform_role,'group',m.id FROM oidc_group_mappings m WHERE m.issuer=$2 AND m.enabled AND m.target_kind='platform' AND m.group_value=ANY($3) AND NOT EXISTS(SELECT 1 FROM platform_role_grants g WHERE g.user_id=$1 AND g.mapping_id=m.id AND g.role=m.platform_role AND g.revoked_at IS NULL)")
        .bind(user).bind(issuer).bind(groups).execute(&mut **tx).await?;
    sqlx::query("UPDATE workspace_membership_grants g SET revoked_at=now() WHERE g.user_id=$1 AND g.source='group' AND g.revoked_at IS NULL AND
        (NOT EXISTS(SELECT 1 FROM oidc_group_mappings m WHERE m.id=g.mapping_id) OR EXISTS(SELECT 1 FROM oidc_group_mappings m WHERE m.id=g.mapping_id AND m.issuer=$2))
        AND NOT EXISTS(SELECT 1 FROM oidc_group_mappings m WHERE m.id=g.mapping_id AND m.issuer=$2 AND m.enabled AND m.target_kind='workspace' AND m.workspace_id=g.workspace_id AND m.workspace_role=g.role AND m.group_value=ANY($3))")
        .bind(user).bind(issuer).bind(groups).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO workspace_membership_grants(id,workspace_id,user_id,role,source,mapping_id) SELECT gen_random_uuid(),m.workspace_id,$1,m.workspace_role,'group',m.id FROM oidc_group_mappings m JOIN workspaces w ON w.id=m.workspace_id AND w.disabled_at IS NULL WHERE m.issuer=$2 AND m.enabled AND m.target_kind='workspace' AND m.group_value=ANY($3) AND NOT EXISTS(SELECT 1 FROM workspace_membership_grants g WHERE g.user_id=$1 AND g.mapping_id=m.id AND g.revoked_at IS NULL)")
        .bind(user).bind(issuer).bind(groups).execute(&mut **tx).await?;
    // Workspace group loss must not make an old credential usable again when a group returns.
    sqlx::query("UPDATE api_keys k SET revoked_at=now() FROM workspaces w WHERE k.workspace_id=w.id AND w.kind IN ('team','project') AND k.issued_to_user_id=$1 AND k.revoked_at IS NULL AND NOT EXISTS(SELECT 1 FROM workspace_membership_grants g WHERE g.workspace_id=w.id AND g.user_id=$1 AND g.revoked_at IS NULL)")
        .bind(user).execute(&mut **tx).await?;
    let entitled: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM platform_role_grants WHERE user_id=$1 AND revoked_at IS NULL)",
    )
    .bind(user)
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,action,resource_type,resource_id,metadata) VALUES($1,$2,'identity.groups_synchronized','user',$2,jsonb_build_object('source','group','entitled',$3::boolean))")
        .bind(Uuid::new_v4()).bind(user).bind(entitled).execute(&mut **tx).await?;
    if !entitled {
        sqlx::query("UPDATE users SET disabled_at=coalesce(disabled_at,now()),cleanup_due_at=coalesce(cleanup_due_at,now()+interval '30 days'),disable_reason=coalesce(disable_reason,'entitlement_loss') WHERE id=$1 AND cleaned_at IS NULL")
            .bind(user).execute(&mut **tx).await?;
        sqlx::query(
            "UPDATE browser_sessions SET revoked_at=now() WHERE user_id=$1 AND revoked_at IS NULL",
        )
        .bind(user)
        .execute(&mut **tx)
        .await?;
        sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE issued_to_user_id=$1 AND revoked_at IS NULL").bind(user).execute(&mut **tx).await?;
        return Ok(false);
    }
    // Any administrative disable reason overrides automatic reactivation.
    sqlx::query("UPDATE users SET disabled_at=NULL,cleanup_due_at=NULL,disable_reason=NULL WHERE id=$1 AND cleaned_at IS NULL AND disable_reason='entitlement_loss' AND cleanup_due_at>now()")
        .bind(user).execute(&mut **tx).await?;
    let active: bool = sqlx::query_scalar(
        "SELECT disabled_at IS NULL AND cleaned_at IS NULL FROM users WHERE id=$1",
    )
    .bind(user)
    .fetch_one(&mut **tx)
    .await?;
    if active {
        let ws: Uuid = sqlx::query_scalar("INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES($1,'Personal','personal',$2) ON CONFLICT(owner_user_id) WHERE kind='personal' AND disabled_at IS NULL DO UPDATE SET owner_user_id=EXCLUDED.owner_user_id RETURNING id")
            .bind(Uuid::new_v4()).bind(user).fetch_one(&mut **tx).await?;
        sqlx::query("INSERT INTO workspace_membership_grants(id,workspace_id,user_id,role,source) VALUES($1,$2,$3,'owner','manual') ON CONFLICT DO NOTHING")
            .bind(Uuid::new_v4()).bind(ws).bind(user).execute(&mut **tx).await?;
    }
    Ok(active)
}

#[cfg(all(test, feature = "integration-tests"))]
#[path = "lifecycle/tests.rs"]
mod tests;
