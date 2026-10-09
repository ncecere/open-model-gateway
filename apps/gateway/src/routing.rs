//! Provider-independent routing over an already authorized, capability-filtered candidate set.
//! No probes or network calls; health is a passive observation, not a guarantee.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use rand::{
    SeedableRng,
    distributions::{Distribution, WeightedIndex},
    rngs::StdRng,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    auth::Principal,
    inference::{error::InferenceError, types::Deployment},
    store::Store,
};

/// Reject oversized configurations rather than silently hiding eligible routes.
pub const MAX_CANDIDATES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutePlan {
    pub deployment_ids: Vec<Uuid>,
    pub max_attempts: usize,
    pub allow_ambiguous_failover: bool,
}

impl Default for RoutePlan {
    fn default() -> Self {
        Self::single(Vec::new())
    }
}

impl RoutePlan {
    /// Preserve repository order, without opting in to another provider attempt.
    pub fn single(deployment_ids: Vec<Uuid>) -> Self {
        Self {
            deployment_ids,
            max_attempts: 1,
            allow_ambiguous_failover: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct RoutingPolicy {
    pub strategy: String,
    pub max_attempts: i32,
    pub allow_ambiguous_failover: bool,
    pub failure_threshold: i32,
    pub cooldown_seconds: i32,
    pub required_residency: Option<String>,
}

impl Default for RoutingPolicy {
    fn default() -> Self {
        Self {
            strategy: "priority".into(),
            max_attempts: 1,
            allow_ambiguous_failover: false,
            failure_threshold: 3,
            cooldown_seconds: 30,
            required_residency: None,
        }
    }
}

impl RoutingPolicy {
    pub fn validate(&self) -> Result<(), InferenceError> {
        if !matches!(self.strategy.as_str(), "priority" | "weighted")
            || !(1..=3).contains(&self.max_attempts)
            || self.failure_threshold < 1
            || !(1..=3600).contains(&self.cooldown_seconds)
            || self
                .required_residency
                .as_deref()
                .is_some_and(|v| !valid_residency(v) || v == "unspecified")
        {
            return Err(InferenceError::Configuration);
        }
        Ok(())
    }
}

#[derive(Clone, sqlx::FromRow)]
struct Candidate {
    deployment_id: Uuid,
    priority: i32,
    weight: i32,
    residency: String,
    operator_disabled: bool,
    circuit_open: bool,
    /// Whole seconds until an open circuit closes (0 when closed).
    cooldown_remaining_seconds: i32,
}

/// The caller owns current authorization and capability checks. The database query
/// additionally fences candidate IDs to live workspace authorization and model. Planning does
/// not reserve a half-open probe or leak a lease if the request is cancelled.
/// Routing configuration and passive health are platform-global resource state.
pub async fn plan(
    store: &Store,
    principal: &Principal,
    model: &str,
    candidates: &[Deployment],
    request_id: Uuid,
) -> Result<RoutePlan, InferenceError> {
    if candidates.len() > MAX_CANDIDATES {
        return Err(InferenceError::Configuration);
    }
    let ids: Vec<_> = candidates.iter().map(|d| d.id).collect();
    validate_ids(&ids)?;
    let policy = sqlx::query_as::<_, RoutingPolicy>(
        r#"SELECT r.strategy, r.max_attempts, r.allow_ambiguous_failover,
                  3 AS failure_threshold, 30 AS cooldown_seconds, r.required_residency
           FROM routing_policies r JOIN models m ON m.id=r.model_id
           WHERE workspace_model_allowed($1,m.id) AND m.public_name=$2"#,
    )
    .bind(principal.workspace_id)
    .bind(model)
    .fetch_optional(&store.pool)
    .await
    .map_err(|_| InferenceError::Storage)?
    .unwrap_or_default();
    let rows = sqlx::query_as::<_, Candidate>(
        r#"SELECT d.id AS deployment_id, COALESCE(r.priority,0) AS priority,
                  COALESCE(r.weight,1) AS weight, COALESCE(r.residency,'unspecified') AS residency,
                  false AS operator_disabled,
                  COALESCE(h.open_until > statement_timestamp(),false) AS circuit_open,
                  CASE WHEN h.open_until > statement_timestamp()
                       THEN LEAST(86400, GREATEST(1, CEIL(EXTRACT(EPOCH FROM h.open_until - statement_timestamp()))))::int
                       ELSE 0 END AS cooldown_remaining_seconds
           FROM unnest($3::uuid[]) WITH ORDINALITY AS input(id, position)
           JOIN deployments d ON d.id=input.id
           JOIN models m ON m.id=d.model_id
           JOIN provider_connections p ON p.id=d.provider_connection_id
           LEFT JOIN deployment_routing r ON r.deployment_id=d.id
           LEFT JOIN deployment_health h ON h.deployment_id=d.id
           WHERE m.public_name=$2 AND workspace_model_allowed($1,m.id) AND d.enabled AND p.enabled
           ORDER BY input.position"#,
    )
    .bind(principal.workspace_id)
    .bind(model)
    .bind(&ids)
    .fetch_all(&store.pool)
    .await
    .map_err(|_| InferenceError::Storage)?;
    let mut seed = Sha256::new();
    seed.update(principal.workspace_id.as_bytes());
    seed.update(request_id.as_bytes());
    seed.update(model.as_bytes());
    order_candidates(&policy, rows, seed.finalize().into())
}

fn validate_ids(ids: &[Uuid]) -> Result<(), InferenceError> {
    if ids.len() > MAX_CANDIDATES || ids.iter().collect::<HashSet<_>>().len() != ids.len() {
        return Err(InferenceError::Configuration);
    }
    if ids.is_empty() {
        return Err(InferenceError::ModelUnavailable);
    }
    Ok(())
}

fn order_candidates(
    policy: &RoutingPolicy,
    mut candidates: Vec<Candidate>,
    seed: [u8; 32],
) -> Result<RoutePlan, InferenceError> {
    policy.validate()?;
    validate_ids(
        &candidates
            .iter()
            .map(|c| c.deployment_id)
            .collect::<Vec<_>>(),
    )?;
    // Validate even disabled candidates: invalid operator configuration must not be hidden.
    if candidates
        .iter()
        .any(|c| !(1..=1000).contains(&c.weight) || !valid_residency(&c.residency))
    {
        return Err(InferenceError::Configuration);
    }
    candidates.retain(|c| {
        !c.operator_disabled
            && policy
                .required_residency
                .as_deref()
                .is_none_or(|label| label == c.residency)
    });
    // Routes in cooldown are skipped. If cooldown is the only reason nothing can
    // serve the model, say so (retryable, with the soonest reopening) instead of
    // reporting the model as missing.
    let cooling = candidates
        .iter()
        .filter(|c| c.circuit_open)
        .map(|c| c.cooldown_remaining_seconds.max(1) as u32)
        .min();
    candidates.retain(|c| !c.circuit_open);
    if candidates.is_empty()
        && let Some(seconds) = cooling
    {
        return Err(InferenceError::RouteCoolingDown(seconds));
    }
    // Stable sorting retains repository order when priority values tie.
    candidates.sort_by_key(|c| c.priority);
    let mut ordered = Vec::with_capacity(candidates.len());
    if policy.strategy == "weighted" {
        let mut rng = StdRng::from_seed(seed);
        while !candidates.is_empty() {
            let tier = candidates
                .iter()
                .take_while(|c| c.priority == candidates[0].priority)
                .count();
            let weights = WeightedIndex::new(candidates[..tier].iter().map(|c| c.weight as u32))
                .map_err(|_| InferenceError::Configuration)?;
            ordered.push(candidates.remove(weights.sample(&mut rng)));
        }
    } else {
        ordered = candidates;
    }
    let first = ordered.first().ok_or(InferenceError::ModelUnavailable)?;
    // Residency is an explicit safety boundary, never inferred from a region/provider.
    let residency = first.residency.clone();
    let first_id = first.deployment_id;
    if policy.max_attempts > 1 {
        ordered.retain(|c| {
            c.deployment_id == first_id || (residency != "unspecified" && c.residency == residency)
        });
    }
    Ok(RoutePlan {
        deployment_ids: ordered.into_iter().map(|c| c.deployment_id).collect(),
        max_attempts: policy.max_attempts as usize,
        allow_ambiguous_failover: policy.allow_ambiguous_failover,
    })
}

fn valid_residency(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

/// This decision is valid ONLY before any ProviderOutput::Stream was returned,
/// even if that stream has not been polled. The engine also enforces the attempt
/// budget and one shared hard deadline; no inference retry occurs in this module.
pub fn may_failover(plan: &RoutePlan, error: InferenceError) -> bool {
    plan.max_attempts > 1
        && match error {
            InferenceError::Busy => true,
            // Ambiguous transport failures may already have incurred provider charges.
            InferenceError::UpstreamUnavailable => plan.allow_ambiguous_failover,
            _ => false,
        }
}

fn affects_health(error: Option<InferenceError>) -> bool {
    matches!(
        error,
        None | Some(
            InferenceError::Busy | InferenceError::UpstreamUnavailable | InferenceError::Timeout
        )
    )
}

/// Record an actual provider result, not local capacity/admission or accounting
/// failures. Success means validated completion (for streams: valid terminal Done),
/// not merely opening a response/stream. Updates serialize atomically in PostgreSQL.
pub async fn record_result(
    store: &Store,
    deployment: Uuid,
    error: Option<InferenceError>,
) -> Result<(), InferenceError> {
    if !affects_health(error) {
        return Ok(());
    }
    let result = sqlx::query(
        r#"WITH policy AS (
            SELECT COALESCE(p.failure_threshold,3) AS threshold,
                   COALESCE(p.cooldown_seconds,30) AS cooldown
            FROM deployments d LEFT JOIN deployment_routing p
              ON p.deployment_id=d.id
            WHERE d.id=$1
        )
        INSERT INTO deployment_health AS h
            (deployment_id, consecutive_failures, open_until, last_observed_at)
        SELECT $1, CASE WHEN $2 THEN 1 ELSE 0 END,
            CASE WHEN $2 AND threshold <= 1 THEN clock_timestamp() + make_interval(secs => cooldown) ELSE NULL END,
            clock_timestamp() FROM policy
        ON CONFLICT (deployment_id) DO UPDATE SET
            consecutive_failures=CASE WHEN $2 THEN LEAST(h.consecutive_failures::bigint+1,2147483647)::integer ELSE 0 END,
            open_until=CASE
                WHEN NOT $2 THEN NULL
                WHEN h.consecutive_failures::bigint+1 >= (SELECT threshold FROM policy)
                    THEN GREATEST(h.open_until, clock_timestamp() + make_interval(secs => (SELECT cooldown FROM policy)))
                ELSE h.open_until END,
            last_observed_at=clock_timestamp()"#,
    ).bind(deployment).bind(error.is_some()).execute(&store.pool)
        .await.map_err(|_| InferenceError::Storage)?;
    if result.rows_affected() != 1 {
        return Err(InferenceError::ModelUnavailable);
    }
    Ok(())
}

/// Sanitized operator view: no endpoint, credential reference, model payload,
/// upstream response, or raw error text. Missing observations remain explicitly unknown.
#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct RouteHealth {
    pub deployment_id: Uuid,
    pub operator_disabled: bool,
    pub consecutive_failures: i32,
    pub open_until: Option<DateTime<Utc>>,
    pub last_observed_at: Option<DateTime<Utc>>,
    pub circuit_open: bool,
}

/// Caller must enforce live platform read authorization.
/// An expired circuit is eligible again, not declared healthy.
pub async fn health(store: &Store, deployment: Uuid) -> Result<RouteHealth, InferenceError> {
    sqlx::query_as::<_, RouteHealth>(
        r#"SELECT d.id AS deployment_id, NOT d.enabled AS operator_disabled,
                  COALESCE(h.consecutive_failures,0) AS consecutive_failures,
                  h.open_until, h.last_observed_at,
                  COALESCE(h.open_until > statement_timestamp(),false) AS circuit_open
           FROM deployments d
           LEFT JOIN deployment_routing r ON r.deployment_id=d.id
           LEFT JOIN deployment_health h ON h.deployment_id=d.id
           WHERE d.id=$1"#,
    )
    .bind(deployment)
    .fetch_optional(&store.pool)
    .await
    .map_err(|_| InferenceError::Storage)?
    .ok_or(InferenceError::ModelUnavailable)
}

#[cfg(test)]
mod tests;
