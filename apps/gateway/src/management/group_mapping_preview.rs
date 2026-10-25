//! Read-only dry-run diff for a proposed OIDC group mapping (#89). Mirrors
//! the `deployment_configuration` preview pattern (#85/PR #336): a
//! read-only snapshot, a pure evaluator, bounded output. Nothing here
//! writes `oidc_group_mappings`, grants or `audit_events`, and no config
//! version is bumped. Validation reuses `directory::{MappingInput,
//! validate_mapping}` exactly, so an invalid proposal is rejected the same
//! way create/update would reject it.
use super::directory::{MappingInput, validate_mapping};
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

/// Up to 200 rows per category (platform/workspace, gained/lost): the
/// bound every list in the response respects.
const LIMIT: i64 = 200;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PreviewInput {
    #[serde(default)]
    delete: bool,
    issuer: Option<String>,
    group_value: Option<String>,
    target_kind: Option<String>,
    platform_role: Option<String>,
    workspace_id: Option<Uuid>,
    workspace_role: Option<String>,
    enabled: Option<bool>,
}
impl PreviewInput {
    /// Every optional field absent (an empty `{}` body, standing in for an
    /// omitted body) or an explicit `delete`.
    fn is_delete_request(&self) -> bool {
        self.delete
            || (self.issuer.is_none()
                && self.group_value.is_none()
                && self.target_kind.is_none()
                && self.platform_role.is_none()
                && self.workspace_id.is_none()
                && self.workspace_role.is_none()
                && self.enabled.is_none())
    }
    /// The same shape create/update validate. Missing required fields (or
    /// `delete` itself) are the same `invalid()` a malformed create/update
    /// body would get, never a distinct, more permissive error.
    fn into_mapping(self) -> Result<MappingInput, ApiError> {
        if self.delete {
            return Err(invalid());
        }
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

async fn require_platform_admin(
    tx: &mut Transaction<'_, Postgres>,
    u: &impl Actor,
) -> Result<(), ApiError> {
    if resources::platform_role_snapshot(tx, u.user_id()).await? != "admin" {
        return Err(denied());
    }
    Ok(())
}

async fn preview_new_mapping(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
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
    let result = evaluate(
        &mut tx,
        &b.issuer,
        &b.group_value,
        None,
        &b.target_kind,
        b.platform_role.as_deref(),
        b.workspace_id,
        b.workspace_role.as_deref(),
        b.enabled,
    )
    .await?;
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
}

async fn preview_existing_mapping(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(id): Path<Uuid>,
    Json(input): Json<PreviewInput>,
) -> DetailedResult {
    if u.token_id().is_some() {
        return Err(denied().into());
    }
    let is_delete = input.is_delete_request();
    let mut tx = resources::read_snapshot(&s).await?;
    require_platform_admin(&mut tx, &u).await?;
    let existing: ExistingMapping = sqlx::query_as(
        "SELECT issuer,group_value,target_kind,platform_role,workspace_id,workspace_role FROM oidc_group_mappings WHERE id=$1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(missing)?;
    let result = if is_delete {
        evaluate(
            &mut tx,
            &existing.issuer,
            &existing.group_value,
            Some(id),
            &existing.target_kind,
            existing.platform_role.as_deref(),
            existing.workspace_id,
            existing.workspace_role.as_deref(),
            // A mapping about to be deleted never still produces a grant,
            // whatever its stored `enabled` value is.
            false,
        )
        .await?
    } else {
        let b = input.into_mapping()?;
        validate_mapping(&mut tx, &b).await?;
        evaluate(
            &mut tx,
            &b.issuer,
            &b.group_value,
            Some(id),
            &b.target_kind,
            b.platform_role.as_deref(),
            b.workspace_id,
            b.workspace_role.as_deref(),
            b.enabled,
        )
        .await?
    };
    tx.commit().await?;
    Ok(Json(result))
}

/// Who is "in" `group_value` right now, and how we know it: a SCIM roster
/// when one exists for this value (authoritative, even if currently empty),
/// otherwise the bounded set of users who already hold a `source='group'`
/// grant from this issuer/group (sign-in derived, so it only reflects users
/// who have already signed in with the group, not current IdP truth).
async fn roster(
    tx: &mut Transaction<'_, Postgres>,
    issuer: &str,
    group_value: &str,
) -> Result<(Vec<Uuid>, &'static str), ApiError> {
    let managed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM scim_groups WHERE lower(display_name)=lower($1) OR external_id=$1)",
    )
    .bind(group_value)
    .fetch_one(&mut **tx)
    .await?;
    if managed {
        let members: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT m.user_id FROM scim_group_members m JOIN scim_groups g ON g.id=m.group_id WHERE lower(g.display_name)=lower($1) OR g.external_id=$1 ORDER BY 1 LIMIT 5000",
        )
        .bind(group_value)
        .fetch_all(&mut **tx)
        .await?;
        return Ok((members, "scim"));
    }
    let derived: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT user_id FROM (
            SELECT g.user_id FROM platform_role_grants g JOIN oidc_group_mappings m ON m.id=g.mapping_id WHERE g.source='group' AND g.revoked_at IS NULL AND m.issuer=$1 AND m.group_value=$2
            UNION
            SELECT g.user_id FROM workspace_membership_grants g JOIN oidc_group_mappings m ON m.id=g.mapping_id WHERE g.source='group' AND g.revoked_at IS NULL AND m.issuer=$1 AND m.group_value=$2
         ) holders ORDER BY user_id LIMIT 5000",
    )
    .bind(issuer)
    .bind(group_value)
    .fetch_all(&mut **tx)
    .await?;
    Ok((derived, "last_sign_in_derived"))
}

/// Enrich bounded diff rows with platform-visible identity (email,
/// display name) in one batched lookup. Never reads anything else about a
/// user, and only the id, email and display name of users already named by
/// a platform-role or shared-workspace grant diff -- never personal
/// workspace data.
async fn render(
    tx: &mut Transaction<'_, Postgres>,
    rows: &[lifecycle::GroupGrantDiffRow],
) -> Result<Vec<Value>, ApiError> {
    if rows.is_empty() {
        return Ok(vec![]);
    }
    let ids: Vec<Uuid> = rows.iter().map(|r| r.user_id).collect();
    let people: Vec<(Uuid, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT id,email,display_name FROM users WHERE id=ANY($1)")
            .bind(&ids)
            .fetch_all(&mut **tx)
            .await?;
    let people: HashMap<Uuid, (Option<String>, Option<String>)> = people
        .into_iter()
        .map(|(id, email, display_name)| (id, (email, display_name)))
        .collect();
    Ok(rows
        .iter()
        .map(|r| {
            let (email, display_name) = people.get(&r.user_id).cloned().unwrap_or((None, None));
            json!({
                "user_id": r.user_id,
                "email": email,
                "display_name": display_name,
                "role": r.role,
                "workspace_id": r.workspace_id,
            })
        })
        .collect())
}

/// The pure-ish evaluator: one roster lookup plus four bounded category
/// queries, no writes. `existing_mapping_id` is `None` for a brand-new
/// mapping (no grant can reference it yet, so every "lost" category is
/// empty by construction) and `Some` for an edit or delete of a stored one.
#[allow(clippy::too_many_arguments)]
async fn evaluate(
    tx: &mut Transaction<'_, Postgres>,
    issuer: &str,
    group_value: &str,
    existing_mapping_id: Option<Uuid>,
    target_kind: &str,
    platform_role: Option<&str>,
    workspace_id: Option<Uuid>,
    workspace_role: Option<&str>,
    enabled: bool,
) -> Result<Value, ApiError> {
    let (members, basis) = roster(tx, issuer, group_value).await?;
    let proposal = lifecycle::GroupGrantProposal {
        existing_mapping_id,
        target_kind,
        platform_role,
        workspace_id,
        workspace_role,
        enabled,
    };
    use lifecycle::GroupGrantCategory::{
        PlatformGained, PlatformLost, WorkspaceGained, WorkspaceLost,
    };
    let platform_gained =
        lifecycle::preview_group_grants(tx, &proposal, &members, PlatformGained, LIMIT).await?;
    let platform_lost =
        lifecycle::preview_group_grants(tx, &proposal, &members, PlatformLost, LIMIT).await?;
    let workspace_gained =
        lifecycle::preview_group_grants(tx, &proposal, &members, WorkspaceGained, LIMIT).await?;
    let workspace_lost =
        lifecycle::preview_group_grants(tx, &proposal, &members, WorkspaceLost, LIMIT).await?;
    let truncated = platform_gained.total > LIMIT
        || platform_lost.total > LIMIT
        || workspace_gained.total > LIMIT
        || workspace_lost.total > LIMIT;
    let gained = platform_gained.total + workspace_gained.total;
    let lost = platform_lost.total + workspace_lost.total;
    let platform_role_gained = render(tx, &platform_gained.rows).await?;
    let platform_role_lost = render(tx, &platform_lost.rows).await?;
    let workspace_membership_gained = render(tx, &workspace_gained.rows).await?;
    let workspace_membership_lost = render(tx, &workspace_lost.rows).await?;
    let mut warnings = vec![
        "Configuration estimate, not a reservation. Apply independently revalidates live mapping state, membership and authority; this preview does not authorize a commit.".to_owned(),
        "Only platform roles and shared (team/project) membership are shown; personal-workspace effects are never included.".to_owned(),
    ];
    if basis == "last_sign_in_derived" {
        warnings.push("No SCIM roster exists for this group value; membership reflects only users who have already signed in with it, not current identity-provider truth.".to_owned());
    }
    // oidc_group_mappings carries no config_versions topic (0028): mapping
    // writes widen or narrow grants, which already bump the 'access' topic
    // on their own UPDATE/DELETE, but the mapping row itself is never
    // versioned. There is nothing stable to name as `expected_revision`.
    warnings.push("No mapping configuration version exists to name as expected_revision; it is omitted. Apply still independently rechecks current mapping state.".to_owned());
    Ok(json!({
        "membership_basis": basis,
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
