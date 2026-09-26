//! Durable distributed governance. All writers serialize on the organization row.
use chrono::{DateTime, Utc};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::{
    inference::{
        error::InferenceError,
        repository::{ExecutionFinish, ExecutionStart, Outcome},
        types::{ChatRequest, Usage},
    },
    store::Store,
};

type Tx<'a> = Transaction<'a, Postgres>;

fn storage(_: sqlx::Error) -> InferenceError {
    InferenceError::Storage
}

async fn lock(tx: &mut Tx<'_>, org: Uuid) -> Result<(), InferenceError> {
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM organizations WHERE id=$1 FOR NO KEY UPDATE")
        .bind(org)
        .fetch_one(&mut **tx)
        .await
        .map_err(storage)?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Price {
    id: Uuid,
    input_microusd_per_million: i64,
    output_microusd_per_million: i64,
    input_token_limit: i64,
    output_token_limit: i64,
}

/// Round each token category up to a USD micro-unit; never use floats.
fn cost(input: i64, output: i64, price: &Price) -> Result<i64, InferenceError> {
    fn component(tokens: i64, rate: i64) -> Option<i128> {
        if tokens < 0 || rate < 0 {
            return None;
        }
        (i128::from(tokens) * i128::from(rate))
            .checked_add(999_999)
            .map(|v| v / 1_000_000)
    }
    let total = component(input, price.input_microusd_per_million)
        .and_then(|v| component(output, price.output_microusd_per_million)?.checked_add(v))
        .ok_or(InferenceError::Storage)?;
    i64::try_from(total).map_err(|_| InferenceError::Storage)
}

#[derive(sqlx::FromRow)]
struct Policy {
    workspace_id: Option<Uuid>,
    api_key_id: Option<Uuid>,
    requests_per_minute: Option<i64>,
    tokens_per_minute: Option<i64>,
    concurrent_requests: Option<i64>,
    monthly_budget_microusd: Option<i64>,
}

/// Insert started execution and conservative reservations atomically before dispatch.
pub async fn admit(
    store: &Store,
    record: &ExecutionStart,
    request: &ChatRequest,
    lease_seconds: i64,
) -> Result<(), InferenceError> {
    admit_checked(store, record, request, lease_seconds, None).await
}
/// Production admissions verify that routing's cached target still matches the priced target.
pub async fn admit_for_deployment(
    store: &Store,
    record: &ExecutionStart,
    request: &ChatRequest,
    lease_seconds: i64,
    deployment: &crate::inference::types::Deployment,
) -> Result<(), InferenceError> {
    admit_checked(store, record, request, lease_seconds, Some(deployment)).await
}
async fn admit_checked(
    store: &Store,
    record: &ExecutionStart,
    request: &ChatRequest,
    lease_seconds: i64,
    expected: Option<&crate::inference::types::Deployment>,
) -> Result<(), InferenceError> {
    // Bound interval arithmetic; this is a lease, not indefinite ownership.
    if !(1..=86_400).contains(&lease_seconds) {
        return Err(InferenceError::Configuration);
    }
    let org = record.principal.organization_id;
    let workspace = record.principal.workspace_id;
    let key = record.principal.key_id;
    let mut tx = store.pool.begin().await.map_err(storage)?;
    sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
    lock(&mut tx, org).await?;
    let lineage = crate::auth::revalidate(&mut tx, &record.principal)
        .await
        .map_err(storage)?
        .ok_or(InferenceError::ModelUnavailable)?;
    // No row lock on restrictions is needed (including absent headers): creation
    // and entitlement removal take catalog then org locks, and rotation takes
    // the org lock. Headers are immutable through management. Check the live
    // deployment UUID, never the alias or a cached candidate's permissions.
    let current = sqlx::query_as::<_, crate::inference::types::Deployment>(
        "SELECT d.id,p.provider,d.upstream_model,p.credential_ref,p.endpoint,p.region
         FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id
         JOIN models m ON m.id=d.model_id
         JOIN organization_model_grants g ON g.model_id=m.id AND g.organization_id=$1
         WHERE d.id=$2 AND g.public_name=$3 AND m.enabled AND d.enabled AND p.enabled
         AND (
           NOT EXISTS (SELECT 1 FROM key_model_restrictions r WHERE r.organization_id=$1
             AND r.workspace_id=$4 AND r.governance_key_id=$5)
           OR EXISTS (SELECT 1 FROM key_model_selections s WHERE s.organization_id=$1
             AND s.workspace_id=$4 AND s.governance_key_id=$5 AND s.model_id=m.id)
         )
         FOR SHARE OF d,p,m,g",
    )
    .bind(org)
    .bind(record.deployment_id)
    .bind(&record.model)
    .bind(workspace)
    .bind(lineage)
    .fetch_optional(&mut *tx)
    .await
    .map_err(storage)?
    .ok_or(InferenceError::ModelUnavailable)?;
    let workspace_grant: Option<Uuid> = sqlx::query_scalar(
        "SELECT wg.model_id FROM workspace_model_grants wg JOIN deployments d ON d.model_id=wg.model_id
         WHERE wg.organization_id=$1 AND wg.workspace_id=$2 AND d.id=$3 FOR SHARE OF wg")
        .bind(org).bind(workspace).bind(record.deployment_id).fetch_optional(&mut *tx).await.map_err(storage)?;
    if workspace_grant.is_none() {
        let individual: Option<Uuid> = sqlx::query_scalar(
            "SELECT ug.model_id FROM user_model_grants ug JOIN deployments d ON d.model_id=ug.model_id
             JOIN workspaces w ON w.organization_id=ug.organization_id AND w.id=$2
             WHERE ug.organization_id=$1 AND d.id=$3 AND ug.user_id=$4
             AND w.kind='personal' AND w.owner_user_id=ug.user_id FOR SHARE OF ug")
            .bind(org).bind(workspace).bind(record.deployment_id).bind(record.principal.user_id)
            .fetch_optional(&mut *tx).await.map_err(storage)?;
        if individual.is_none() {
            return Err(InferenceError::ModelUnavailable);
        }
    }
    if record.provider != current.provider {
        return Err(InferenceError::Configuration);
    }
    if let Some(expected) = expected
        && (current.id != expected.id
            || current.provider != expected.provider
            || current.upstream_model != expected.upstream_model
            || current.credential_ref != expected.credential_ref
            || current.endpoint != expected.endpoint
            || current.region != expected.region)
    {
        return Err(InferenceError::Configuration);
    }
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await
        .map_err(storage)?;
    let lease_expires_at = now
        .checked_add_signed(chrono::TimeDelta::seconds(lease_seconds))
        .ok_or(InferenceError::Configuration)?;
    let policies = sqlx::query_as::<_, Policy>(
        "SELECT workspace_id,api_key_id,requests_per_minute,tokens_per_minute,concurrent_requests,monthly_budget_microusd
         FROM governance_policies WHERE organization_id=$1
         AND (workspace_id IS NULL OR workspace_id=$2) AND (api_key_id IS NULL OR api_key_id=$3)
         UNION ALL SELECT NULL::uuid,NULL::uuid,requests_per_minute,tokens_per_minute,concurrent_requests,monthly_budget_microusd
         FROM platform_organization_policies WHERE organization_id=$1",
    ).bind(org).bind(workspace).bind(lineage).fetch_all(&mut *tx).await.map_err(storage)?;
    let price = sqlx::query_as::<_, Price>(
        "SELECT id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit
         FROM deployment_prices WHERE deployment_id=$1 ORDER BY created_at DESC,id DESC LIMIT 1",
    ).bind(record.deployment_id).fetch_optional(&mut *tx).await.map_err(storage)?;
    let requires_bound = policies
        .iter()
        .any(|p| p.tokens_per_minute.is_some() || p.monthly_budget_microusd.is_some());
    let (tokens, held) = if let Some(p) = &price {
        let output = request
            .max_output_tokens
            .filter(|v| *v > 0)
            .map(i64::from)
            .ok_or(InferenceError::Configuration)?;
        if output > p.output_token_limit {
            return Err(InferenceError::Configuration);
        }
        let tokens = p
            .input_token_limit
            .checked_add(output)
            .ok_or(InferenceError::Configuration)?;
        let held =
            cost(p.input_token_limit, output, p).map_err(|_| InferenceError::Configuration)?;
        (Some(tokens), Some(held))
    } else if requires_bound {
        return Err(InferenceError::Configuration);
    } else {
        (None, None)
    };
    for policy in policies {
        // SUM(BIGINT) is NUMERIC in PostgreSQL; comparison cannot wrap an i64.
        let allowed: bool = sqlx::query_scalar(r#"
            SELECT
              ($5::bigint IS NULL OR count(*) FILTER (WHERE minute_start=date_trunc('minute',$4::timestamptz,'UTC'))::numeric + 1 <= $5)
              AND ($6::bigint IS NULL OR (
                count(*) FILTER (WHERE minute_start=date_trunc('minute',$4::timestamptz,'UTC') AND reserved_tokens IS NULL)=0
                AND coalesce(sum(greatest(coalesce(reserved_tokens,0)::numeric,coalesce(input_tokens,0)::numeric+coalesce(output_tokens,0)::numeric)) FILTER (WHERE minute_start=date_trunc('minute',$4::timestamptz,'UTC')),0) + $9::bigint <= $6))
              AND ($7::bigint IS NULL OR count(*) FILTER (WHERE state='pending' AND lease_expires_at>$4)::numeric + 1 <= $7)
              AND ($8::bigint IS NULL OR (
                count(*) FILTER (WHERE month_start=date_trunc('month',$4::timestamptz,'UTC') AND held_microusd IS NULL AND actual_microusd IS NULL)=0
                AND coalesce(sum(coalesce(actual_microusd,held_microusd)) FILTER (WHERE month_start=date_trunc('month',$4::timestamptz,'UTC')),0) + $10::bigint <= $8))
            FROM governance_reservations WHERE organization_id=$1
              AND ($2::uuid IS NULL OR workspace_id=$2) AND ($3::uuid IS NULL OR api_key_id IN (SELECT id FROM api_keys WHERE organization_id=$1 AND governance_key_id=$3))
              AND (minute_start=date_trunc('minute',$4::timestamptz,'UTC') OR month_start=date_trunc('month',$4::timestamptz,'UTC') OR (state='pending' AND lease_expires_at>$4))
        "#).bind(org).bind(policy.workspace_id).bind(policy.api_key_id).bind(now)
            .bind(policy.requests_per_minute).bind(policy.tokens_per_minute)
            .bind(policy.concurrent_requests).bind(policy.monthly_budget_microusd)
            .bind(tokens).bind(held).fetch_one(&mut *tx).await.map_err(storage)?;
        if !allowed {
            return Err(InferenceError::Busy);
        }
    }
    sqlx::query(r#"INSERT INTO inference_executions
        (id,organization_id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,started_at,root_request_id,attempt_number)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'started',$9,$10,$11)"#)
        .bind(record.id).bind(org).bind(workspace).bind(key).bind(record.deployment_id)
        .bind(&record.model).bind(&record.provider).bind(record.streamed).bind(now)
        .bind(record.root_request_id).bind(record.attempt_number)
        .execute(&mut *tx).await.map_err(storage)?;
    sqlx::query(r#"INSERT INTO governance_reservations
        (execution_id,organization_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state,reserved_tokens,held_microusd)
        VALUES ($1,$2,$3,$4,$5,$6,$7,date_trunc('minute',$7::timestamptz,'UTC'),date_trunc('month',$7::timestamptz,'UTC'),$8,'pending',$9,$10)"#)
        .bind(record.id).bind(org).bind(workspace).bind(key).bind(record.deployment_id)
        .bind(price.as_ref().map(|p|p.id)).bind(now).bind(lease_expires_at).bind(tokens).bind(held)
        .execute(&mut *tx).await.map_err(storage)?;
    if held.is_some() {
        ledger(
            &mut tx,
            org,
            record.id,
            "hold",
            held,
            Usage::default(),
            None,
        )
        .await?;
    }
    tx.commit().await.map_err(storage)
}

#[derive(sqlx::FromRow)]
struct Reservation {
    workspace_id: Uuid,
    deployment_id: Uuid,
    price_id: Option<Uuid>,
    state: String,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
}

async fn reservation(tx: &mut Tx<'_>, org: Uuid, id: Uuid) -> Result<Reservation, InferenceError> {
    sqlx::query_as("SELECT organization_id,workspace_id,deployment_id,price_id,state,input_tokens,output_tokens FROM governance_reservations WHERE organization_id=$1 AND execution_id=$2")
        .bind(org).bind(id).fetch_one(&mut **tx).await.map_err(storage)
}

fn usage_values(usage: Usage) -> Result<(Option<i64>, Option<i64>), InferenceError> {
    Ok((
        usage
            .input_tokens
            .map(i64::try_from)
            .transpose()
            .map_err(|_| InferenceError::Storage)?,
        usage
            .output_tokens
            .map(i64::try_from)
            .transpose()
            .map_err(|_| InferenceError::Storage)?,
    ))
}

async fn pinned_cost(
    tx: &mut Tx<'_>,
    r: &Reservation,
    usage: Usage,
) -> Result<Option<i64>, InferenceError> {
    let (input, output) = usage_values(usage)?;
    let (Some(price_id), Some(input), Some(output)) = (r.price_id, input, output) else {
        return Ok(None);
    };
    let p = sqlx::query_as::<_, Price>("SELECT id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit FROM deployment_prices WHERE deployment_id=$1 AND id=$2")
        .bind(r.deployment_id).bind(price_id).fetch_one(&mut **tx).await.map_err(storage)?;
    cost(input, output, &p).map(Some)
}

async fn ledger(
    tx: &mut Tx<'_>,
    org: Uuid,
    id: Uuid,
    kind: &str,
    amount: Option<i64>,
    usage: Usage,
    evidence: Option<&str>,
) -> Result<(), InferenceError> {
    let (input, output) = usage_values(usage)?;
    // Migration already records unknown cost for legacy pending executions.
    // Preserve that immutable event rather than failing their later completion.
    let insert = if kind == "unknown" {
        "INSERT INTO monetary_ledger (id,organization_id,execution_id,kind,amount_microusd,input_tokens,output_tokens,evidence) VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT (organization_id,execution_id,kind) DO NOTHING"
    } else {
        "INSERT INTO monetary_ledger (id,organization_id,execution_id,kind,amount_microusd,input_tokens,output_tokens,evidence) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)"
    };
    sqlx::query(insert)
        .bind(Uuid::new_v4())
        .bind(org)
        .bind(id)
        .bind(kind)
        .bind(amount)
        .bind(input)
        .bind(output)
        .bind(evidence)
        .execute(&mut **tx)
        .await
        .map_err(storage)?;
    Ok(())
}

/// Complete usage on successful calls settles; all other outcomes retain holds.
/// Repeating an identical terminal finish is harmless, but cannot reopen a lease.
pub async fn finish(store: &Store, record: &ExecutionFinish) -> Result<(), InferenceError> {
    let mut tx = store.pool.begin().await.map_err(storage)?;
    let org: Uuid =
        sqlx::query_scalar("SELECT organization_id FROM inference_executions WHERE id=$1")
            .bind(record.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(storage)?;
    lock(&mut tx, org).await?;
    let r = reservation(&mut tx, org, record.id).await?;
    let (input, output) = usage_values(record.usage)?;
    let outcome = record.outcome.as_str();
    let error = record.error.map(|v| v.code());
    if r.state != "pending" {
        let same: bool = sqlx::query_scalar("SELECT state=$2 AND error_code IS NOT DISTINCT FROM $3::text AND input_tokens IS NOT DISTINCT FROM $4::bigint AND output_tokens IS NOT DISTINCT FROM $5::bigint FROM inference_executions WHERE id=$1 AND organization_id=$6")
            .bind(record.id).bind(outcome).bind(error).bind(input).bind(output).bind(org)
            .fetch_one(&mut *tx).await.map_err(storage)?;
        return if same {
            Ok(())
        } else {
            Err(InferenceError::Storage)
        };
    }
    let actual = if record.outcome == Outcome::Succeeded {
        pinned_cost(&mut tx, &r, record.usage).await?
    } else {
        None
    };
    // Unknown usage is not zero. Reported categories are only a lower bound:
    // grow (never shrink) an unresolved hold when that floor exceeds the bound.
    let floor = if actual.is_none() {
        pinned_cost(
            &mut tx,
            &r,
            Usage {
                input_tokens: Some(record.usage.input_tokens.unwrap_or(0)),
                output_tokens: Some(record.usage.output_tokens.unwrap_or(0)),
            },
        )
        .await?
    } else {
        None
    };
    let state = if actual.is_some() {
        "settled"
    } else {
        "unknown"
    };
    let changed = sqlx::query("UPDATE inference_executions SET state=$3,error_code=$4,input_tokens=$5,output_tokens=$6,elapsed_ms=$7,completed_at=clock_timestamp() WHERE organization_id=$1 AND id=$2 AND state='started'")
        .bind(org).bind(record.id).bind(outcome).bind(error).bind(input).bind(output)
        .bind(record.elapsed_ms.min(i64::MAX as u64) as i64).execute(&mut *tx).await.map_err(storage)?.rows_affected();
    if changed != 1 {
        return Err(InferenceError::Storage);
    }
    sqlx::query("UPDATE governance_reservations SET state=$3,actual_microusd=$4,input_tokens=$5,output_tokens=$6,held_microusd=CASE WHEN $7::bigint IS NULL THEN held_microusd ELSE greatest(held_microusd,$7) END WHERE organization_id=$1 AND execution_id=$2")
        .bind(org).bind(record.id).bind(state).bind(actual).bind(input).bind(output).bind(floor)
        .execute(&mut *tx).await.map_err(storage)?;
    ledger(
        &mut tx,
        org,
        record.id,
        if actual.is_some() {
            "settlement"
        } else {
            "unknown"
        },
        actual.or(floor),
        record.usage,
        None,
    )
    .await?;
    tx.commit().await.map_err(storage)
}

/// Bounded, race-safe crash reconciliation. Unknown budget holds are not refunded.
pub async fn reconcile_expired(store: &Store, limit: i64) -> Result<u64, InferenceError> {
    if !(1..=10_000).contains(&limit) {
        return Err(InferenceError::InvalidRequest);
    }
    let candidates: Vec<(Uuid, Uuid)> = sqlx::query_as("SELECT organization_id,execution_id FROM governance_reservations WHERE state='pending' AND lease_expires_at<=clock_timestamp() ORDER BY lease_expires_at,execution_id LIMIT $1")
        .bind(limit).fetch_all(&store.pool).await.map_err(storage)?;
    let mut count = 0;
    for (org, id) in candidates {
        let mut tx = store.pool.begin().await.map_err(storage)?;
        lock(&mut tx, org).await?;
        let changed = sqlx::query("UPDATE governance_reservations SET state='unknown' WHERE organization_id=$1 AND execution_id=$2 AND state='pending' AND lease_expires_at<=clock_timestamp()")
            .bind(org).bind(id).execute(&mut *tx).await.map_err(storage)?.rows_affected();
        if changed == 1 {
            let changed = sqlx::query("UPDATE inference_executions SET state='cancelled',error_code='lease_expired',completed_at=clock_timestamp() WHERE organization_id=$1 AND id=$2 AND state='started'")
                .bind(org).bind(id).execute(&mut *tx).await.map_err(storage)?.rows_affected();
            if changed != 1 {
                return Err(InferenceError::Storage);
            }
            ledger(&mut tx, org, id, "unknown", None, Usage::default(), None).await?;
            count += 1;
        }
        tx.commit().await.map_err(storage)?;
    }
    Ok(count)
}

/// Operator-authorized evidence-backed usage correction; never assume zero usage.
/// Caller must authorize actor and org/workspace privacy BEFORE invoking this.
pub async fn resolve_usage(
    store: &Store,
    org: Uuid,
    execution: Uuid,
    usage: Usage,
    evidence: &str,
    actor: Uuid,
) -> Result<(), InferenceError> {
    if evidence.trim().is_empty()
        || evidence.chars().count() > 200
        || evidence.chars().any(char::is_control)
    {
        return Err(InferenceError::InvalidRequest);
    }
    let (input, output) = usage_values(usage)?;
    if input.is_none() || output.is_none() {
        return Err(InferenceError::InvalidRequest);
    }
    let mut tx = store.pool.begin().await.map_err(storage)?;
    lock(&mut tx, org).await?;
    let r = reservation(&mut tx, org, execution).await?;
    // Authority and privacy must remain valid after waiting for the ledger lock.
    // Holding a shared user lock serializes operator revocation with this adjustment.
    let authorized:Option<Uuid>=sqlx::query_scalar("SELECT u.id FROM users u JOIN organizations o ON o.id=$2 AND o.disabled_at IS NULL JOIN workspaces w ON w.organization_id=o.id AND w.id=$3 AND w.disabled_at IS NULL WHERE u.id=$1 AND u.platform_admin AND u.disabled_at IS NULL AND (w.kind IN ('team','project') OR w.owner_user_id=u.id) FOR SHARE OF u,w")
        .bind(actor).bind(org).bind(r.workspace_id).fetch_optional(&mut *tx).await.map_err(storage)?;
    if authorized.is_none() {
        return Err(InferenceError::InvalidRequest);
    }
    if r.state == "settled" {
        let reconciled: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM monetary_ledger WHERE organization_id=$1 AND execution_id=$2 AND kind='reconciliation')")
            .bind(org).bind(execution).fetch_one(&mut *tx).await.map_err(storage)?;
        return if reconciled && r.input_tokens == input && r.output_tokens == output {
            Ok(())
        } else {
            Err(InferenceError::InvalidRequest)
        };
    }
    let terminal: bool = sqlx::query_scalar(
        "SELECT state<>'started' FROM inference_executions WHERE organization_id=$1 AND id=$2",
    )
    .bind(org)
    .bind(execution)
    .fetch_one(&mut *tx)
    .await
    .map_err(storage)?;
    if r.state != "unknown" || !terminal {
        return Err(InferenceError::InvalidRequest);
    }
    // Partial reported usage is a floor: evidence must not optimistically erase it.
    if input < r.input_tokens || output < r.output_tokens {
        return Err(InferenceError::InvalidRequest);
    }
    let actual = pinned_cost(&mut tx, &r, usage)
        .await?
        .ok_or(InferenceError::Configuration)?;
    sqlx::query("UPDATE governance_reservations SET state='settled',actual_microusd=$3,input_tokens=$4,output_tokens=$5 WHERE organization_id=$1 AND execution_id=$2")
        .bind(org).bind(execution).bind(actual).bind(input).bind(output).execute(&mut *tx).await.map_err(storage)?;
    sqlx::query("UPDATE inference_executions SET input_tokens=$3,output_tokens=$4 WHERE organization_id=$1 AND id=$2")
        .bind(org).bind(execution).bind(input).bind(output).execute(&mut *tx).await.map_err(storage)?;
    ledger(
        &mut tx,
        org,
        execution,
        "reconciliation",
        Some(actual),
        usage,
        Some(evidence),
    )
    .await?;
    sqlx::query("INSERT INTO audit_events (id,organization_id,workspace_id,actor_user_id,action,target_id) VALUES ($1,$2,$3,$4,'usage.reconciled',$5)")
        .bind(Uuid::new_v4()).bind(org).bind(r.workspace_id).bind(actor).bind(execution)
        .execute(&mut *tx).await.map_err(storage)?;
    tx.commit().await.map_err(storage)
}

#[cfg(test)]
mod tests;
