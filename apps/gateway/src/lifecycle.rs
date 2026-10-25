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

/// Exclusive authority locks (`governance::locks`) for changes to `users`
/// (suspension, cleanup, entitlement loss, grant and membership changes, key
/// revocation): their personal workspaces, the users, then the lineages of
/// keys issued to them, in canonical order. Call before the first write, so
/// the triggers' lazy acquisition never runs out of order.
pub(crate) async fn lock_users(
    tx: &mut Transaction<'_, Postgres>,
    users: &[Uuid],
) -> Result<(), sqlx::Error> {
    use crate::governance::locks::{Scope, exclusive};
    if users.is_empty() {
        return Ok(());
    }
    let rows: Vec<(i16, Uuid)> = sqlx::query_as("SELECT 1::int2,id FROM workspaces WHERE kind='personal' AND owner_user_id=ANY($1) UNION SELECT 2::int2,u FROM unnest($1::uuid[]) u UNION SELECT 3::int2,governance_key_id FROM api_keys WHERE issued_to_user_id=ANY($1)")
        .bind(users)
        .fetch_all(&mut **tx)
        .await?;
    exclusive(
        tx,
        rows.into_iter().map(|(kind, id)| match kind {
            1 => Scope::Workspace(id),
            2 => Scope::User(id),
            _ => Scope::Lineage(id),
        }),
    )
    .await
}

/// Remove a departed user's personal data. `Ok(false)` while the user is on
/// legal hold (#75): checked after the caller's user lock, in a statement of
/// its own, so a hold committed meanwhile is seen.
pub(crate) async fn cleanup_user(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
) -> Result<bool, sqlx::Error> {
    let held: bool = sqlx::query_scalar(&format!("SELECT {}", crate::legal_holds::user_held("$1")))
        .bind(user)
        .fetch_one(&mut **tx)
        .await?;
    if held {
        return Ok(false);
    }
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
    // Admin API tokens (#37 PR1): the owner-invalidation trigger already
    // revoked every token this user owns the instant `platform_role_grants`
    // below loses their last `admin` row (or, independently, the moment
    // `users.cleaned_at` below is set). The address is kept until then (it
    // helps investigate misuse of a high-risk credential), but a cleaned-up
    // account is gone for good, so scrub it here like the rest of this
    // function's address/identity copies (docs/identity.md's retention
    // note next to the sessions statement).
    sqlx::query("UPDATE admin_api_tokens SET last_used_address=NULL WHERE owner_user_id=$1")
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
    // Other copies of the person's email and their files' names (#76): an
    // accepted invitation's address, alert recipients and uploaded filenames.
    // (Before the email itself is cleared below.)
    // Not inside a team or project on legal hold (#75): its data stays as is.
    let ws_held = |column: &str| crate::legal_holds::workspace_held(column);
    sqlx::query(&format!("UPDATE workspace_invitations i SET email='removed@removed.invalid' WHERE lower(i.email)=(SELECT lower(email) FROM users WHERE id=$1) AND NOT {}", ws_held("i.workspace_id"))).bind(user).execute(&mut **tx).await?;
    sqlx::query(&format!("UPDATE alert_rules r SET notify_emails=array(SELECT e FROM unnest(r.notify_emails) e WHERE lower(e)<>(SELECT lower(email) FROM users WHERE id=$1)),updated_at=now() WHERE EXISTS(SELECT 1 FROM unnest(r.notify_emails) e WHERE lower(e)=(SELECT lower(email) FROM users WHERE id=$1)) AND NOT {}", ws_held("r.workspace_id"))).bind(user).execute(&mut **tx).await?;
    sqlx::query(&format!("UPDATE stored_files f SET filename=NULL WHERE f.created_by_user_id=$1 AND f.filename IS NOT NULL AND NOT {}", ws_held("f.workspace_id"))).bind(user).execute(&mut **tx).await?;
    // SCIM (0014): directory attributes and group membership go with the email.
    sqlx::query("UPDATE scim_users SET user_name=NULL,external_id=NULL,given_name=NULL,family_name=NULL,active=false,updated_at=now() WHERE user_id=$1").bind(user).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM scim_group_members WHERE user_id=$1")
        .bind(user)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE users SET email=NULL,display_name=NULL,oidc_link_allowed=false,cleaned_at=now(),disabled_at=coalesce(disabled_at,now()) WHERE id=$1 AND cleaned_at IS NULL").bind(user).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,action,resource_type,resource_id) VALUES($1,NULL,'user.cleaned','user',$2)")
        .bind(Uuid::new_v4()).bind(user).execute(&mut **tx).await?;
    Ok(true)
}

/// Bounded maintenance entry point. Service accounts and immutable accounting are untouched.
pub async fn cleanup_inactive_accounts(store: &Store) -> Result<u64, sqlx::Error> {
    cleanup_inactive_accounts_fenced(store, None).await
}

/// [`cleanup_inactive_accounts`] as the `lifecycle` lease holder: the
/// transaction is fenced to the term, and a lock-free check first skips the
/// catalog/installation locks entirely when nothing is due (the common case;
/// before P5 every replica took the installation row every minute).
pub async fn cleanup_inactive_accounts_fenced(
    store: &Store,
    fence: Option<&crate::leases::Fence>,
) -> Result<u64, sqlx::Error> {
    let due: bool = sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM users WHERE disabled_at IS NOT NULL AND cleaned_at IS NULL AND cleanup_due_at<=now() AND NOT {})", crate::legal_holds::user_held("users.id")))
        .fetch_one(&store.pool)
        .await?;
    if !due {
        return Ok(0);
    }
    let mut tx = crate::db::begin(&store.pool).await?;
    crate::leases::fence(&mut tx, fence).await?;
    lock(&mut tx).await?;
    let users: Vec<Uuid> = sqlx::query_scalar(&format!("SELECT id FROM users WHERE disabled_at IS NOT NULL AND cleaned_at IS NULL AND cleanup_due_at<=now() AND NOT {} ORDER BY cleanup_due_at,id LIMIT 100 FOR UPDATE", crate::legal_holds::user_held("users.id")))
        .fetch_all(&mut *tx).await?;
    lock_users(&mut tx, &users).await?;
    let mut cleaned = 0;
    for user in &users {
        if cleanup_user(&mut tx, *user).await? {
            cleaned += 1;
        }
    }
    tx.commit().await?;
    Ok(cleaned)
}

/// Grant/mapping identity: grant `g` is currently sourced from mapping `m`
/// (`g.mapping_id=m.id`). Every provenance/revoke check below -- and the
/// preview evaluator (`management::group_mapping_preview`, #89) -- joins on
/// this exact text, so a grant's source mapping is decided one way only.
pub(crate) fn group_match_predicate() -> &'static str {
    "m.id=g.mapping_id"
}
/// Whether mapping `m` is enabled and targets `kind` (`"platform"` or
/// `"workspace"`), independent of any existing grant: the condition for
/// creating a fresh group-sourced grant. `m` is either a real
/// `oidc_group_mappings` row (the write path) or a one-row proposal CTE of
/// the same column names (the preview evaluator) -- the text never changes.
pub(crate) fn group_enabled_for(kind: &str) -> &'static str {
    if kind == "platform" {
        "m.enabled AND m.target_kind='platform'"
    } else {
        "m.enabled AND m.target_kind='workspace'"
    }
}
/// Whether mapping `m`, if enabled, currently produces grant `g`'s own role
/// (same workspace too, for a workspace target). `apply_group_grants`'s
/// revoke step and the preview evaluator's "would this grant survive" check
/// share this exact text -- one source of truth for "what does this mapping
/// do," never a parallel reimplementation that could drift.
pub(crate) fn group_target_predicate(kind: &str) -> String {
    if kind == "platform" {
        format!("{} AND m.platform_role=g.role", group_enabled_for(kind))
    } else {
        format!(
            "{} AND m.workspace_id=g.workspace_id AND m.workspace_role=g.role",
            group_enabled_for(kind)
        )
    }
}
/// Group-provenance grants (`source='group'`) for one issuer; manual/bootstrap grants are
/// never touched. `scope=None` reconciles every mapping of the issuer (a signed sign-in
/// claim, plus mappings that no longer exist). `scope=Some(values)` reconciles only
/// mappings whose group value is in `values` (SCIM-managed groups).
pub(crate) async fn apply_group_grants(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
    issuer: &str,
    groups: &[String],
    scope: Option<&[String]>,
) -> Result<(), sqlx::Error> {
    let gm = group_match_predicate();
    sqlx::query(&format!("UPDATE platform_role_grants g SET revoked_at=now() WHERE g.user_id=$1 AND g.source='group' AND g.revoked_at IS NULL AND
        CASE WHEN $4::text[] IS NULL THEN (NOT EXISTS(SELECT 1 FROM oidc_group_mappings m WHERE {gm}) OR EXISTS(SELECT 1 FROM oidc_group_mappings m WHERE {gm} AND m.issuer=$2))
        ELSE EXISTS(SELECT 1 FROM oidc_group_mappings m WHERE {gm} AND m.issuer=$2 AND m.group_value=ANY($4)) END
        AND NOT EXISTS(SELECT 1 FROM oidc_group_mappings m WHERE {gm} AND m.issuer=$2 AND {target} AND m.group_value=ANY($3))",
        target = group_target_predicate("platform")))
        .bind(user).bind(issuer).bind(groups).bind(scope).execute(&mut **tx).await?;
    sqlx::query(&format!("INSERT INTO platform_role_grants(id,user_id,role,source,mapping_id) SELECT gen_random_uuid(),$1,m.platform_role,'group',m.id FROM oidc_group_mappings m WHERE m.issuer=$2 AND {enabled} AND m.group_value=ANY($3) AND ($4::text[] IS NULL OR m.group_value=ANY($4)) AND NOT EXISTS(SELECT 1 FROM platform_role_grants g WHERE g.user_id=$1 AND {gm} AND g.role=m.platform_role AND g.revoked_at IS NULL)",
        enabled = group_enabled_for("platform")))
        .bind(user).bind(issuer).bind(groups).bind(scope).execute(&mut **tx).await?;
    sqlx::query(&format!("UPDATE workspace_membership_grants g SET revoked_at=now() WHERE g.user_id=$1 AND g.source='group' AND g.revoked_at IS NULL AND
        CASE WHEN $4::text[] IS NULL THEN (NOT EXISTS(SELECT 1 FROM oidc_group_mappings m WHERE {gm}) OR EXISTS(SELECT 1 FROM oidc_group_mappings m WHERE {gm} AND m.issuer=$2))
        ELSE EXISTS(SELECT 1 FROM oidc_group_mappings m WHERE {gm} AND m.issuer=$2 AND m.group_value=ANY($4)) END
        AND NOT EXISTS(SELECT 1 FROM oidc_group_mappings m WHERE {gm} AND m.issuer=$2 AND {target} AND m.group_value=ANY($3))",
        target = group_target_predicate("workspace")))
        .bind(user).bind(issuer).bind(groups).bind(scope).execute(&mut **tx).await?;
    sqlx::query(&format!("INSERT INTO workspace_membership_grants(id,workspace_id,user_id,role,source,mapping_id) SELECT gen_random_uuid(),m.workspace_id,$1,m.workspace_role,'group',m.id FROM oidc_group_mappings m JOIN workspaces w ON w.id=m.workspace_id AND w.disabled_at IS NULL WHERE m.issuer=$2 AND {enabled} AND m.group_value=ANY($3) AND ($4::text[] IS NULL OR m.group_value=ANY($4)) AND NOT EXISTS(SELECT 1 FROM workspace_membership_grants g WHERE g.user_id=$1 AND {gm} AND g.revoked_at IS NULL)",
        enabled = group_enabled_for("workspace")))
        .bind(user).bind(issuer).bind(groups).bind(scope).execute(&mut **tx).await?;
    // Workspace group loss must not make an old credential usable again when a group returns.
    sqlx::query("UPDATE api_keys k SET revoked_at=now() FROM workspaces w WHERE k.workspace_id=w.id AND w.kind IN ('team','project') AND k.issued_to_user_id=$1 AND k.revoked_at IS NULL AND NOT EXISTS(SELECT 1 FROM workspace_membership_grants g WHERE g.workspace_id=w.id AND g.user_id=$1 AND g.revoked_at IS NULL)")
        .bind(user).execute(&mut **tx).await?;
    Ok(())
}

/// Bounded, read-only simulation of [`apply_group_grants`] for one proposed
/// mapping (#89): never writes, never audits, never bumps a config version.
/// `m` below is a one-row `VALUES` CTE with the same column names an
/// `oidc_group_mappings` row has, so [`group_enabled_for`] and
/// [`group_target_predicate`] run unchanged against it -- the same text the
/// real write path runs against the table.
pub(crate) struct GroupGrantProposal<'a> {
    /// `None` for a brand-new mapping (no existing grants can reference it yet).
    pub(crate) existing_mapping_id: Option<Uuid>,
    pub(crate) target_kind: &'a str,
    pub(crate) platform_role: Option<&'a str>,
    pub(crate) workspace_id: Option<Uuid>,
    pub(crate) workspace_role: Option<&'a str>,
    /// `false` for a delete preview: an about-to-be-removed mapping never
    /// produces a grant, regardless of its stored `enabled` value.
    pub(crate) enabled: bool,
}
/// One of the four bounded lists the preview evaluator reports. A single
/// mapping targets exactly one of platform/workspace, so in the common case
/// (create, or an edit that keeps its target kind) only one gained and one
/// lost category ever has rows; an edit that changes target kind entirely
/// can populate one of each pair.
#[derive(Clone, Copy)]
pub(crate) enum GroupGrantCategory {
    PlatformGained,
    PlatformLost,
    WorkspaceGained,
    WorkspaceLost,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct GroupGrantDiffRow {
    pub(crate) user_id: Uuid,
    pub(crate) role: String,
    pub(crate) workspace_id: Option<Uuid>,
}
/// One bounded category's page: up to `limit` rows plus the true total, so
/// the caller can report `{data,total,truncated}`.
pub(crate) struct GroupGrantDiffPage {
    pub(crate) rows: Vec<GroupGrantDiffRow>,
    pub(crate) total: i64,
}
/// Bounded, read-only simulation of one category of [`apply_group_grants`]
/// for one proposed mapping (#89): never writes, never audits, never bumps a
/// config version. `roster` is the bounded set of users to evaluate for a
/// *gained* category (SCIM roster or sign-in-derived current holders, never
/// a foreign personal workspace); a *lost* category needs no roster, since
/// it only ever draws from grants already carrying `proposal`'s own
/// `existing_mapping_id`. Every row excludes disabled/cleaned-up users and
/// never touches `api_keys`: a deactivated user or a revoked key never
/// reactivates here, because neither write path above does. `m` below is a
/// one-row `VALUES` CTE with the same column names an `oidc_group_mappings`
/// row has, so [`group_enabled_for`] and [`group_target_predicate`] run
/// unchanged against it -- the same text the real write path runs against
/// the table.
pub(crate) async fn preview_group_grants(
    tx: &mut Transaction<'_, Postgres>,
    proposal: &GroupGrantProposal<'_>,
    roster: &[Uuid],
    category: GroupGrantCategory,
    limit: i64,
) -> Result<GroupGrantDiffPage, sqlx::Error> {
    let gm = group_match_predicate();
    let select = match category {
        GroupGrantCategory::PlatformGained => format!(
            "SELECT u.id AS user_id,m.platform_role AS role,NULL::uuid AS workspace_id
             FROM unnest($1::uuid[]) u(id) CROSS JOIN m
             JOIN users usr ON usr.id=u.id AND usr.disabled_at IS NULL AND usr.cleaned_at IS NULL
             WHERE {enabled} AND NOT EXISTS(SELECT 1 FROM platform_role_grants g WHERE g.user_id=u.id AND g.role=m.platform_role AND g.revoked_at IS NULL)",
            enabled = group_enabled_for("platform"),
        ),
        GroupGrantCategory::PlatformLost => format!(
            "SELECT g.user_id AS user_id,g.role AS role,NULL::uuid AS workspace_id
             FROM platform_role_grants g CROSS JOIN m
             JOIN users usr ON usr.id=g.user_id AND usr.disabled_at IS NULL AND usr.cleaned_at IS NULL
             WHERE g.source='group' AND g.revoked_at IS NULL AND {gm} AND NOT ({target})
             AND NOT EXISTS(SELECT 1 FROM platform_role_grants g2 WHERE g2.user_id=g.user_id AND g2.role=g.role AND g2.revoked_at IS NULL AND g2.id<>g.id)",
            target = group_target_predicate("platform"),
        ),
        GroupGrantCategory::WorkspaceGained => format!(
            "SELECT u.id AS user_id,m.workspace_role AS role,m.workspace_id AS workspace_id
             FROM unnest($1::uuid[]) u(id) CROSS JOIN m
             JOIN users usr ON usr.id=u.id AND usr.disabled_at IS NULL AND usr.cleaned_at IS NULL
             JOIN workspaces w ON w.id=m.workspace_id AND w.disabled_at IS NULL AND w.kind IN ('team','project')
             WHERE {enabled} AND NOT EXISTS(SELECT 1 FROM workspace_membership_grants g WHERE g.user_id=u.id AND g.workspace_id=m.workspace_id AND g.role=m.workspace_role AND g.revoked_at IS NULL)",
            enabled = group_enabled_for("workspace"),
        ),
        GroupGrantCategory::WorkspaceLost => format!(
            "SELECT g.user_id AS user_id,g.role AS role,g.workspace_id AS workspace_id
             FROM workspace_membership_grants g CROSS JOIN m
             JOIN users usr ON usr.id=g.user_id AND usr.disabled_at IS NULL AND usr.cleaned_at IS NULL
             WHERE g.source='group' AND g.revoked_at IS NULL AND {gm} AND NOT ({target})
             AND NOT EXISTS(SELECT 1 FROM workspace_membership_grants g2 WHERE g2.user_id=g.user_id AND g2.workspace_id=g.workspace_id AND g2.revoked_at IS NULL AND g2.id<>g.id)",
            target = group_target_predicate("workspace"),
        ),
    };
    let sql = format!(
        "WITH m(id,target_kind,platform_role,workspace_id,workspace_role,enabled) AS (
            VALUES ($3::uuid,$4::text,$5::text,$6::uuid,$7::text,$8::boolean)
         ), category AS ({select})
         SELECT user_id,role,workspace_id,count(*) OVER() AS total FROM category ORDER BY user_id LIMIT $2"
    );
    #[derive(sqlx::FromRow)]
    struct Row {
        user_id: Uuid,
        role: String,
        workspace_id: Option<Uuid>,
        total: i64,
    }
    let rows: Vec<Row> = sqlx::query_as(&sql)
        .bind(roster)
        .bind(limit)
        .bind(proposal.existing_mapping_id)
        .bind(proposal.target_kind)
        .bind(proposal.platform_role)
        .bind(proposal.workspace_id)
        .bind(proposal.workspace_role)
        .bind(proposal.enabled)
        .fetch_all(&mut **tx)
        .await?;
    let total = rows.first().map_or(0, |r| r.total);
    Ok(GroupGrantDiffPage {
        rows: rows
            .into_iter()
            .map(|r| GroupGrantDiffRow {
                user_id: r.user_id,
                role: r.role,
                workspace_id: r.workspace_id,
            })
            .collect(),
        total,
    })
}

async fn entitled(tx: &mut Transaction<'_, Postgres>, user: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM platform_role_grants WHERE user_id=$1 AND revoked_at IS NULL)",
    )
    .bind(user)
    .fetch_one(&mut **tx)
    .await
}

async fn disable_for_entitlement_loss(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET disabled_at=coalesce(disabled_at,now()),cleanup_due_at=coalesce(cleanup_due_at,now()+interval '30 days'),disable_reason=coalesce(disable_reason,'entitlement_loss') WHERE id=$1 AND cleaned_at IS NULL")
        .bind(user).execute(&mut **tx).await?;
    sqlx::query(
        "UPDATE browser_sessions SET revoked_at=now() WHERE user_id=$1 AND revoked_at IS NULL",
    )
    .bind(user)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE api_keys SET revoked_at=now() WHERE issued_to_user_id=$1 AND revoked_at IS NULL",
    )
    .bind(user)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// SCIM group membership changed for `user`. Only SCIM-managed group values (`managed`)
/// are reconciled; token-derived grants for other groups wait for the next sign-in.
/// Losing the last platform grant is entitlement loss (sessions and keys revoked). A user
/// who never had a grant (provisioned, no mapped group) is left as is. Regaining a grant
/// within the grace period lifts entitlement-loss suspension only; revoked keys stay revoked.
pub(crate) async fn synchronize_scim_groups(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
    issuer: &str,
    groups: &[String],
    managed: &[String],
) -> Result<(), sqlx::Error> {
    let had = entitled(tx, user).await?;
    apply_group_grants(tx, user, issuer, groups, Some(managed)).await?;
    let now_entitled = entitled(tx, user).await?;
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,action,resource_type,resource_id,metadata) VALUES($1,NULL,'identity.groups_synchronized','user',$2,jsonb_build_object('source','scim','entitled',$3::boolean))")
        .bind(Uuid::new_v4()).bind(user).bind(now_entitled).execute(&mut **tx).await?;
    if had && !now_entitled {
        disable_for_entitlement_loss(tx, user).await?;
    } else if now_entitled {
        sqlx::query("UPDATE users SET disabled_at=NULL,cleanup_due_at=NULL,disable_reason=NULL WHERE id=$1 AND cleaned_at IS NULL AND disable_reason='entitlement_loss' AND cleanup_due_at>now()")
            .bind(user).execute(&mut **tx).await?;
    }
    Ok(())
}

/// Only call with a well-formed, signature-verified group claim. Empty groups are authoritative.
/// Manual grants survive group synchronization. Missing/malformed claims never call this function.
pub(crate) async fn synchronize_groups(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
    issuer: &str,
    groups: &[String],
) -> Result<bool, sqlx::Error> {
    apply_group_grants(tx, user, issuer, groups, None).await?;
    let entitled = entitled(tx, user).await?;
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,action,resource_type,resource_id,metadata) VALUES($1,$2,'identity.groups_synchronized','user',$2,jsonb_build_object('source','group','entitled',$3::boolean))")
        .bind(Uuid::new_v4()).bind(user).bind(entitled).execute(&mut **tx).await?;
    if !entitled {
        disable_for_entitlement_loss(tx, user).await?;
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
