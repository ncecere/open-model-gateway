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

/// A mapping's write target: `Platform` role or `Workspace` membership.
/// Shared by `apply_group_grants` and the preview evaluator
/// (`management::group_mapping_preview`, #89) instead of either side typing
/// `"platform"`/`"workspace"` literals that could drift or typo apart.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupTargetKind {
    Platform,
    Workspace,
}
impl GroupTargetKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Platform => "platform",
            Self::Workspace => "workspace",
        }
    }
}
/// Grant/mapping identity: grant `g` is currently sourced from mapping `m`
/// (`g.mapping_id=m.id`). Every provenance/revoke check below -- and the
/// preview evaluator -- joins on this exact text, so a grant's source
/// mapping is decided one way only.
pub(crate) fn group_match_predicate() -> &'static str {
    "m.id=g.mapping_id"
}
/// Whether mapping `m` is enabled and targets `kind`, independent of any
/// existing grant: the condition for creating a fresh group-sourced grant.
/// `m` is either a real `oidc_group_mappings` row (the write path) or a
/// one-row proposal CTE of the same column names (the preview evaluator) --
/// the text never changes.
pub(crate) fn group_enabled_for(kind: GroupTargetKind) -> &'static str {
    match kind {
        GroupTargetKind::Platform => "m.enabled AND m.target_kind='platform'",
        GroupTargetKind::Workspace => "m.enabled AND m.target_kind='workspace'",
    }
}
/// Whether mapping `m`, if enabled, currently produces grant `g`'s own role
/// (same workspace too, for a workspace target). `apply_group_grants`'s
/// revoke step and the preview evaluator's "would this grant survive" check
/// share this exact text -- one source of truth for "what does this mapping
/// do," never a parallel reimplementation that could drift.
pub(crate) fn group_target_predicate(kind: GroupTargetKind) -> String {
    match kind {
        GroupTargetKind::Platform => {
            format!("{} AND m.platform_role=g.role", group_enabled_for(kind))
        }
        GroupTargetKind::Workspace => format!(
            "{} AND m.workspace_id=g.workspace_id AND m.workspace_role=g.role",
            group_enabled_for(kind)
        ),
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
        target = group_target_predicate(GroupTargetKind::Platform)))
        .bind(user).bind(issuer).bind(groups).bind(scope).execute(&mut **tx).await?;
    sqlx::query(&format!("INSERT INTO platform_role_grants(id,user_id,role,source,mapping_id) SELECT gen_random_uuid(),$1,m.platform_role,'group',m.id FROM oidc_group_mappings m WHERE m.issuer=$2 AND {enabled} AND m.group_value=ANY($3) AND ($4::text[] IS NULL OR m.group_value=ANY($4)) AND NOT EXISTS(SELECT 1 FROM platform_role_grants g WHERE g.user_id=$1 AND {gm} AND g.role=m.platform_role AND g.revoked_at IS NULL)",
        enabled = group_enabled_for(GroupTargetKind::Platform)))
        .bind(user).bind(issuer).bind(groups).bind(scope).execute(&mut **tx).await?;
    sqlx::query(&format!("UPDATE workspace_membership_grants g SET revoked_at=now() WHERE g.user_id=$1 AND g.source='group' AND g.revoked_at IS NULL AND
        CASE WHEN $4::text[] IS NULL THEN (NOT EXISTS(SELECT 1 FROM oidc_group_mappings m WHERE {gm}) OR EXISTS(SELECT 1 FROM oidc_group_mappings m WHERE {gm} AND m.issuer=$2))
        ELSE EXISTS(SELECT 1 FROM oidc_group_mappings m WHERE {gm} AND m.issuer=$2 AND m.group_value=ANY($4)) END
        AND NOT EXISTS(SELECT 1 FROM oidc_group_mappings m WHERE {gm} AND m.issuer=$2 AND {target} AND m.group_value=ANY($3))",
        target = group_target_predicate(GroupTargetKind::Workspace)))
        .bind(user).bind(issuer).bind(groups).bind(scope).execute(&mut **tx).await?;
    sqlx::query(&format!("INSERT INTO workspace_membership_grants(id,workspace_id,user_id,role,source,mapping_id) SELECT gen_random_uuid(),m.workspace_id,$1,m.workspace_role,'group',m.id FROM oidc_group_mappings m JOIN workspaces w ON w.id=m.workspace_id AND w.disabled_at IS NULL WHERE m.issuer=$2 AND {enabled} AND m.group_value=ANY($3) AND ($4::text[] IS NULL OR m.group_value=ANY($4)) AND NOT EXISTS(SELECT 1 FROM workspace_membership_grants g WHERE g.user_id=$1 AND {gm} AND g.revoked_at IS NULL)",
        enabled = group_enabled_for(GroupTargetKind::Workspace)))
        .bind(user).bind(issuer).bind(groups).bind(scope).execute(&mut **tx).await?;
    // Workspace group loss must not make an old credential usable again when a group returns.
    sqlx::query("UPDATE api_keys k SET revoked_at=now() FROM workspaces w WHERE k.workspace_id=w.id AND w.kind IN ('team','project') AND k.issued_to_user_id=$1 AND k.revoked_at IS NULL AND NOT EXISTS(SELECT 1 FROM workspace_membership_grants g WHERE g.workspace_id=w.id AND g.user_id=$1 AND g.revoked_at IS NULL)")
        .bind(user).execute(&mut **tx).await?;
    Ok(())
}

/// A proposed mapping change (#89): `None` `existing_mapping_id` for a
/// brand-new mapping (no grant can reference it yet). `enabled` is `false`
/// for a delete preview: an about-to-be-removed mapping never produces a
/// grant, whatever its stored `enabled` value is.
pub(crate) struct GroupGrantProposal<'a> {
    pub(crate) existing_mapping_id: Option<Uuid>,
    pub(crate) target_kind: GroupTargetKind,
    pub(crate) platform_role: Option<&'a str>,
    pub(crate) workspace_id: Option<Uuid>,
    pub(crate) workspace_role: Option<&'a str>,
    pub(crate) enabled: bool,
}
/// Capped-roster false-loss fix (#89): how to decide whether an arbitrary
/// user -- not only one that happened to land inside the capped roster
/// array -- is *currently* a member of the mapping's group, independent of
/// `group_mapping_preview::ROSTER_LIMIT` truncation. Mirrors `roster()`'s
/// own two membership sources exactly (a SCIM roster when the group is
/// SCIM-managed, matched by `display_name`/`external_id`; otherwise the
/// sign-in-derived set of users already holding a `source='group'` grant
/// from this issuer/group value), but as a per-user `EXISTS` check instead
/// of array membership, so a capped-out existing grant holder is still
/// correctly recognized as a member. The roster array passed alongside
/// this to [`preview_platform_effect`]/[`preview_workspace_effect`] remains
/// capped and is used only to *discover* additional (gain) candidates;
/// losses must never be decided by whether a user landed inside it.
pub(crate) struct GroupMembershipBasis<'a> {
    pub(crate) is_scim: bool,
    pub(crate) issuer: &'a str,
    pub(crate) group_value: &'a str,
}
/// Every group-sourced grant an existing mapping currently produces,
/// unconditionally -- exactly the rows `directory::revoke_mapping_grants`
/// revokes the instant a PATCH changes any field or a DELETE commits,
/// regardless of whether the new definition (if any) would recreate them.
/// Review #89-2: a management write's own immediate effect is distinct from
/// what a later sign-in/SCIM reconciliation might restore.
pub(crate) struct MappingActiveGrants {
    pub(crate) platform: Vec<(Uuid, String)>,
    pub(crate) workspace: Vec<(Uuid, Uuid, String)>,
}
pub(crate) async fn mapping_active_grants(
    tx: &mut Transaction<'_, Postgres>,
    mapping_id: Uuid,
) -> Result<MappingActiveGrants, sqlx::Error> {
    let platform = sqlx::query_as("SELECT user_id,role FROM platform_role_grants WHERE mapping_id=$1 AND source='group' AND revoked_at IS NULL ORDER BY user_id")
        .bind(mapping_id).fetch_all(&mut **tx).await?;
    let workspace = sqlx::query_as("SELECT user_id,workspace_id,role FROM workspace_membership_grants WHERE mapping_id=$1 AND source='group' AND revoked_at IS NULL ORDER BY user_id")
        .bind(mapping_id).fetch_all(&mut **tx).await?;
    Ok(MappingActiveGrants {
        platform,
        workspace,
    })
}
/// Consequences of unconditionally revoking `mapping_id`'s grants right
/// now, mirroring `directory::revoke_mapping_grants`/`deactivate_unentitled`
/// exactly but as a read: whether the write would be refused outright
/// (stranding the last platform admin), which currently-active users would
/// lose their only platform entitlement and be suspended, and how many
/// distinct currently-active keys would be permanently revoked -- review
/// #89-3: `disable_for_entitlement_loss` revokes *every* key a newly
/// unentitled user holds (any workspace), not only the one lost workspace
/// membership's keys, so a user counted in `users_suspended` has all of
/// their keys counted here too, counted once even if they also lose a
/// specific workspace's membership independently. Never touches `api_keys`
/// or `users`: a prediction, not a write.
pub(crate) struct ImmediateConsequences {
    pub(crate) strands_last_admin: bool,
    pub(crate) users_suspended: Vec<Uuid>,
    pub(crate) keys_revoked_count: i64,
}
pub(crate) async fn immediate_consequences(
    tx: &mut Transaction<'_, Postgres>,
    mapping_id: Uuid,
) -> Result<ImmediateConsequences, sqlx::Error> {
    let strands_last_admin: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM effective_platform_roles WHERE role='admin') AND NOT EXISTS(SELECT 1 FROM platform_role_grants g JOIN users u ON u.id=g.user_id WHERE g.role='admin' AND g.revoked_at IS NULL AND u.disabled_at IS NULL AND u.cleaned_at IS NULL AND g.mapping_id IS DISTINCT FROM $1)")
        .bind(mapping_id).fetch_one(&mut **tx).await?;
    let users_suspended: Vec<Uuid> = sqlx::query_scalar("SELECT DISTINCT g.user_id FROM platform_role_grants g JOIN users u ON u.id=g.user_id WHERE g.mapping_id=$1 AND g.source='group' AND g.revoked_at IS NULL AND u.disabled_at IS NULL AND u.cleaned_at IS NULL AND NOT EXISTS(SELECT 1 FROM platform_role_grants g2 WHERE g2.user_id=g.user_id AND g2.revoked_at IS NULL AND g2.mapping_id IS DISTINCT FROM $1) ORDER BY g.user_id")
        .bind(mapping_id).fetch_all(&mut **tx).await?;
    let keys_revoked_count: i64 = sqlx::query_scalar("SELECT count(DISTINCT k.id) FROM api_keys k WHERE k.revoked_at IS NULL AND (
        k.issued_to_user_id=ANY($2::uuid[])
        OR EXISTS(SELECT 1 FROM workspace_membership_grants g WHERE g.mapping_id=$1 AND g.source='group' AND g.revoked_at IS NULL AND g.user_id=k.issued_to_user_id AND g.workspace_id=k.workspace_id
          AND NOT EXISTS(SELECT 1 FROM workspace_membership_grants g2 WHERE g2.workspace_id=g.workspace_id AND g2.user_id=g.user_id AND g2.revoked_at IS NULL AND g2.mapping_id IS DISTINCT FROM $1))
      )")
        .bind(mapping_id).bind(&users_suspended).fetch_one(&mut **tx).await?;
    Ok(ImmediateConsequences {
        strands_last_admin,
        users_suspended,
        keys_revoked_count,
    })
}
/// Rank-compared platform-role effect for one candidate user: `before`/
/// `after` are effective role names (`None` for no entitlement), already
/// classified `gained` (`after` outranks `before`) or `lost` (a downgrade or
/// total loss, review #89-2's "manual member + group admin" example).
/// `reactivated` is set when a user currently suspended for entitlement
/// loss, still inside its grace window, would regain entitlement (review
/// #89-3); `would_be_suspended` when a currently active user would lose
/// every platform grant.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PlatformEffectRow {
    pub(crate) user_id: Uuid,
    pub(crate) before: Option<String>,
    pub(crate) after: Option<String>,
    pub(crate) reactivated: bool,
    pub(crate) would_be_suspended: bool,
}
fn platform_role_name(rank: i32) -> Option<String> {
    match rank {
        3 => Some("admin".to_owned()),
        2 => Some("auditor".to_owned()),
        1 => Some("user".to_owned()),
        _ => None,
    }
}
/// Net before/after effective platform role (review #89-2's precedence: the
/// highest rank across every active grant, any source) for every roster
/// member plus every current holder of `proposal.existing_mapping_id`'s own
/// platform grants, after reconciliation would apply `proposal` -- i.e. with
/// this mapping's own current contribution excluded from "after" and its
/// new contribution (if enabled, platform-targeted and the user is still a
/// current group member per `membership`, checked independently of the
/// capped `roster` array so a capped-out existing holder is never shown
/// losing a mapping that still produces their grant) added back. A
/// disabled (not cleaned) user's `before` is `None`
/// (no current entitlement) even if their grant rows are not yet revoked,
/// matching `effective_platform_roles`. `after` only counts those rows for a
/// currently *active* user, or a currently disabled one exactly when the
/// real reactivation condition in `synchronize_groups`/
/// `synchronize_scim_groups` would fire (`disable_reason='entitlement_loss'
/// AND cleanup_due_at>now()`, review #89-4): an administratively suspended
/// user, or one whose grace window already expired, stays at `after=None`
/// regardless of any grant row, because nothing in the real write path
/// would lift their suspension. Cleaned users are always excluded. Never
/// bounded here -- the caller (roster is already capped) truncates and
/// counts.
pub(crate) async fn preview_platform_effect(
    tx: &mut Transaction<'_, Postgres>,
    proposal: &GroupGrantProposal<'_>,
    roster: &[Uuid],
    membership: &GroupMembershipBasis<'_>,
) -> Result<(Vec<PlatformEffectRow>, Vec<PlatformEffectRow>), sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Row {
        user_id: Uuid,
        before_rank: i32,
        after_rank: i32,
        reactivation_eligible: bool,
        currently_disabled: bool,
    }
    let sql = "WITH m(id,target_kind,platform_role,enabled) AS (VALUES ($2::uuid,$3::text,$4::text,$5::boolean)),
         candidates AS (SELECT DISTINCT u FROM (SELECT unnest($1::uuid[]) u UNION SELECT g.user_id FROM platform_role_grants g WHERE g.mapping_id=$2 AND g.source='group' AND g.revoked_at IS NULL) x),
         is_member AS (
           SELECT c.u,
             CASE WHEN $6::boolean THEN EXISTS(SELECT 1 FROM scim_group_members sm JOIN scim_groups sg ON sg.id=sm.group_id WHERE (sg.display_name=$7 OR sg.external_id=$7) AND sm.user_id=c.u)
             ELSE EXISTS(SELECT 1 FROM platform_role_grants g JOIN oidc_group_mappings mm ON mm.id=g.mapping_id WHERE g.source='group' AND g.revoked_at IS NULL AND mm.issuer=$8 AND mm.group_value=$7 AND g.user_id=c.u)
               OR EXISTS(SELECT 1 FROM workspace_membership_grants g JOIN oidc_group_mappings mm ON mm.id=g.mapping_id WHERE g.source='group' AND g.revoked_at IS NULL AND mm.issuer=$8 AND mm.group_value=$7 AND g.user_id=c.u)
             END AS member
           FROM candidates c
         )
         SELECT c.u AS user_id,
           CASE WHEN usr.disabled_at IS NOT NULL THEN 0 ELSE coalesce((SELECT max(CASE g.role WHEN 'admin' THEN 3 WHEN 'auditor' THEN 2 ELSE 1 END) FROM platform_role_grants g WHERE g.user_id=c.u AND g.revoked_at IS NULL),0) END AS before_rank,
           CASE WHEN usr.disabled_at IS NOT NULL AND NOT coalesce(usr.disable_reason='entitlement_loss' AND usr.cleanup_due_at>now(),false) THEN 0 ELSE
             greatest(
               coalesce((SELECT max(CASE g.role WHEN 'admin' THEN 3 WHEN 'auditor' THEN 2 ELSE 1 END) FROM platform_role_grants g WHERE g.user_id=c.u AND g.revoked_at IS NULL AND ($2::uuid IS NULL OR g.mapping_id IS DISTINCT FROM $2)),0),
               CASE WHEN im.member AND m.enabled AND m.target_kind='platform' THEN CASE m.platform_role WHEN 'admin' THEN 3 WHEN 'auditor' THEN 2 ELSE 1 END ELSE 0 END
             )
           END AS after_rank,
           coalesce(usr.disable_reason='entitlement_loss' AND usr.cleanup_due_at>now(),false) AS reactivation_eligible,
           usr.disabled_at IS NOT NULL AS currently_disabled
         FROM candidates c CROSS JOIN m JOIN users usr ON usr.id=c.u JOIN is_member im ON im.u=c.u
         WHERE usr.cleaned_at IS NULL";
    let rows: Vec<Row> = sqlx::query_as(sql)
        .bind(roster)
        .bind(proposal.existing_mapping_id)
        .bind(proposal.target_kind.as_str())
        .bind(proposal.platform_role)
        .bind(proposal.enabled)
        .bind(membership.is_scim)
        .bind(membership.group_value)
        .bind(membership.issuer)
        .fetch_all(&mut **tx)
        .await?;
    let mut gained = Vec::new();
    let mut lost = Vec::new();
    for r in rows {
        if r.before_rank == r.after_rank {
            continue;
        }
        let row = PlatformEffectRow {
            user_id: r.user_id,
            before: platform_role_name(r.before_rank),
            after: platform_role_name(r.after_rank),
            reactivated: r.currently_disabled && r.reactivation_eligible && r.after_rank > 0,
            would_be_suspended: r.after_rank == 0 && r.before_rank > 0 && !r.currently_disabled,
        };
        if r.after_rank > r.before_rank {
            gained.push(row);
        } else {
            lost.push(row);
        }
    }
    Ok((gained, lost))
}
/// [`PlatformEffectRow`] for one (user, workspace) pair. Workspace
/// membership does not suspend a user by itself (platform entitlement
/// does); `before`/`after` are workspace-role names (`member`/`admin`/
/// `owner`), `None` for no membership.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceEffectRow {
    pub(crate) user_id: Uuid,
    pub(crate) workspace_id: Uuid,
    pub(crate) before: Option<String>,
    pub(crate) after: Option<String>,
}
fn workspace_role_name(rank: i32) -> Option<String> {
    match rank {
        3 => Some("owner".to_owned()),
        2 => Some("admin".to_owned()),
        1 => Some("member".to_owned()),
        _ => None,
    }
}
/// [`preview_platform_effect`]'s workspace counterpart. Candidates are every
/// roster member paired with `proposal.workspace_id` (only when the
/// proposal targets a workspace) plus every (user, workspace) pair any
/// current grant of `proposal.existing_mapping_id` already names -- so an
/// edit that moves a mapping to a different workspace shows the old
/// workspace's loss and the new workspace's gain, never just one. The
/// mapping's own new contribution (both the entitlement check below and
/// the workspace-role check in the final `SELECT`) is gated on
/// `membership`, an independent per-user `EXISTS` check, never on whether
/// the candidate happened to land inside the capped `roster` array (#89
/// overflow fix): the roster array only discovers additional candidates.
/// `effective_workspace_memberships` requires live platform entitlement
/// too (review #89-4), so `before` is forced to `None` for a user with no
/// *current* active platform grant or who is currently disabled (ineligible
/// for automatic reactivation). `after` instead reuses the exact
/// `after_rank > 0` predicate of [`preview_platform_effect`] (adversarial
/// finding, lifecycle.rs): the same `proposal` can itself change the
/// user's platform entitlement (scenario A -- turning a user's sole
/// platform mapping into a workspace mapping must not show a workspace
/// gain, since applying it suspends them first) or a disabled user's grace-
/// window reactivation can restore it (scenario B -- an entitlement-loss-
/// suspended user with a *surviving*, unrelated platform grant reactivates
/// during reconciliation and so still gains the workspace). Using the
/// user's raw current `disabled_at`/grant state for `after`, as before,
/// missed both. Candidates also include every *other*, unrelated
/// shared-workspace membership (manual or group) a platform-entitlement
/// candidate -- a roster member, or a current holder of
/// `proposal.existing_mapping_id`'s own platform grants, the exact same
/// set [`preview_platform_effect`] evaluates -- already holds (adversarial
/// finding, round 6): a platform mapping's deletion/edit can suspend or
/// reactivate a user outright, which cascades into *every* Team/Project
/// membership they hold, not only a workspace this mapping happens to
/// also name. A workspace-target proposal cannot itself change platform
/// entitlement, so these extra pairs are harmless no-ops (`before`
/// unaffected, filtered out below) whenever nothing about the user's
/// entitlement actually changes.
pub(crate) async fn preview_workspace_effect(
    tx: &mut Transaction<'_, Postgres>,
    proposal: &GroupGrantProposal<'_>,
    roster: &[Uuid],
    membership: &GroupMembershipBasis<'_>,
) -> Result<(Vec<WorkspaceEffectRow>, Vec<WorkspaceEffectRow>), sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Row {
        user_id: Uuid,
        workspace_id: Uuid,
        before_rank: i32,
        after_rank: i32,
    }
    let sql = "WITH m(id,target_kind,workspace_id,workspace_role,enabled,platform_role) AS (VALUES ($2::uuid,$3::text,$4::uuid,$5::text,$6::boolean,$7::text)),
         pairs AS (
           SELECT DISTINCT user_id,workspace_id FROM (
             SELECT unnest($1::uuid[]) user_id,$4::uuid workspace_id WHERE $3='workspace' AND $6 AND $4 IS NOT NULL
             UNION
             SELECT g.user_id,g.workspace_id FROM workspace_membership_grants g WHERE g.mapping_id=$2 AND g.source='group' AND g.revoked_at IS NULL
             UNION
             SELECT g.user_id,g.workspace_id FROM workspace_membership_grants g JOIN workspaces w ON w.id=g.workspace_id AND w.kind IN ('team','project') WHERE g.revoked_at IS NULL AND (g.user_id=ANY($1::uuid[]) OR EXISTS(SELECT 1 FROM platform_role_grants pg WHERE pg.mapping_id=$2 AND pg.source='group' AND pg.revoked_at IS NULL AND pg.user_id=g.user_id))
           ) x WHERE workspace_id IS NOT NULL
         ),
         is_member AS (
           SELECT DISTINCT p.user_id AS u,
             CASE WHEN $8::boolean THEN EXISTS(SELECT 1 FROM scim_group_members sm JOIN scim_groups sg ON sg.id=sm.group_id WHERE (sg.display_name=$9 OR sg.external_id=$9) AND sm.user_id=p.user_id)
             ELSE EXISTS(SELECT 1 FROM platform_role_grants g JOIN oidc_group_mappings mm ON mm.id=g.mapping_id WHERE g.source='group' AND g.revoked_at IS NULL AND mm.issuer=$10 AND mm.group_value=$9 AND g.user_id=p.user_id)
               OR EXISTS(SELECT 1 FROM workspace_membership_grants g JOIN oidc_group_mappings mm ON mm.id=g.mapping_id WHERE g.source='group' AND g.revoked_at IS NULL AND mm.issuer=$10 AND mm.group_value=$9 AND g.user_id=p.user_id)
             END AS member
           FROM pairs p
         ),
         after_entitlement AS (
           SELECT DISTINCT p.user_id,
             (CASE WHEN usr.disabled_at IS NOT NULL AND NOT coalesce(usr.disable_reason='entitlement_loss' AND usr.cleanup_due_at>now(),false) THEN 0 ELSE
               greatest(
                 coalesce((SELECT max(CASE g.role WHEN 'admin' THEN 3 WHEN 'auditor' THEN 2 ELSE 1 END) FROM platform_role_grants g WHERE g.user_id=p.user_id AND g.revoked_at IS NULL AND ($2::uuid IS NULL OR g.mapping_id IS DISTINCT FROM $2)),0),
                 CASE WHEN im.member AND m.enabled AND m.target_kind='platform' THEN CASE m.platform_role WHEN 'admin' THEN 3 WHEN 'auditor' THEN 2 ELSE 1 END ELSE 0 END
               )
             END) > 0 AS entitled
           FROM pairs p CROSS JOIN m JOIN users usr ON usr.id=p.user_id JOIN is_member im ON im.u=p.user_id
         )
         SELECT p.user_id,p.workspace_id,
           CASE WHEN NOT (usr.disabled_at IS NULL AND EXISTS(SELECT 1 FROM platform_role_grants pg WHERE pg.user_id=p.user_id AND pg.revoked_at IS NULL)) THEN 0 ELSE coalesce((SELECT max(CASE g.role WHEN 'owner' THEN 3 WHEN 'admin' THEN 2 ELSE 1 END) FROM workspace_membership_grants g WHERE g.user_id=p.user_id AND g.workspace_id=p.workspace_id AND g.revoked_at IS NULL),0) END AS before_rank,
           CASE WHEN NOT ae.entitled THEN 0 ELSE
             greatest(
               coalesce((SELECT max(CASE g.role WHEN 'owner' THEN 3 WHEN 'admin' THEN 2 ELSE 1 END) FROM workspace_membership_grants g WHERE g.user_id=p.user_id AND g.workspace_id=p.workspace_id AND g.revoked_at IS NULL AND ($2::uuid IS NULL OR g.mapping_id IS DISTINCT FROM $2)),0),
               CASE WHEN im.member AND p.workspace_id=$4 AND m.enabled AND m.target_kind='workspace' THEN CASE m.workspace_role WHEN 'admin' THEN 2 ELSE 1 END ELSE 0 END
             )
           END AS after_rank
         FROM pairs p CROSS JOIN m JOIN users usr ON usr.id=p.user_id JOIN workspaces w ON w.id=p.workspace_id JOIN after_entitlement ae ON ae.user_id=p.user_id JOIN is_member im ON im.u=p.user_id
         WHERE usr.cleaned_at IS NULL AND w.disabled_at IS NULL";
    let rows: Vec<Row> = sqlx::query_as(sql)
        .bind(roster)
        .bind(proposal.existing_mapping_id)
        .bind(proposal.target_kind.as_str())
        .bind(proposal.workspace_id)
        .bind(proposal.workspace_role)
        .bind(proposal.enabled)
        .bind(proposal.platform_role)
        .bind(membership.is_scim)
        .bind(membership.group_value)
        .bind(membership.issuer)
        .fetch_all(&mut **tx)
        .await?;
    let mut gained = Vec::new();
    let mut lost = Vec::new();
    for r in rows {
        if r.before_rank == r.after_rank {
            continue;
        }
        let row = WorkspaceEffectRow {
            user_id: r.user_id,
            workspace_id: r.workspace_id,
            before: workspace_role_name(r.before_rank),
            after: workspace_role_name(r.after_rank),
        };
        if r.after_rank > r.before_rank {
            gained.push(row);
        } else {
            lost.push(row);
        }
    }
    Ok((gained, lost))
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
