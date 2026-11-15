//! The steps of applying a published template version (#84 slice B): the
//! body and idempotency checks, the reviewed version, and the writes that
//! create the workspace, in the order `apply` runs them.
use super::*;

/// Body-only limits before any lock: at most 10 slots listed, each once,
/// at most 100 people, nobody twice (the canonical digest relies on it).
pub(super) fn check_apply_body(b: &ApplyInput) -> Result<(), ManagementError> {
    let people: usize = b.assignments.iter().map(|a| a.user_ids.len()).sum();
    let distinct_slots = b
        .assignments
        .iter()
        .map(|a| a.slot.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let distinct_people = b
        .assignments
        .iter()
        .flat_map(|a| a.user_ids.iter())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    if !valid_name(&b.name)
        || b.version < 1
        || b.assignments.is_empty()
        || b.assignments.len() > 10
        || distinct_slots != b.assignments.len()
        || people > 100
        || distinct_people != people
        || !(1..=128).contains(&b.idempotency_key.len())
        || !b.idempotency_key.bytes().all(|c| c.is_ascii_graphic())
    {
        return Err(invalid().into());
    }
    Ok(())
}
/// The idempotency-key digest and the canonical payload digest of an application.
pub(super) fn apply_digests(
    id: Uuid,
    b: &ApplyInput,
) -> Result<(Vec<u8>, Vec<u8>), ManagementError> {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(b.idempotency_key.as_bytes()).to_vec();
    // Canonical: slots by name, each slot's people as a set, so a retry
    // that lists the same assignments in another order is the same payload.
    let canonical: std::collections::BTreeMap<&str, std::collections::BTreeSet<Uuid>> = b
        .assignments
        .iter()
        .map(|a| (a.slot.as_str(), a.user_ids.iter().copied().collect()))
        .collect();
    let payload = Sha256::digest(
        serde_json::to_vec(
            &json!({"template":id,"version":b.version,"name":b.name,"assignments":canonical}),
        )
        .map_err(|_| invalid())?,
    )
    .to_vec();
    Ok((digest, payload))
}
/// An earlier application by this admin with the same idempotency key:
/// its workspace, payload digest and version.
pub(super) async fn recorded_application(
    tx: &mut Transaction<'_, Postgres>,
    applied_by: Uuid,
    digest: &[u8],
) -> Result<Option<(Uuid, Vec<u8>, i32)>, ManagementError> {
    Ok(sqlx::query_as("SELECT workspace_id,payload_digest,version FROM workspace_template_applications WHERE applied_by=$1 AND idempotency_digest=$2")
        .bind(applied_by)
        .bind(digest)
        .fetch_optional(&mut **tx)
        .await?)
}
/// The response to a replayed key: the recorded application, unless the key
/// was used for another payload.
pub(super) fn replay(
    id: Uuid,
    recorded: (Uuid, Vec<u8>, i32),
    payload: &[u8],
) -> Result<Json<Value>, ManagementError> {
    let (ws, old, version) = recorded;
    if old != payload {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "This idempotency key was used for another application",
        )
        .into());
    }
    Ok(Json(
        json!({"workspace_id":ws,"template_id":id,"version":version,"replayed":true}),
    ))
}
/// The published version's number, content and hash, bound to the version the
/// applier reviewed.
pub(super) async fn reviewed_version(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    reviewed: i32,
) -> Result<(i32, Value, Vec<u8>), ManagementError> {
    let published: Option<(i32, Value, Vec<u8>)> = sqlx::query_as("SELECT version,content,content_hash FROM workspace_template_versions WHERE template_id=$1 AND published_at IS NOT NULL AND retired_at IS NULL")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspace_templates WHERE id=$1)")
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
    if !exists {
        return Err(missing().into());
    }
    // Bound to the version the applier reviewed: a newer, retired or
    // unpublished version is not applied.
    let Some(found) = published.filter(|(v, _, _)| *v == reviewed) else {
        return Err(ManagementError::detailed(
            StatusCode::CONFLICT,
            "template_version_stale",
            "This template version is not the published one; review the template again".into(),
            json!({}),
        ));
    };
    Ok(found)
}
/// Re-validated now: references must still exist.
/// Only a validation refusal is "no longer fits"; storage errors propagate.
pub(super) async fn revalidated_content(
    tx: &mut Transaction<'_, Postgres>,
    content: &Value,
) -> Result<Content, ManagementError> {
    match validate_content(tx, content).await {
        Ok(c) => Ok(c),
        Err(e) if e.0 == StatusCode::BAD_REQUEST => Err(ManagementError::detailed(
            StatusCode::CONFLICT,
            "template_reference_unavailable",
            "A catalog or model in this template no longer exists".into(),
            json!({}),
        )),
        Err(e) => Err(e.into()),
    }
}
/// Takes the type and workspace scopes before the first write, then creates
/// the workspace and audits it.
pub(super) async fn create_workspace(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    ws: Uuid,
    kind: locks::WorkspaceType,
    c: &Content,
    name: &str,
) -> Result<(), ManagementError> {
    // Type and workspace scopes before the first write. The workspace's own
    // writes below (override, selections, policy) re-take its Workspace scope
    // through triggers, so the users' scopes, a later class, come after them.
    locks::exclusive(tx, [locks::Scope::Type(kind), locks::Scope::Workspace(ws)]).await?;
    sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,$2,$3)")
        .bind(ws)
        .bind(name)
        .bind(&c.kind)
        .execute(&mut **tx)
        .await?;
    audit(
        tx,
        u,
        Some(ws),
        "workspace.created",
        "workspace",
        Some(ws),
        json!({"kind":c.kind}),
    )
    .await?;
    Ok(())
}
/// The template's catalog replacement, model selections and local limits.
pub(super) async fn apply_configuration(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    ws: Uuid,
    c: &Content,
) -> Result<(), ManagementError> {
    if let Some(catalogs) = &c.catalogs.catalog_ids {
        catalogs::template_catalogs(tx, u, ws, catalogs).await?;
    }
    for model in &c.model_ids {
        if !catalogs::template_model(tx, u, ws, *model).await? {
            return Err(ManagementError::detailed(
                StatusCode::CONFLICT,
                "template_model_unavailable",
                "A model in this template is no longer available from its catalogs".into(),
                json!({}),
            ));
        }
    }
    if let Some(limits) = &c.limits
        && governance::apply_template_limits(tx, u, ws, limits)
            .await?
            .is_none()
    {
        return Err(ManagementError::detailed(
            StatusCode::CONFLICT,
            "template_limits_exceed_platform",
            "This template's limits are looser than the platform's for this workspace type".into(),
            json!({}),
        ));
    }
    Ok(())
}
/// Locks every member's User scope in one canonical call (key order, not plan
/// order), after the workspace's writes and before the grants, then adds them.
pub(super) async fn add_members(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    ws: Uuid,
    plan: &[Planned<'_>],
) -> Result<(), ManagementError> {
    locks::exclusive(
        tx,
        plan.iter().map(|(user, _, _)| locks::Scope::User(*user)),
    )
    .await?;
    for (user, role, slot) in plan {
        if !members::template_member(tx, u, ws, *user, role).await? {
            return Err(ManagementError::detailed(
                StatusCode::CONFLICT,
                "template_assignment_ineligible",
                "A person assigned to a slot cannot have workspace access".into(),
                json!({"slot":slot}),
            ));
        }
    }
    Ok(())
}
/// The immutable record of one application.
pub(super) struct Application<'a> {
    pub(super) template: Uuid,
    pub(super) version: i32,
    pub(super) hash: &'a [u8],
    pub(super) workspace: Uuid,
    pub(super) applied_by: Uuid,
    pub(super) digest: &'a [u8],
    pub(super) payload: &'a [u8],
}
pub(super) async fn record_application(
    tx: &mut Transaction<'_, Postgres>,
    a: &Application<'_>,
) -> Result<(), ManagementError> {
    sqlx::query("INSERT INTO workspace_template_applications(id,template_id,version,content_hash,workspace_id,applied_by,idempotency_digest,payload_digest) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(Uuid::now_v7())
        .bind(a.template)
        .bind(a.version)
        .bind(a.hash)
        .bind(a.workspace)
        .bind(a.applied_by)
        .bind(a.digest)
        .bind(a.payload)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
