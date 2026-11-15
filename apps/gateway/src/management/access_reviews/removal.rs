//! A "remove" decision on an access review item: the access is removed at
//! once, in the decision's transaction, through the direct routes' paths.
use super::*;

/// An item as a removal needs it: kind, workspace, person (none for a
/// service key), role (none for a key), platform grant, key lineage, and
/// whether the viewer may decide it.
type RemovalTarget = (
    String,
    Option<Uuid>,
    Option<Uuid>,
    Option<String>,
    Option<Uuid>,
    Option<Uuid>,
    bool,
);

/// A "remove" decision (#90 slice 3): the access is removed at once, in the
/// same transaction, through the path the direct routes use (memberships:
/// the members path, owner-only for owners, the last owner protected, keys
/// revoked once no membership remains; platform roles: the role path, under
/// the exclusive catalog lock, entitlement loss as when revoking directly;
/// keys (slice 6): the revoke path, every key of the lineage revoked).
/// A refused removal records nothing. Locks before any check or write; the
/// same 404 as any decision for an item the viewer may not decide.
#[cfg_attr(not(test), expect(clippy::too_many_lines, reason = "#499"))]
pub(super) async fn remove(
    s: &Store,
    u: &BrowserPrincipal,
    item: Uuid,
) -> Result<Json<Value>, ManagementError> {
    // Authorized first, without any lock: someone who may not decide the
    // item never takes a removal's locks (the exclusive catalog lock, or the
    // person's scope). Everything is checked again under the locks.
    let found: Option<RemovalTarget> = sqlx::query_as(
        "SELECT subject_kind, workspace_id, user_id, role, platform_grant_id, key_lineage_id, access_review_may_decide(id,$2) FROM access_review_items WHERE id=$1",
    )
    .bind(item)
    .bind(u.user_id)
    .fetch_optional(&s.pool)
    .await?;
    let Some((kind, ws, subject, role, platform_grant, lineage, true)) = found else {
        return Err(missing().into());
    };
    let mut tx = if kind == "workspace_key" {
        let (ws, lineage) = ws.zip(lineage).ok_or_else(missing)?;
        let mut tx = resources::installation_tx(s).await?;
        // As the revoke route: the workspace row, then the key lineage
        // exclusively (in-flight admissions of the key finish first), here
        // with the reviewer's own scope in the same canonical call.
        sqlx::query("SELECT 1 FROM workspaces WHERE id=$1 FOR NO KEY UPDATE")
            .bind(ws)
            .execute(&mut *tx)
            .await?;
        locks::exclusive(
            &mut tx,
            [
                locks::Scope::Lineage(lineage),
                locks::Scope::User(u.user_id),
            ],
        )
        .await?;
        tx
    } else if kind == "workspace_membership" {
        let (ws, subject) = ws.zip(subject).ok_or_else(missing)?;
        let mut tx = resources::installation_tx(s).await?;
        // The workspace row, as the members routes lock it, then the
        // member's and their key lineages' scopes with the reviewer's own in
        // one canonical call. The reviewer's scope is taken exclusively (the
        // call has one mode): it only holds the reviewer's own admission for
        // the transaction, and keeps a concurrent demotion of the reviewer
        // ordered with this removal.
        sqlx::query("SELECT 1 FROM workspaces WHERE id=$1 FOR NO KEY UPDATE")
            .bind(ws)
            .execute(&mut *tx)
            .await?;
        members::lock_member_removal(&mut tx, u, ws, subject, vec![locks::Scope::User(u.user_id)])
            .await?;
        tx
    } else {
        // The role path needs the exclusive catalog lock, which requires a
        // Platform Admin: anyone else may not decide a platform item (404).
        match resources::catalog_tx(s, u, true).await {
            Ok(tx) => tx,
            Err(e) if e.0 == StatusCode::FORBIDDEN => return Err(missing().into()),
            Err(e) => return Err(e.into()),
        }
    };
    resources::platform_role(&mut tx, u.user_id).await?;
    sqlx::query("SELECT 1 FROM access_review_campaigns c JOIN access_review_items i ON i.campaign_id=c.id WHERE i.id=$1 FOR SHARE OF c")
        .bind(item)
        .fetch_optional(&mut *tx)
        .await?;
    let (may, active, source): (bool, bool, Option<String>) = sqlx::query_as(
        "SELECT access_review_may_decide(i.id,$2), access_review_item_active(i.id), i.source FROM access_review_items i WHERE i.id=$1",
    )
    .bind(item)
    .bind(u.user_id)
    .fetch_one(&mut *tx)
    .await?;
    if !may {
        return Err(missing().into());
    }
    if !active {
        return Err(ManagementError::detailed(
            StatusCode::CONFLICT,
            "access_review_item_inactive",
            "This access is no longer in effect; there is nothing to decide".into(),
            json!({}),
        ));
    }
    if source.as_deref() == Some("group") {
        return Err(ManagementError::detailed(
            StatusCode::CONFLICT,
            "access_review_remove_requires_manual",
            "Access provided by an identity provider group is removed at the identity provider: flag it instead".into(),
            json!({}),
        ));
    }
    if kind == "workspace_membership" && role.as_deref() == Some("owner") {
        let allowed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM effective_workspace_memberships WHERE workspace_id=$1 AND user_id=$2 AND role='owner') OR EXISTS(SELECT 1 FROM effective_platform_roles WHERE user_id=$2 AND role='admin')")
            .bind(ws)
            .bind(u.user_id)
            .fetch_one(&mut *tx)
            .await?;
        if !allowed {
            return Err(ManagementError::detailed(
                StatusCode::CONFLICT,
                "access_review_owner_removal",
                "Only an owner of the workspace or a Platform Admin removes an owner".into(),
                json!({}),
            ));
        }
    }
    sqlx::query("INSERT INTO access_review_decisions(id,item_id,decision,decided_by) VALUES($1,$2,'remove',$3)")
        .bind(Uuid::now_v7())
        .bind(item)
        .bind(u.user_id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        u,
        ws,
        "access_review.decided",
        "access_review_item",
        Some(item),
        json!({"kind":"remove","role":role}),
    )
    .await?;
    // Only the item's own grant: another grant of the same role (manual,
    // bootstrap or group) is a separate item, and stays. A key item: every
    // key of its lineage, as revoking each directly (never re-enabled).
    let removed = match (kind.as_str(), ws, subject, role.as_deref(), lineage) {
        ("workspace_key", Some(ws), _, _, Some(lineage)) => {
            revoke_lineage(&mut tx, u, ws, lineage).await
        }
        ("workspace_membership", Some(ws), Some(subject), _, _) => {
            members::revoke_member_locked(&mut tx, u, ws, subject).await
        }
        ("platform_role", None, Some(subject), Some(role), _) => {
            directory::revoke_role_locked(&mut tx, u, subject, role, platform_grant).await
        }
        _ => Err(missing()),
    };
    if let Err(e) = removed {
        if e.0 == StatusCode::CONFLICT {
            return Err(ManagementError::detailed(
                StatusCode::CONFLICT,
                "access_review_removal_refused",
                e.1.to_string(),
                json!({}),
            ));
        }
        return Err(e.into());
    }
    let result = one_item(&mut tx, item, false).await?;
    tx.commit().await?;
    Ok(result)
}

/// Revokes every unrevoked key of a lineage once the removal holds its locks,
/// each audited `key.revoked` as on the revoke route.
async fn revoke_lineage(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    ws: Uuid,
    lineage: Uuid,
) -> Result<(), ApiError> {
    let keys: Vec<Uuid> = sqlx::query_scalar("UPDATE api_keys SET revoked_at=coalesce(revoked_at,now()) WHERE workspace_id=$1 AND governance_key_id=$2 AND revoked_at IS NULL RETURNING id")
        .bind(ws)
        .bind(lineage)
        .fetch_all(&mut **tx)
        .await?;
    for key in keys {
        audit(tx, u, Some(ws), "key.revoked", "key", Some(key), json!({})).await?;
    }
    Ok(())
}
