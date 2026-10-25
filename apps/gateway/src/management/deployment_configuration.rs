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
    catalog_version: i64,
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
        let (observed_at, catalog_version, dependency_versions) = sqlx::query_as::<_, (chrono::DateTime<chrono::Utc>, i64, Value)>(
            "SELECT transaction_timestamp(),(SELECT version FROM config_versions WHERE topic='catalog'),(SELECT jsonb_object_agg(topic,version::text) FROM config_versions WHERE topic IN ('catalog','access'))",
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
            catalog_version,
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
            "expected_revision": format!("catalog:{}", self.catalog_version),
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
                "The expected revision is the installation catalog configuration version, not a per-deployment revision. Existing apply routes do not enforce preview preconditions."
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
            "SELECT d.id,d.model_id,m.public_name,d.enabled,m.enabled model_enabled,p.enabled connection_enabled,p.provider,m.supported_protocols FROM deployments d JOIN models m ON m.id=d.model_id JOIN provider_connections p ON p.id=d.provider_connection_id WHERE d.id=$1",
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
