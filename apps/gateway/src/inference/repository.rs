use async_trait::async_trait;
use uuid::Uuid;

use super::{
    error::InferenceError,
    types::{Deployment, Usage},
};
use crate::{auth::Principal, store::Store};

pub struct ExecutionStart {
    pub id: Uuid,
    pub root_request_id: Uuid,
    pub attempt_number: i32,
    pub principal: Principal,
    pub deployment_id: Uuid,
    pub provider: String,
    pub model: String,
    pub streamed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    Failed,
    Cancelled,
}
impl Outcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

pub struct ExecutionFinish {
    pub id: Uuid,
    pub outcome: Outcome,
    pub error: Option<InferenceError>,
    pub usage: Usage,
    pub elapsed_ms: u64,
}

#[async_trait]
pub trait InferenceRepository: Send + Sync {
    async fn deployments(
        &self,
        principal: &Principal,
        model: &str,
    ) -> Result<Vec<Deployment>, InferenceError>;
    async fn start(&self, record: &ExecutionStart) -> Result<(), InferenceError>;
    async fn finish(&self, record: &ExecutionFinish) -> Result<(), InferenceError>;
    async fn admit(
        &self,
        record: &ExecutionStart,
        _request: &super::types::ChatRequest,
        _lease_seconds: i64,
        _deployment: &Deployment,
    ) -> Result<(), InferenceError> {
        self.start(record).await
    }
    async fn route_plan(
        &self,
        _principal: &Principal,
        _model: &str,
        candidates: &[Deployment],
        _request_id: Uuid,
    ) -> Result<crate::routing::RoutePlan, InferenceError> {
        Ok(crate::routing::RoutePlan {
            deployment_ids: candidates.iter().map(|d| d.id).collect(),
            max_attempts: 1,
            allow_ambiguous_failover: false,
        })
    }
    async fn route_result(
        &self,
        _org: Uuid,
        _deployment: Uuid,
        _error: Option<InferenceError>,
    ) -> Result<(), InferenceError> {
        Ok(())
    }
}

#[async_trait]
impl InferenceRepository for Store {
    async fn deployments(
        &self,
        principal: &Principal,
        model: &str,
    ) -> Result<Vec<Deployment>, InferenceError> {
        let deployments = sqlx::query_as::<_, Deployment>(r#"
            SELECT d.id, p.provider, d.upstream_model, p.credential_ref, p.endpoint, p.region
            FROM models m
            JOIN organization_model_grants g ON g.model_id=m.id AND g.organization_id=$1
            JOIN workspaces w ON w.organization_id=g.organization_id AND w.id=$2 AND w.disabled_at IS NULL
            JOIN deployments d ON d.model_id=m.id
            JOIN provider_connections p ON p.id=d.provider_connection_id
            JOIN api_keys k ON k.organization_id=$1 AND k.workspace_id=$2 AND k.id=$5
                AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>now())
            WHERE g.public_name=$3 AND m.enabled AND d.enabled AND p.enabled AND (
                NOT EXISTS (SELECT 1 FROM key_model_restrictions r WHERE r.organization_id=$1
                    AND r.workspace_id=$2 AND r.governance_key_id=k.governance_key_id)
                OR EXISTS (SELECT 1 FROM key_model_selections s WHERE s.organization_id=$1
                    AND s.workspace_id=$2 AND s.governance_key_id=k.governance_key_id AND s.model_id=m.id)
            ) AND (
                EXISTS (SELECT 1 FROM workspace_model_grants wg WHERE wg.organization_id=$1 AND wg.workspace_id=$2 AND wg.model_id=m.id)
                OR (w.kind='personal' AND w.owner_user_id=$4 AND EXISTS (
                    SELECT 1 FROM user_model_grants ug JOIN organization_memberships om
                    ON om.organization_id=ug.organization_id AND om.user_id=ug.user_id AND om.disabled_at IS NULL
                    JOIN users u ON u.id=ug.user_id AND u.disabled_at IS NULL
                    WHERE ug.organization_id=$1 AND ug.user_id=$4 AND ug.model_id=m.id
                ))
            )
            ORDER BY d.created_at, d.id LIMIT 257
        "#).bind(principal.organization_id).bind(principal.workspace_id).bind(model).bind(principal.user_id).bind(principal.key_id)
            .fetch_all(&self.pool).await.map_err(|_| InferenceError::Storage)?;
        if deployments.len() > crate::routing::MAX_CANDIDATES {
            return Err(InferenceError::Configuration);
        }
        Ok(deployments)
    }

    async fn start(&self, record: &ExecutionStart) -> Result<(), InferenceError> {
        sqlx::query(r#"INSERT INTO inference_executions
            (id, organization_id, workspace_id, api_key_id, deployment_id, public_model, provider, streamed, state, root_request_id, attempt_number)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'started',$9,$10)"#)
            .bind(record.id).bind(record.principal.organization_id).bind(record.principal.workspace_id)
            .bind(record.principal.key_id).bind(record.deployment_id).bind(&record.model)
            .bind(&record.provider).bind(record.streamed).bind(record.root_request_id).bind(record.attempt_number).execute(&self.pool).await
            .map_err(|_| InferenceError::Storage)?;
        Ok(())
    }

    async fn finish(&self, record: &ExecutionFinish) -> Result<(), InferenceError> {
        crate::governance::finish(self, record).await
    }
    async fn admit(
        &self,
        record: &ExecutionStart,
        request: &super::types::ChatRequest,
        lease_seconds: i64,
        deployment: &Deployment,
    ) -> Result<(), InferenceError> {
        crate::governance::admit_for_deployment(self, record, request, lease_seconds, deployment)
            .await
    }
    async fn route_plan(
        &self,
        principal: &Principal,
        model: &str,
        candidates: &[Deployment],
        request_id: Uuid,
    ) -> Result<crate::routing::RoutePlan, InferenceError> {
        crate::routing::plan(self, principal, model, candidates, request_id).await
    }
    async fn route_result(
        &self,
        org: Uuid,
        deployment: Uuid,
        error: Option<InferenceError>,
    ) -> Result<(), InferenceError> {
        crate::routing::record_result(self, org, deployment, error).await
    }
}
