//! Typed deployment enablement operation. HTTP handlers own transactions and
//! audit; preparing a proposal and evaluating it never performs a write.
use super::*;
use crate::providers::capabilities;
use resources::Enabled;

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
}
impl DeploymentChange {
    pub(super) async fn prepare(
        tx: &mut Transaction<'_, Postgres>,
        u: &impl Actor,
        id: Uuid,
        input: &Enabled,
    ) -> Result<Self, ManagementError> {
        resources::require_platform_admin(&resources::platform_role_snapshot(tx, u.user_id()).await?)?;
        let before = sqlx::query_as::<_, DeploymentState>(
            "SELECT d.id,d.model_id,m.public_name,d.enabled,m.enabled model_enabled,p.enabled connection_enabled,p.provider,m.supported_protocols FROM deployments d JOIN models m ON m.id=d.model_id JOIN provider_connections p ON p.id=d.provider_connection_id WHERE d.id=$1",
        ).bind(id).fetch_optional(&mut **tx).await?.ok_or_else(missing)?;
        Self::validate(before, input.enabled)
    }
    fn validate(before: DeploymentState, enabled: bool) -> Result<Self, ManagementError> {
        // Preserve apply's validation even for an already enabled route.
        // Unsupported legacy routes can always be disabled.
        if enabled && let Some(reason) = capabilities::unsupported_reason(&before.provider, &before.supported_protocols) {
            return Err(ManagementError::route_unsupported(format!("{reason}.")));
        }
        Ok(Self { before, enabled })
    }
    pub(super) async fn apply(&self, tx: &mut Transaction<'_, Postgres>) -> Result<(), ApiError> {
        if sqlx::query("UPDATE deployments SET enabled=$2 WHERE id=$1")
            .bind(self.before.id).bind(self.enabled).execute(&mut **tx).await?.rows_affected() != 1 {
            return Err(missing());
        }
        Ok(())
    }
}
