//! Typed deployment enablement operation. HTTP handlers own transactions and
//! audit; preparing a proposal and evaluating it never performs a write.
use super::*;
use crate::providers::capabilities;
use resources::Enabled;
use serde::Serialize;

#[derive(sqlx::FromRow)]
struct DeploymentState {
    id: Uuid,
    model_id: Uuid,
    public_name: String,
    enabled: bool,
    revision: i64,
    model_enabled: bool,
    connection_enabled: bool,
    provider: String,
    supported_protocols: Vec<String>,
}
/// A validated, normalized proposal (the only input is a typed boolean).
/// This is not authority to commit: apply's caller must hold `catalog_tx`'s
/// exclusive lock and live authorization for the entire operation.
pub(super) struct DeploymentChange {
    before: DeploymentState,
    enabled: bool,
    serving_before: bool,
    serving_after: bool,
}

const WORKSPACE_LIMIT: i64 = 100;
#[derive(sqlx::FromRow, Serialize)]
struct AffectedWorkspace {
    id: Uuid,
    name: String,
    kind: String,
}
/// All inputs to the pure evaluator, observed in one primary snapshot.
struct PreviewSnapshot {
    change: DeploymentChange,
    observed_at: chrono::DateTime<chrono::Utc>,
    dependency_versions: Value,
    other_serving_routes: i64,
    workspaces: Vec<AffectedWorkspace>,
    workspace_count: i64,
}
impl PreviewSnapshot {
    async fn load(
        tx: &mut Transaction<'_, Postgres>,
        change: DeploymentChange,
    ) -> Result<Self, ApiError> {
        let (observed_at, dependency_versions) = sqlx::query_as::<_, (chrono::DateTime<chrono::Utc>, Value)>(
            "SELECT transaction_timestamp(),(SELECT jsonb_object_agg(topic,version::text) FROM config_versions WHERE topic IN ('catalog','access'))",
        ).fetch_one(&mut **tx).await?;
        let other_serving_routes = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM deployments d WHERE d.model_id=$1 AND d.id<>$2 AND {}",
            *resources::SERVING_ROUTE,
        ))
        .bind(change.before.model_id)
        .bind(change.before.id)
        .fetch_one(&mut **tx)
        .await?;
        let changed = change.serving_before != change.serving_after;
        // Platform configuration authority covers shared metadata only. Do not
        // read foreign personal IDs/owners/keys, even to paginate or count them.
        // workspace_model_allowed applies live catalog replacement/inheritance
        // and independent direct assignment before counts or LIMIT. EXISTS
        // semantics avoid double-counting selections offered by many catalogs.
        let visible = "FROM workspaces w WHERE $2 AND w.kind IN ('team','project') AND w.disabled_at IS NULL AND workspace_model_allowed(w.id,$1)";
        let workspace_count = sqlx::query_scalar(&format!("SELECT count(*) {visible}"))
            .bind(change.before.model_id)
            .bind(changed)
            .fetch_one(&mut **tx)
            .await?;
        let workspaces = sqlx::query_as(&format!(
            "SELECT w.id,w.name,w.kind {visible} ORDER BY w.name,w.id LIMIT $3"
        ))
        .bind(change.before.model_id)
        .bind(changed)
        .bind(WORKSPACE_LIMIT)
        .fetch_all(&mut **tx)
        .await?;
        Ok(Self {
            change,
            observed_at,
            dependency_versions,
            other_serving_routes,
            workspaces,
            workspace_count,
        })
    }
    /// No database, locks, side effects, inference simulation or provider calls.
    fn evaluate(self) -> Value {
        let c = self.change;
        let lost = c.serving_before && !c.serving_after;
        let gained = !c.serving_before && c.serving_after;
        json!({
            "target": {"kind":"deployment","id":c.before.id},
            "expected_revision": c.before.revision.to_string(),
            "diff": if c.before.enabled == c.enabled { vec![] } else { vec![json!({"field":"enabled","before":c.before.enabled,"after":c.enabled})] },
            "serving_routes_gained": if gained { vec![c.before.id] } else { vec![] },
            "serving_routes_lost": if lost { vec![c.before.id] } else { vec![] },
            "models_losing_last_serving_route": if lost && self.other_serving_routes == 0 { vec![json!({"id":c.before.model_id,"public_name":c.before.public_name})] } else { vec![] },
            "affected_workspaces": {"data":self.workspaces,"total":self.workspace_count,"limit":WORKSPACE_LIMIT,"truncated":self.workspace_count>WORKSPACE_LIMIT},
            "observed_at": self.observed_at,
            "dependency_versions": self.dependency_versions,
            "warnings": [
                "Configuration estimate, not a reservation. Apply revalidates live configuration and authority; this preview does not authorize a commit.",
                "Serving eligibility is configuration-only, not a price, health, quota or admission guarantee. Existing admitted requests may finish.",
                "Affected workspace details and counts cover active shared workspaces with current model selections only. Personal workspace effects are omitted.",
                "The expected revision is the deployment enablement revision. The configuration apply route refuses a stale revision; the existing PATCH and DELETE routes do not enforce it."
            ]
        })
    }
}

pub(super) async fn preview(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(id): Path<Uuid>,
    Json(input): Json<Enabled>,
) -> DetailedResult {
    if u.token_id().is_some() {
        return Err(denied().into());
    }
    let mut tx = resources::read_snapshot(&s).await?;
    let change = DeploymentChange::prepare(&mut tx, &u, id, &input).await?;
    let snapshot = PreviewSnapshot::load(&mut tx, change).await?;
    tx.commit().await?;
    Ok(Json(snapshot.evaluate()))
}

impl DeploymentChange {
    pub(super) async fn prepare(
        tx: &mut Transaction<'_, Postgres>,
        u: &impl Actor,
        id: Uuid,
        input: &Enabled,
    ) -> Result<Self, ManagementError> {
        resources::require_platform_admin(
            &resources::platform_role_snapshot(tx, u.user_id()).await?,
        )?;
        let before = sqlx::query_as::<_, DeploymentState>(
            "SELECT d.id,d.model_id,m.public_name,d.enabled,d.revision,m.enabled model_enabled,p.enabled connection_enabled,p.provider,m.supported_protocols FROM deployments d JOIN models m ON m.id=d.model_id JOIN provider_connections p ON p.id=d.provider_connection_id WHERE d.id=$1",
        ).bind(id).fetch_optional(&mut **tx).await?.ok_or_else(missing)?;
        Self::validate(before, input.enabled)
    }
    fn validate(before: DeploymentState, enabled: bool) -> Result<Self, ManagementError> {
        // Preserve apply's validation even for an already enabled route.
        // Unsupported legacy routes can always be disabled.
        if enabled
            && let Some(reason) =
                capabilities::unsupported_reason(&before.provider, &before.supported_protocols)
        {
            return Err(ManagementError::route_unsupported(format!("{reason}.")));
        }
        // Both preview and apply prepare the same serving transition. These
        // are estimates, not additional apply restrictions: enabling a route
        // on a disabled model/connection remains a valid existing operation.
        let eligible = before.model_enabled
            && before.connection_enabled
            && capabilities::route_serves(&before.provider, &before.supported_protocols);
        let serving_before = before.enabled && eligible;
        let serving_after = enabled && eligible;
        Ok(Self {
            before,
            enabled,
            serving_before,
            serving_after,
        })
    }
    pub(super) async fn apply(&self, tx: &mut Transaction<'_, Postgres>) -> Result<(), ApiError> {
        if sqlx::query("UPDATE deployments SET enabled=$2 WHERE id=$1")
            .bind(self.before.id)
            .bind(self.enabled)
            .execute(&mut **tx)
            .await?
            .rows_affected()
            != 1
        {
            return Err(missing());
        }
        Ok(())
    }
}

/// A guarded apply: the enablement to write and the revision it was reviewed
/// against, optionally restoring a recorded change (#87).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ApplyInput {
    enabled: bool,
    expected_revision: String,
    #[serde(default)]
    restore_of: Option<Uuid>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RestoreInput {
    change_id: Uuid,
}
/// A decimal revision as the preview reports it: no sign, no padding.
fn revision(raw: &str) -> Result<i64, ApiError> {
    if raw.is_empty() || raw.len() > 19 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    raw.parse().map_err(|_| invalid())
}
/// The caller previews again for the current revision; the error carries only
/// the shared envelope.
fn revision_conflict() -> ManagementError {
    ManagementError::detailed(
        StatusCode::CONFLICT,
        "revision_conflict",
        "The deployment changed since this preview; review the current configuration again.".into(),
        json!({}),
    )
}
/// A recorded snapshot of this deployment. Another deployment's change is as
/// unknown as a missing one; a legacy event without values cannot be restored.
async fn source(
    tx: &mut Transaction<'_, Postgres>,
    deployment: Uuid,
    change: Uuid,
) -> Result<(bool, i64, chrono::DateTime<chrono::Utc>), ManagementError> {
    if let Some(row) = sqlx::query_as(
        "SELECT after_enabled,revision,created_at FROM configuration_changes WHERE id=$1 AND deployment_id=$2",
    )
    .bind(change)
    .bind(deployment)
    .fetch_optional(&mut **tx)
    .await?
    {
        return Ok(row);
    }
    let legacy: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT FROM audit_events WHERE id=$1 AND resource_id=$2 AND workspace_id IS NULL AND resource_type IN ('deployment','deployments') AND action IN ('deployment.created','configuration.updated'))",
    )
    .bind(change)
    .bind(deployment)
    .fetch_one(&mut **tx)
    .await?;
    if legacy {
        return Err(ManagementError::detailed(
            StatusCode::CONFLICT,
            "restore_values_unavailable",
            "This change was recorded without its values, so it cannot be restored.".into(),
            json!({}),
        ));
    }
    Err(missing().into())
}

/// Preview of restoring the configuration a recorded change produced. Like
/// every preview it writes nothing and authorizes nothing.
pub(super) async fn restore_preview(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(id): Path<Uuid>,
    Json(input): Json<RestoreInput>,
) -> DetailedResult {
    if u.token_id().is_some() {
        return Err(denied().into());
    }
    let mut tx = resources::read_snapshot(&s).await?;
    resources::require_platform_admin(
        &resources::platform_role_snapshot(&mut tx, u.user_id()).await?,
    )?;
    let (enabled, source_revision, created_at) = source(&mut tx, id, input.change_id).await?;
    let change = DeploymentChange::prepare(&mut tx, &u, id, &Enabled { enabled }).await?;
    let snapshot = PreviewSnapshot::load(&mut tx, change).await?;
    tx.commit().await?;
    let mut preview = snapshot.evaluate();
    preview["restore"] = json!({"change_id":input.change_id,"revision":source_revision.to_string(),"created_at":created_at,"enabled":enabled});
    Ok(Json(preview))
}

/// Apply an enablement only at the reviewed revision. A restore additionally
/// names its source, which must hold the applied value; the new change is
/// linked to it. Requesting the current value records nothing.
pub(super) async fn apply(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(id): Path<Uuid>,
    Json(input): Json<ApplyInput>,
) -> DetailedResult {
    if u.token_id().is_some() {
        return Err(denied().into());
    }
    let expected = revision(&input.expected_revision)?;
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    resources::configuration_actor(&mut tx, &u).await?;
    let change = DeploymentChange::prepare(
        &mut tx,
        &u,
        id,
        &Enabled {
            enabled: input.enabled,
        },
    )
    .await?;
    if change.before.revision != expected {
        return Err(revision_conflict());
    }
    if let Some(restore) = input.restore_of {
        let (value, _, _) = source(&mut tx, id, restore).await?;
        if value != input.enabled {
            return Err(ManagementError::detailed(
                StatusCode::CONFLICT,
                "restore_source_mismatch",
                "The restored change recorded a different value.".into(),
                json!({}),
            ));
        }
    }
    let target = json!({"kind":"deployment","id":id});
    if change.before.enabled == input.enabled {
        tx.commit().await?;
        return Ok(Json(
            json!({"outcome":"unchanged","target":target,"revision":expected.to_string(),"change_id":null,"restored_from":null}),
        ));
    }
    if let Some(restore) = input.restore_of {
        sqlx::query("SELECT set_config('omg.configuration_restore',$1,true)")
            .bind(restore.to_string())
            .execute(&mut *tx)
            .await?;
    }
    // The revision is the precondition of the write itself as well.
    if sqlx::query("UPDATE deployments SET enabled=$2 WHERE id=$1 AND revision=$3")
        .bind(id)
        .bind(input.enabled)
        .bind(expected)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        != 1
    {
        return Err(missing().into());
    }
    let (change_id, revision): (Uuid, i64) = sqlx::query_as(
        "SELECT id,revision FROM configuration_changes WHERE deployment_id=$1 ORDER BY revision DESC LIMIT 1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(
        json!({"outcome":"applied","target":target,"revision":revision.to_string(),"change_id":change_id,"restored_from":input.restore_of}),
    ))
}
