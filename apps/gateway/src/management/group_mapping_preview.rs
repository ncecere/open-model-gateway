//! Read-only dry-run diff for a proposed OIDC group mapping (#89). Mirrors
//! the `deployment_configuration` preview pattern (#85/PR #336): a
//! read-only snapshot, a pure evaluator, bounded output. Nothing here
//! writes `oidc_group_mappings`, grants or `audit_events`, and no config
//! version is bumped. Validation reuses `directory::{MappingInput,
//! validate_mapping}` exactly, so an invalid proposal is rejected the same
//! way create/update would reject it.
//!
//! Two separate things are reported (review round 2, #89):
//! * `immediate`: what `directory::revoke_mapping_grants` does
//!   unconditionally and synchronously the instant a PATCH changes any
//!   field or a DELETE commits -- every grant the *existing* mapping
//!   currently produces, revoked, whatever the new definition (if any)
//!   would recreate, plus its consequences (users suspended for losing
//!   their only platform entitlement, keys permanently revoked for losing
//!   their only workspace membership). Even a same-role rename shows up
//!   here, because the real write path revokes first and only a later
//!   sign-in/SCIM reconciliation restores anything.
//! * `diff`: the net effective-role change (precedence across every
//!   active grant, any source) once that reconciliation has caught up --
//!   gained (a new entitlement or a role upgrade) or lost (a downgrade or
//!   total loss, for example a manual `member` grant left behind after a
//!   group `admin` mapping is deleted).
use super::directory::{
    MAPPING_PATCH_FIELDS, MappingInput, mapping_digest, merge_mapping_patch, validate_mapping,
};
use super::settings::SignIn;
use super::*;
use crate::lifecycle;
use std::collections::HashMap;

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route(
            "/api/v1/platform/oidc/group-mappings/preview",
            post(preview_new_mapping),
        )
        .route(
            "/api/v1/platform/oidc/group-mappings/{id}/preview",
            post(preview_existing_mapping),
        )
}

/// Up to 200 rows per category: the bound every list in the response respects.
const LIMIT: usize = 200;
/// How many users a group may have before a roster lookup marks the whole
/// preview `complete:false` instead of silently undercounting (review #89
/// item 5). Lowered under `cfg(test)` so the overflow path is exercised
/// without seeding 5,000+ rows.
#[cfg(not(test))]
const ROSTER_LIMIT: i64 = 5000;
#[cfg(test)]
const ROSTER_LIMIT: i64 = 3;

/// The new-mapping preview's own body: unlike the existing-mapping preview
/// (which must mirror `PATCH`'s partial-merge semantics, review #89 round
/// 5), there is no stored row to merge against, so this is always a full
/// replacement -- every field required, exactly like `create_mapping`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PreviewInput {
    issuer: Option<String>,
    group_value: Option<String>,
    target_kind: Option<String>,
    platform_role: Option<String>,
    workspace_id: Option<Uuid>,
    workspace_role: Option<String>,
    enabled: Option<bool>,
}
impl PreviewInput {
    /// The same shape create/update validate. A missing required field is
    /// the same `invalid()` a malformed create/update body would get,
    /// never a distinct, more permissive error.
    fn into_mapping(self) -> Result<MappingInput, ApiError> {
        Ok(MappingInput {
            issuer: self.issuer.ok_or_else(invalid)?,
            group_value: self.group_value.ok_or_else(invalid)?,
            target_kind: self.target_kind.ok_or_else(invalid)?,
            platform_role: self.platform_role,
            workspace_id: self.workspace_id,
            workspace_role: self.workspace_role,
            enabled: self.enabled.ok_or_else(invalid)?,
        })
    }
}
fn target_kind_of(target_kind: &str) -> lifecycle::GroupTargetKind {
    if target_kind == "platform" {
        lifecycle::GroupTargetKind::Platform
    } else {
        lifecycle::GroupTargetKind::Workspace
    }
}
/// [`ExistingMapping`] as the same jsonb shape [`MappingInput`]
/// deserializes, for [`merge_mapping_patch`] -- the exact projection
/// `update_mapping`'s own `current` query builds, just assembled from an
/// already-fetched row instead of a second `SELECT`.
fn existing_as_value(existing: &ExistingMapping) -> Value {
    json!({
        "issuer": existing.issuer,
        "group_value": existing.group_value,
        "target_kind": existing.target_kind,
        "platform_role": existing.platform_role,
        "workspace_id": existing.workspace_id,
        "workspace_role": existing.workspace_role,
        "enabled": existing.enabled,
    })
}

async fn require_platform_admin(
    tx: &mut Transaction<'_, Postgres>,
    u: &impl Actor,
) -> Result<(), ApiError> {
    if resources::platform_role_snapshot(tx, u.user_id()).await? != "admin" {
        return Err(denied());
    }
    Ok(())
}
/// SCIM is bound to exactly one issuer at startup (`identity::ScimRuntime`,
/// `crate::scim`'s module docs). Review #89 item 2: a mapping's SCIM
/// roster must never be trusted unless its own `issuer` equals this one.
/// `Unknown` only happens when no sign-in state was wired into the request
/// at all; the real router always layers `Extension<SignIn>` (every
/// `management::router` request carries it), so this is strictly a test-
/// harness case, kept issuer-agnostic (the pre-existing behavior) rather
/// than gated, since there is nothing to compare against.
enum ScimIssuer {
    Unknown,
    Configured(Option<String>),
}
fn configured_scim_issuer(config: &Option<Extension<SignIn>>) -> ScimIssuer {
    match config {
        None => ScimIssuer::Unknown,
        Some(Extension(c)) => ScimIssuer::Configured(c.0.scim().map(|rt| rt.issuer.clone())),
    }
}

async fn preview_new_mapping(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    config: Option<Extension<SignIn>>,
    Json(input): Json<PreviewInput>,
) -> DetailedResult {
    // Browser session only (route_policy::TokenPolicy::Denied already
    // refuses every admin token before dispatch); this repeats the check
    // the way deployment_configuration.rs's preview does, in case a future
    // route ever reused this handler under a different policy.
    if u.token_id().is_some() {
        return Err(denied().into());
    }
    let b = input.into_mapping()?;
    let mut tx = resources::read_snapshot(&s).await?;
    require_platform_admin(&mut tx, &u).await?;
    validate_mapping(&mut tx, &b).await?;
    let proposal = lifecycle::GroupGrantProposal {
        existing_mapping_id: None,
        target_kind: target_kind_of(&b.target_kind),
        platform_role: b.platform_role.as_deref(),
        workspace_id: b.workspace_id,
        workspace_role: b.workspace_role.as_deref(),
        enabled: b.enabled,
    };
    let scim_issuer = configured_scim_issuer(&config);
    let result = evaluate(&mut tx, &b.issuer, &b.group_value, &proposal, &scim_issuer).await?;
    tx.commit().await?;
    Ok(Json(result))
}

#[derive(sqlx::FromRow)]
struct ExistingMapping {
    issuer: String,
    group_value: String,
    target_kind: String,
    platform_role: Option<String>,
    workspace_id: Option<Uuid>,
    workspace_role: Option<String>,
    enabled: bool,
}

/// `delete:true`, or no mapping field present at all (an empty `{}` body,
/// standing in for an omitted body): the delete-preview request, exactly
/// `PreviewInput`'s old `is_delete_request`. Any other top-level key is
/// refused up front (`422`), the same way [`merge_mapping_patch`] refuses
/// one for the merge branch -- one allowed-field check either branch ends
/// up taking, not two that could drift.
fn classify_existing_mapping_body(
    body: &Value,
) -> Result<(bool, serde_json::Map<String, Value>), ApiError> {
    let object = body.as_object().ok_or_else(invalid)?;
    let mut delete_flag = false;
    let mut patch = serde_json::Map::new();
    for (k, v) in object {
        if k == "delete" {
            delete_flag = v.as_bool().ok_or_else(invalid)?;
            continue;
        }
        if !MAPPING_PATCH_FIELDS.contains(&k.as_str()) {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Unknown group mapping field",
            ));
        }
        patch.insert(k.clone(), v.clone());
    }
    let is_delete = delete_flag || patch.is_empty();
    Ok((is_delete, patch))
}

async fn preview_existing_mapping(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(id): Path<Uuid>,
    config: Option<Extension<SignIn>>,
    Json(body): Json<Value>,
) -> DetailedResult {
    if u.token_id().is_some() {
        return Err(denied().into());
    }
    let (is_delete, patch) = classify_existing_mapping_body(&body)?;
    let mut tx = resources::read_snapshot(&s).await?;
    require_platform_admin(&mut tx, &u).await?;
    let existing: ExistingMapping = sqlx::query_as(
        "SELECT issuer,group_value,target_kind,platform_role,workspace_id,workspace_role,enabled FROM oidc_group_mappings WHERE id=$1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(missing)?;
    let digest = mapping_digest(
        &existing.issuer,
        &existing.group_value,
        &existing.target_kind,
        existing.platform_role.as_deref(),
        existing.workspace_id,
        existing.workspace_role.as_deref(),
        existing.enabled,
    );
    // Owns the merged mapping so the borrowed proposal outlives the branch.
    let proposed;
    let (issuer, group_value, proposal, fields_changed) = if is_delete {
        let proposal = lifecycle::GroupGrantProposal {
            existing_mapping_id: Some(id),
            target_kind: target_kind_of(&existing.target_kind),
            platform_role: existing.platform_role.as_deref(),
            workspace_id: existing.workspace_id,
            workspace_role: existing.workspace_role.as_deref(),
            // A mapping about to be deleted never still produces a grant,
            // whatever its stored `enabled` value is.
            enabled: false,
        };
        (
            existing.issuer.clone(),
            existing.group_value.clone(),
            proposal,
            true,
        )
    } else {
        // `PATCH`'s own merge semantics (review #89 round 5), not a full
        // replacement: a field this body omits keeps its stored value.
        let current = existing_as_value(&existing);
        let (merged, b) =
            merge_mapping_patch(&mut tx, current.clone(), &Value::Object(patch)).await?;
        proposed = b;
        let b = &proposed;
        let changed = merged != current;
        let proposal = lifecycle::GroupGrantProposal {
            existing_mapping_id: Some(id),
            target_kind: target_kind_of(&b.target_kind),
            platform_role: b.platform_role.as_deref(),
            workspace_id: b.workspace_id,
            workspace_role: b.workspace_role.as_deref(),
            enabled: b.enabled,
        };
        (b.issuer.clone(), b.group_value.clone(), proposal, changed)
    };
    let scim_issuer = configured_scim_issuer(&config);
    let mut result = evaluate(&mut tx, &issuer, &group_value, &proposal, &scim_issuer).await?;
    if fields_changed {
        let immediate = render_immediate(&mut tx, id).await?;
        result["immediate"] = immediate;
    }
    result["digest"] = json!(digest);
    tx.commit().await?;
    Ok(Json(result))
}

/// Who is "in" `group_value` right now, and how we know it: a SCIM roster
/// when one exists for this value (authoritative, even if currently empty),
/// otherwise the bounded set of users who already hold a `source='group'`
/// grant from this issuer/group (sign-in derived, so it only reflects users
/// who have already signed in with the group, not current IdP truth).
/// Matching is exact (review #89 item 4): the real reconciliation
/// (`lifecycle::apply_group_grants`) compares `group_value` by equality,
/// never case-folded, and SCIM's own membership resolution
/// (`scim::managed_values`/`member_values`) matches `scim_groups`'
/// `display_name`/`external_id` the same exact way. `scim_issuer` (review
/// #89 item 2) is the one issuer SCIM is actually bound to; a mapping
/// proposed under any other issuer can never receive SCIM-sourced grants in
/// reality, so its SCIM roster is never trusted even when `group_value`
/// happens to match a real `scim_groups` row.
async fn roster(
    tx: &mut Transaction<'_, Postgres>,
    issuer: &str,
    group_value: &str,
    scim_issuer: &ScimIssuer,
) -> Result<(Vec<Uuid>, &'static str, bool), ApiError> {
    let issuer_trusted = match scim_issuer {
        ScimIssuer::Unknown => true,
        ScimIssuer::Configured(Some(configured)) => configured == issuer,
        ScimIssuer::Configured(None) => false,
    };
    let managed: bool = issuer_trusted
        && sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM scim_groups WHERE display_name=$1 OR external_id=$1)",
        )
        .bind(group_value)
        .fetch_one(&mut **tx)
        .await?;
    if managed {
        let members: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT m.user_id FROM scim_group_members m JOIN scim_groups g ON g.id=m.group_id WHERE g.display_name=$1 OR g.external_id=$1 ORDER BY 1 LIMIT $2",
        )
        .bind(group_value)
        .bind(ROSTER_LIMIT + 1)
        .fetch_all(&mut **tx)
        .await?;
        let complete = members.len() as i64 <= ROSTER_LIMIT;
        let members = members.into_iter().take(ROSTER_LIMIT as usize).collect();
        return Ok((members, "scim", complete));
    }
    let derived: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT user_id FROM (
            SELECT g.user_id FROM platform_role_grants g JOIN oidc_group_mappings m ON m.id=g.mapping_id WHERE g.source='group' AND g.revoked_at IS NULL AND m.issuer=$1 AND m.group_value=$2
            UNION
            SELECT g.user_id FROM workspace_membership_grants g JOIN oidc_group_mappings m ON m.id=g.mapping_id WHERE g.source='group' AND g.revoked_at IS NULL AND m.issuer=$1 AND m.group_value=$2
         ) holders ORDER BY user_id LIMIT $3",
    )
    .bind(issuer)
    .bind(group_value)
    .bind(ROSTER_LIMIT + 1)
    .fetch_all(&mut **tx)
    .await?;
    let complete = derived.len() as i64 <= ROSTER_LIMIT;
    let derived = derived.into_iter().take(ROSTER_LIMIT as usize).collect();
    Ok((derived, "last_sign_in_derived", complete))
}

/// Platform-visible identity (email, display name) for a bounded set of
/// user ids, in one batched lookup. Never reads anything else about a
/// user -- only platform roles and shared-workspace grants ever name
/// someone here, never personal workspace data.
async fn names(
    tx: &mut Transaction<'_, Postgres>,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, (Option<String>, Option<String>)>, ApiError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let people: Vec<(Uuid, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT id,email,display_name FROM users WHERE id=ANY($1)")
            .bind(ids)
            .fetch_all(&mut **tx)
            .await?;
    Ok(people
        .into_iter()
        .map(|(id, email, display_name)| (id, (email, display_name)))
        .collect())
}

fn truncate<T>(mut rows: Vec<T>) -> (Vec<T>, usize, bool) {
    let total = rows.len();
    let truncated = total > LIMIT;
    rows.truncate(LIMIT);
    (rows, total, truncated)
}

async fn render_platform(
    tx: &mut Transaction<'_, Postgres>,
    rows: Vec<lifecycle::PlatformEffectRow>,
) -> Result<(Value, usize, bool), ApiError> {
    let (rows, total, truncated) = truncate(rows);
    let who = names(tx, &rows.iter().map(|r| r.user_id).collect::<Vec<_>>()).await?;
    let data = rows
        .into_iter()
        .map(|r| {
            let (email, display_name) = who.get(&r.user_id).cloned().unwrap_or((None, None));
            json!({
                "user_id": r.user_id, "email": email, "display_name": display_name,
                "before": r.before, "after": r.after, "workspace_id": Value::Null,
                "reactivated": r.reactivated, "would_be_suspended": r.would_be_suspended,
            })
        })
        .collect();
    Ok((Value::Array(data), total, truncated))
}
async fn render_workspace(
    tx: &mut Transaction<'_, Postgres>,
    rows: Vec<lifecycle::WorkspaceEffectRow>,
) -> Result<(Value, usize, bool), ApiError> {
    let (rows, total, truncated) = truncate(rows);
    let who = names(tx, &rows.iter().map(|r| r.user_id).collect::<Vec<_>>()).await?;
    let data = rows
        .into_iter()
        .map(|r| {
            let (email, display_name) = who.get(&r.user_id).cloned().unwrap_or((None, None));
            json!({
                "user_id": r.user_id, "email": email, "display_name": display_name,
                "before": r.before, "after": r.after, "workspace_id": r.workspace_id,
            })
        })
        .collect();
    Ok((Value::Array(data), total, truncated))
}

/// `immediate` section (review #89 item 1): every grant `mapping_id`
/// currently produces, unconditionally, plus the consequent suspensions
/// and permanent key revocations the real write would cause the instant it
/// commits. Only called when the write would actually revoke something
/// (a DELETE, or a PATCH that changes any field).
async fn render_immediate(
    tx: &mut Transaction<'_, Postgres>,
    mapping_id: Uuid,
) -> Result<Value, ApiError> {
    let active = lifecycle::mapping_active_grants(tx, mapping_id).await?;
    let consequences = lifecycle::immediate_consequences(tx, mapping_id).await?;
    let platform_ids: Vec<Uuid> = active.platform.iter().map(|(u, _)| *u).collect();
    let platform_who = names(tx, &platform_ids).await?;
    let platform_role_revoked: Vec<Value> = active
        .platform
        .iter()
        .map(|(user_id, role)| {
            let (email, display_name) = platform_who.get(user_id).cloned().unwrap_or((None, None));
            json!({"user_id": user_id, "email": email, "display_name": display_name, "role": role})
        })
        .collect();
    let workspace_ids: Vec<Uuid> = active.workspace.iter().map(|(u, _, _)| *u).collect();
    let workspace_who = names(tx, &workspace_ids).await?;
    let workspace_membership_revoked: Vec<Value> = active
        .workspace
        .iter()
        .map(|(user_id, workspace_id, role)| {
            let (email, display_name) = workspace_who.get(user_id).cloned().unwrap_or((None, None));
            json!({"user_id": user_id, "email": email, "display_name": display_name, "role": role, "workspace_id": workspace_id})
        })
        .collect();
    let suspended_who = names(tx, &consequences.users_suspended).await?;
    let users_suspended: Vec<Value> = consequences
        .users_suspended
        .iter()
        .map(|user_id| {
            let (email, display_name) = suspended_who.get(user_id).cloned().unwrap_or((None, None));
            json!({"user_id": user_id, "email": email, "display_name": display_name})
        })
        .collect();
    Ok(json!({
        "applies": true,
        "blocked_by_last_admin": consequences.strands_last_admin,
        "platform_role_revoked": platform_role_revoked,
        "workspace_membership_revoked": workspace_membership_revoked,
        "users_suspended": users_suspended,
        "keys_revoked_count": consequences.keys_revoked_count,
    }))
}

/// The evaluator: one roster lookup plus the net before/after precedence
/// comparison for both platform and workspace targets, no writes.
/// `immediate` (review item 1) is added by the caller only when the real
/// write would actually revoke something; otherwise it defaults to
/// `applies:false`.
async fn evaluate(
    tx: &mut Transaction<'_, Postgres>,
    issuer: &str,
    group_value: &str,
    proposal: &lifecycle::GroupGrantProposal<'_>,
    scim_issuer: &ScimIssuer,
) -> Result<Value, ApiError> {
    let (members, basis, complete) = roster(tx, issuer, group_value, scim_issuer).await?;
    let (platform_gained_rows, platform_lost_rows) =
        lifecycle::preview_platform_effect(tx, proposal, &members).await?;
    let (workspace_gained_rows, workspace_lost_rows) =
        lifecycle::preview_workspace_effect(tx, proposal, &members).await?;
    let (platform_role_gained, platform_gained_total, platform_gained_truncated) =
        render_platform(tx, platform_gained_rows).await?;
    let (platform_role_lost, platform_lost_total, platform_lost_truncated) =
        render_platform(tx, platform_lost_rows).await?;
    let (workspace_membership_gained, workspace_gained_total, workspace_gained_truncated) =
        render_workspace(tx, workspace_gained_rows).await?;
    let (workspace_membership_lost, workspace_lost_total, workspace_lost_truncated) =
        render_workspace(tx, workspace_lost_rows).await?;
    let truncated = platform_gained_truncated
        || platform_lost_truncated
        || workspace_gained_truncated
        || workspace_lost_truncated;
    let gained = platform_gained_total + workspace_gained_total;
    let lost = platform_lost_total + workspace_lost_total;
    let mut warnings = vec![
        "Configuration estimate, not a reservation. Apply independently revalidates live mapping state, membership and authority; this preview does not authorize a commit.".to_owned(),
        "Only platform roles and shared (team/project) membership are shown; personal-workspace effects are never included.".to_owned(),
    ];
    if basis == "last_sign_in_derived" {
        warnings.push("No SCIM roster exists for this group value; membership reflects only users who have already signed in with it, not current identity-provider truth.".to_owned());
    }
    if !complete {
        warnings.push(format!(
            "This group's membership exceeds the {ROSTER_LIMIT}-user evaluation cap; this preview is incomplete and may undercount gains."
        ));
    }
    // oidc_group_mappings carries no config_versions topic (0028): mapping
    // writes widen or narrow grants, which already bump the 'access' topic
    // on their own UPDATE/DELETE, but the mapping row itself is never
    // versioned. There is nothing stable to name as `expected_revision`;
    // `digest` (added by the caller, present only for an existing mapping)
    // is the observation-only token for a stale-confirmation check instead.
    warnings.push("No mapping configuration version exists to name as expected_revision; it is omitted. Apply still independently rechecks current mapping state.".to_owned());
    Ok(json!({
        "membership_basis": basis,
        "complete": complete,
        "digest": Value::Null,
        "immediate": {
            "applies": false,
            "blocked_by_last_admin": false,
            "platform_role_revoked": [],
            "workspace_membership_revoked": [],
            "users_suspended": [],
            "keys_revoked_count": 0,
        },
        "diff": {
            "platform_role_gained": platform_role_gained,
            "platform_role_lost": platform_role_lost,
            "workspace_membership_gained": workspace_membership_gained,
            "workspace_membership_lost": workspace_membership_lost,
        },
        "counts": {"gained": gained, "lost": lost, "truncated": truncated},
        "warnings": warnings,
    }))
}
