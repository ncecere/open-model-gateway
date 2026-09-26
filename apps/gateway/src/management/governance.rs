//! Browser-session governance configuration and bounded, tenant-scoped reporting.
use super::*;
use crate::inference::{error::InferenceError, types::Usage};
use axum::http::header;

#[cfg(all(test, feature = "integration-tests"))]
mod tests;

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route(
            "/api/v1/orgs/{org}/policy",
            get(org_policy).put(put_org_policy),
        )
        .route(
            "/api/v1/workspaces/{ws}/policy",
            get(workspace_policy).put(put_workspace_policy),
        )
        .route(
            "/api/v1/workspaces/{ws}/keys/{key}/policy",
            get(key_policy).put(put_key_policy),
        )
        .route(
            "/api/v1/orgs/{org}/deployments/{deployment}/prices",
            get(resources::providers).post(resources::providers),
        )
        .route(
            "/api/v1/orgs/{org}/models/{model}/routing",
            get(resources::providers).put(resources::providers),
        )
        .route(
            "/api/v1/orgs/{org}/deployments/{deployment}/routing",
            get(resources::providers).put(resources::providers),
        )
        .route("/api/v1/workspaces/{ws}/cost-summary", get(cost_summary))
        .route("/api/v1/workspaces/{ws}/costs", get(costs))
        .route("/api/v1/workspaces/{ws}/usage-export", get(usage_export))
        .route(
            "/api/v1/workspaces/{ws}/costs/{execution}/reconcile",
            post(reconcile),
        )
}

pub(super) fn platform_routes() -> Router<Store> {
    Router::new()
        .route(
            "/api/v1/platform/orgs/{org}/policy",
            get(platform_policy).put(put_platform_policy),
        )
        .route(
            "/api/v1/platform/deployments/{deployment}/prices",
            get(prices).post(create_price),
        )
        .route(
            "/api/v1/platform/models/{model}/routing",
            get(model_routing).put(put_model_routing),
        )
        .route(
            "/api/v1/platform/deployments/{deployment}/routing",
            get(deployment_routing).put(put_deployment_routing),
        )
}

// A PUT must explicitly supply even nullable fields; omission is not a reset.
fn nullable<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(d)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    #[serde(deserialize_with = "nullable")]
    requests_per_minute: Option<i64>,
    #[serde(deserialize_with = "nullable")]
    tokens_per_minute: Option<i64>,
    #[serde(deserialize_with = "nullable")]
    concurrent_requests: Option<i64>,
    #[serde(deserialize_with = "nullable")]
    monthly_budget_microusd: Option<String>,
}
fn positive_limit(value: i64) -> bool {
    (1..=i64::from(i32::MAX)).contains(&value)
}
fn money(value: &str, positive: bool) -> Result<i64, ApiError> {
    if value.is_empty() || value.len() > 19 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let value: i64 = value.parse().map_err(|_| invalid())?;
    if positive && value == 0 {
        return Err(invalid());
    }
    Ok(value)
}
impl Policy {
    fn budget(&self) -> Result<Option<i64>, ApiError> {
        if [
            self.requests_per_minute,
            self.tokens_per_minute,
            self.concurrent_requests,
        ]
        .into_iter()
        .flatten()
        .any(|v| !positive_limit(v))
        {
            return Err(invalid());
        }
        self.monthly_budget_microusd
            .as_deref()
            .map(|v| money(v, true))
            .transpose()
    }
}

async fn lock_admin<'a>(
    store: &'a Store,
    user: &BrowserPrincipal,
    org: Uuid,
    operator: bool,
) -> Result<Transaction<'a, Postgres>, ApiError> {
    let mut tx = resources::org_tx(store, org).await?;
    let role = resources::live_org_role(&mut tx, user, org).await?;
    if !admin(&role) || (operator && role != "operator") {
        return Err(denied());
    }
    Ok(tx)
}
type Limits = (Option<i64>, Option<i64>, Option<i64>, Option<i64>);
fn limits_json(v: Limits) -> Value {
    json!({"requests_per_minute":v.0,"tokens_per_minute":v.1,"concurrent_requests":v.2,"monthly_budget_microusd":v.3.map(|v|v.to_string())})
}
async fn parent_limits(
    tx: &mut Transaction<'_, Postgres>,
    org: Uuid,
    ws: Option<Uuid>,
    key: bool,
) -> Result<Limits, ApiError> {
    Ok(sqlx::query_as("SELECT min(requests_per_minute),min(tokens_per_minute),min(concurrent_requests),min(monthly_budget_microusd) FROM (SELECT requests_per_minute,tokens_per_minute,concurrent_requests,monthly_budget_microusd FROM platform_organization_policies WHERE organization_id=$1 UNION ALL SELECT requests_per_minute,tokens_per_minute,concurrent_requests,monthly_budget_microusd FROM governance_policies WHERE organization_id=$1 AND api_key_id IS NULL AND (($2::uuid IS NOT NULL AND workspace_id IS NULL) OR ($3 AND workspace_id=$2))) parents")
        .bind(org).bind(ws).bind(key).fetch_one(&mut **tx).await?)
}
fn validate_child(p: &Policy, budget: Option<i64>, ceiling: Limits) -> Result<(), ApiError> {
    for (child, parent) in [
        (p.requests_per_minute, ceiling.0),
        (p.tokens_per_minute, ceiling.1),
        (p.concurrent_requests, ceiling.2),
        (budget, ceiling.3),
    ] {
        if child.zip(parent).is_some_and(|(c, p)| c > p) {
            return Err(invalid());
        }
    }
    Ok(())
}
async fn platform_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(org): Path<Uuid>,
) -> ApiResult {
    let mut tx = lock_admin(&s, &u, org, true).await?;
    let limits = parent_limits(&mut tx, org, None, false).await?;
    tx.commit().await?;
    Ok(Json(json!({"policy":limits_json(limits)})))
}
async fn put_platform_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(org): Path<Uuid>,
    Json(p): Json<Policy>,
) -> ApiResult {
    let budget = p.budget()?;
    let mut tx = lock_admin(&s, &u, org, true).await?;
    sqlx::query("INSERT INTO platform_organization_policies(organization_id,requests_per_minute,tokens_per_minute,concurrent_requests,monthly_budget_microusd) VALUES($1,$2,$3,$4,$5) ON CONFLICT(organization_id) DO UPDATE SET requests_per_minute=excluded.requests_per_minute,tokens_per_minute=excluded.tokens_per_minute,concurrent_requests=excluded.concurrent_requests,monthly_budget_microusd=excluded.monthly_budget_microusd")
        .bind(org).bind(p.requests_per_minute).bind(p.tokens_per_minute).bind(p.concurrent_requests).bind(budget).execute(&mut *tx).await?;
    audit(
        &mut tx,
        u.user_id,
        Some(org),
        None,
        "governance.ceiling_updated",
        Some(org),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
async fn read_policy(
    store: &Store,
    user: &BrowserPrincipal,
    org: Uuid,
    ws: Option<Uuid>,
    key: Option<Uuid>,
) -> ApiResult {
    let mut tx = resources::org_tx(store, org).await?;
    let role = if let Some(ws) = ws {
        resources::workspace_role(&mut tx, user, org, ws).await?
    } else {
        resources::org_admin(&mut tx, user, org).await?;
        "admin".into()
    };
    let ceiling = parent_limits(&mut tx, org, ws, key.is_some()).await?;
    let key = if let Some(key) = key {
        Some(sqlx::query_scalar::<_,Uuid>("SELECT governance_key_id FROM api_keys WHERE organization_id=$1 AND workspace_id=$2 AND id=$3 AND ($4 OR issued_to_user_id=$5) FOR SHARE").bind(org).bind(ws).bind(key).bind(admin(&role)).bind(user.user_id).fetch_optional(&mut *tx).await?.ok_or_else(missing)?)
    } else {
        None
    };
    let value: Option<Value> = sqlx::query_scalar("SELECT jsonb_build_object('requests_per_minute',requests_per_minute,'tokens_per_minute',tokens_per_minute,'concurrent_requests',concurrent_requests,'monthly_budget_microusd',monthly_budget_microusd::text) FROM governance_policies WHERE organization_id=$1 AND workspace_id IS NOT DISTINCT FROM $2::uuid AND api_key_id IS NOT DISTINCT FROM $3::uuid")
        .bind(org).bind(ws).bind(key).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(
        json!({"policy":value.unwrap_or_else(||limits_json((None,None,None,None))),"ceiling":limits_json(ceiling)}),
    ))
}
async fn write_policy(
    store: &Store,
    user: &BrowserPrincipal,
    org: Uuid,
    ws: Option<Uuid>,
    key: Option<Uuid>,
    policy: Policy,
) -> ApiResult {
    let budget = policy.budget()?;
    let mut tx = resources::org_tx(store, org).await?;
    let org_role = resources::live_org_role(&mut tx, user, org).await?;
    let role = if let Some(ws) = ws {
        resources::workspace_role(&mut tx, user, org, ws).await?
    } else {
        org_role.clone()
    };
    if !admin(&role) {
        return Err(denied());
    }
    let ceiling = parent_limits(&mut tx, org, ws, key.is_some()).await?;
    validate_child(&policy, budget, ceiling)?;
    let key = if let Some(key) = key {
        Some(sqlx::query_scalar::<_, Uuid>("SELECT governance_key_id FROM api_keys WHERE organization_id=$1 AND workspace_id=$2 AND id=$3 FOR SHARE")
            .bind(org).bind(ws).bind(key).fetch_optional(&mut *tx).await?.ok_or_else(missing)?)
    } else {
        None
    };
    if !admin(&org_role) {
        // Delegated shared-workspace administrators can tighten, not erase or
        // relax a restriction previously configured by an organization admin.
        let old: Option<Limits> = sqlx::query_as("SELECT requests_per_minute,tokens_per_minute,concurrent_requests,monthly_budget_microusd FROM governance_policies WHERE organization_id=$1 AND workspace_id IS NOT DISTINCT FROM $2::uuid AND api_key_id IS NOT DISTINCT FROM $3::uuid")
            .bind(org).bind(ws).bind(key).fetch_optional(&mut *tx).await?;
        if let Some(old) = old {
            for (new, old) in [
                (policy.requests_per_minute, old.0),
                (policy.tokens_per_minute, old.1),
                (policy.concurrent_requests, old.2),
                (budget, old.3),
            ] {
                // Compare durable local values, not effective limits: a tighter
                // parent today must not mask removal of a cap when it rises later.
                if old.is_some_and(|old| new.is_none_or(|new| new > old)) {
                    return Err(denied());
                }
            }
        }
    }
    let scope = if key.is_some() {
        "key"
    } else if ws.is_some() {
        "workspace"
    } else {
        "organization"
    };
    // Serialized by the organization lock; no partial-index conflict inference needed.
    sqlx::query("DELETE FROM governance_policies WHERE organization_id=$1 AND workspace_id IS NOT DISTINCT FROM $2::uuid AND api_key_id IS NOT DISTINCT FROM $3::uuid")
        .bind(org).bind(ws).bind(key).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO governance_policies(id,organization_id,scope,workspace_id,api_key_id,requests_per_minute,tokens_per_minute,concurrent_requests,monthly_budget_microusd) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
        .bind(Uuid::new_v4()).bind(org).bind(scope).bind(ws).bind(key).bind(policy.requests_per_minute).bind(policy.tokens_per_minute).bind(policy.concurrent_requests).bind(budget).execute(&mut *tx).await?;
    audit(
        &mut tx,
        user.user_id,
        Some(org),
        ws,
        "governance.policy_updated",
        key.or(ws).or(Some(org)),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
async fn org_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(org): Path<Uuid>,
) -> ApiResult {
    read_policy(&s, &u, org, None, None).await
}
async fn put_org_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(org): Path<Uuid>,
    Json(p): Json<Policy>,
) -> ApiResult {
    write_policy(&s, &u, org, None, None, p).await
}
async fn workspace_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
) -> ApiResult {
    let org = resources::workspace_org(&s, ws).await?;
    read_policy(&s, &u, org, Some(ws), None).await
}
async fn put_workspace_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(p): Json<Policy>,
) -> ApiResult {
    let org = resources::workspace_org(&s, ws).await?;
    write_policy(&s, &u, org, Some(ws), None, p).await
}
async fn key_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, key)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    let org = resources::workspace_org(&s, ws).await?;
    read_policy(&s, &u, org, Some(ws), Some(key)).await
}
async fn put_key_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, key)): Path<(Uuid, Uuid)>,
    Json(p): Json<Policy>,
) -> ApiResult {
    let org = resources::workspace_org(&s, ws).await?;
    write_policy(&s, &u, org, Some(ws), Some(key), p).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Price {
    input_microusd_per_million: String,
    output_microusd_per_million: String,
    input_token_limit: i64,
    output_token_limit: i64,
}
async fn prices(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(deployment): Path<Uuid>,
    Query(page): Query<Page>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM deployments WHERE id=$1")
        .bind(deployment)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    let (limit, offset) = page.bounds()?;
    let data: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('id',id,'input_microusd_per_million',input_microusd_per_million::text,'output_microusd_per_million',output_microusd_per_million::text,'input_token_limit',input_token_limit,'output_token_limit',output_token_limit,'created_at',created_at) FROM deployment_prices WHERE deployment_id=$1 ORDER BY created_at DESC,id DESC LIMIT $2 OFFSET $3")
        .bind(deployment).bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
async fn create_price(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(deployment): Path<Uuid>,
    Json(p): Json<Price>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    let input = money(&p.input_microusd_per_million, false)?;
    let output = money(&p.output_microusd_per_million, false)?;
    if !positive_limit(p.input_token_limit) || !positive_limit(p.output_token_limit) {
        return Err(invalid());
    }
    // Reject configurations whose maximum reservation cannot fit the ledger.
    let maximum = (i128::from(input) * i128::from(p.input_token_limit) + 999_999) / 1_000_000
        + (i128::from(output) * i128::from(p.output_token_limit) + 999_999) / 1_000_000;
    if maximum > i128::from(i64::MAX) {
        return Err(invalid());
    }
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM deployments WHERE id=$1 FOR SHARE")
        .bind(deployment)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(id).bind(deployment).bind(input).bind(output).bind(p.input_token_limit).bind(p.output_token_limit).execute(&mut *tx).await?;
    audit(
        &mut tx,
        u.user_id,
        None,
        None,
        "governance.price_created",
        Some(id),
    )
    .await?;
    tx.commit().await?;
    Ok(identifier(id))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelRouting {
    strategy: String,
    max_attempts: i32,
    allow_ambiguous_failover: bool,
    failure_threshold: i32,
    cooldown_seconds: i32,
    #[serde(default)]
    required_residency: Option<String>,
}
async fn model_routing(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(model): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    let policy: Value = sqlx::query_scalar("SELECT jsonb_build_object('strategy',coalesce(p.strategy,'priority'),'max_attempts',coalesce(p.max_attempts,1),'allow_ambiguous_failover',coalesce(p.allow_ambiguous_failover,false),'failure_threshold',coalesce(p.failure_threshold,3),'cooldown_seconds',coalesce(p.cooldown_seconds,30),'required_residency',p.required_residency) FROM models m LEFT JOIN model_routing_policies p ON p.model_id=m.id WHERE m.id=$1")
        .bind(model).fetch_optional(&mut *tx).await?.ok_or_else(missing)?;
    tx.commit().await?;
    Ok(Json(json!({"policy":policy})))
}
async fn put_model_routing(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(model): Path<Uuid>,
    Json(p): Json<ModelRouting>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    if !matches!(p.strategy.as_str(), "priority" | "weighted")
        || !(1..=3).contains(&p.max_attempts)
        || p.failure_threshold < 1
        || !(1..=3600).contains(&p.cooldown_seconds)
    {
        return Err(invalid());
    }
    if p.required_residency
        .as_deref()
        .is_some_and(|v| v == "unspecified" || !residency_valid(v))
    {
        return Err(invalid());
    }
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM models WHERE id=$1 FOR SHARE")
        .bind(model)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    sqlx::query("INSERT INTO model_routing_policies(model_id,strategy,max_attempts,allow_ambiguous_failover,failure_threshold,cooldown_seconds,required_residency) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(model_id) DO UPDATE SET strategy=excluded.strategy,max_attempts=excluded.max_attempts,allow_ambiguous_failover=excluded.allow_ambiguous_failover,failure_threshold=excluded.failure_threshold,cooldown_seconds=excluded.cooldown_seconds,required_residency=excluded.required_residency")
        .bind(model).bind(p.strategy).bind(p.max_attempts).bind(p.allow_ambiguous_failover).bind(p.failure_threshold).bind(p.cooldown_seconds).bind(p.required_residency).execute(&mut *tx).await?;
    audit(
        &mut tx,
        u.user_id,
        None,
        None,
        "governance.model_routing_updated",
        Some(model),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeploymentRouting {
    priority: i32,
    weight: i32,
    residency: String,
    #[serde(default)]
    operator_disabled: bool,
}
fn residency_valid(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && value.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}
async fn deployment_routing(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(deployment): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    let value: Value = sqlx::query_scalar("SELECT jsonb_build_object('routing',jsonb_build_object('priority',coalesce(r.priority,0),'weight',coalesce(r.weight,1),'residency',coalesce(r.residency,'unspecified'),'operator_disabled',coalesce(r.operator_disabled,false)),'health',jsonb_build_object('consecutive_failures',coalesce(h.consecutive_failures,0),'open_until',h.open_until,'last_observed_at',h.last_observed_at)) FROM deployments d LEFT JOIN deployment_routing r ON r.deployment_id=d.id LEFT JOIN deployment_route_health h ON h.deployment_id=d.id WHERE d.id=$1")
        .bind(deployment).fetch_optional(&mut *tx).await?.ok_or_else(missing)?;
    tx.commit().await?;
    Ok(Json(value))
}
async fn put_deployment_routing(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(deployment): Path<Uuid>,
    Json(p): Json<DeploymentRouting>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, true).await?;
    if !(1..=1000).contains(&p.weight) || !residency_valid(&p.residency) {
        return Err(invalid());
    }
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM deployments WHERE id=$1 FOR SHARE")
        .bind(deployment)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    sqlx::query("INSERT INTO deployment_routing(deployment_id,priority,weight,residency,operator_disabled) VALUES($1,$2,$3,$4,$5) ON CONFLICT(deployment_id) DO UPDATE SET priority=excluded.priority,weight=excluded.weight,residency=excluded.residency,operator_disabled=excluded.operator_disabled")
        .bind(deployment).bind(p.priority).bind(p.weight).bind(p.residency).bind(p.operator_disabled).execute(&mut *tx).await?;
    audit(
        &mut tx,
        u.user_id,
        None,
        None,
        "governance.deployment_routing_updated",
        Some(deployment),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}

async fn cost_summary(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    let value: Value = sqlx::query_scalar("SELECT jsonb_build_object('currency','USD','known_cost_microusd',coalesce(sum(r.actual_microusd),0)::text,'held_microusd',coalesce(sum(r.held_microusd) FILTER(WHERE r.state IN ('pending','unknown')),0)::text,'unknown_cost_requests',count(*) FILTER(WHERE r.actual_microusd IS NULL),'requests',count(*)) FROM inference_executions e JOIN api_keys k ON k.organization_id=e.organization_id AND k.workspace_id=e.workspace_id AND k.id=e.api_key_id LEFT JOIN governance_reservations r ON r.organization_id=e.organization_id AND r.execution_id=e.id WHERE e.organization_id=$1 AND e.workspace_id=$2 AND ($3 OR k.issued_to_user_id=$4) AND e.started_at>=date_trunc('month',now(),'UTC') AND e.started_at<((date_trunc('month',now() AT TIME ZONE 'UTC')+interval '1 month') AT TIME ZONE 'UTC')")
        .bind(a.org).bind(ws).bind(admin(&a.role)).bind(u.user_id).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(value))
}
async fn cost_rows(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    a: &WorkspaceAccess,
    ws: Uuid,
    limit: i64,
    offset: i64,
) -> Result<Vec<Value>, ApiError> {
    Ok(sqlx::query_scalar("SELECT jsonb_build_object('id',e.id,'public_model',e.public_model,'provider',e.provider,'state',e.state,'started_at',e.started_at,'input_tokens',e.input_tokens,'output_tokens',e.output_tokens,'price_id',r.price_id,'cost_microusd',r.actual_microusd::text,'reserved_microusd',r.held_microusd::text,'cost_status',coalesce(r.state,'unknown'),'details_redacted_at',e.details_redacted_at) FROM inference_executions e JOIN api_keys k ON k.organization_id=e.organization_id AND k.workspace_id=e.workspace_id AND k.id=e.api_key_id LEFT JOIN governance_reservations r ON r.organization_id=e.organization_id AND r.execution_id=e.id WHERE e.organization_id=$1 AND e.workspace_id=$2 AND ($3 OR k.issued_to_user_id=$4) ORDER BY e.started_at DESC,e.id LIMIT $5 OFFSET $6")
        .bind(a.org).bind(ws).bind(admin(&a.role)).bind(u.user_id).bind(limit).bind(offset).fetch_all(&mut **tx).await?)
}
async fn costs(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(page): Query<Page>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    let (limit, offset) = page.bounds()?;
    let data = cost_rows(&mut tx, &u, &a, ws, limit, offset).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
fn csv_cell(value: &str) -> String {
    // Quote every field. Prefix formula-looking cells, including leading whitespace/control.
    let trimmed = value.trim_start_matches(|c: char| c.is_whitespace() || c.is_control());
    let formula =
        trimmed.starts_with(['=', '+', '-', '@']) || value.starts_with(['\t', '\r', '\n']);
    format!(
        "\"{}{}\"",
        if formula { "'" } else { "" },
        value.replace('"', "\"\"")
    )
}
async fn usage_export(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    let limit = page.limit.unwrap_or(1000);
    let offset = page.offset.unwrap_or(0);
    if !(1..=1000).contains(&limit) || !(0..=100000).contains(&offset) {
        return Err(invalid());
    }
    let data = cost_rows(&mut tx, &u, &a, ws, limit, offset).await?;
    tx.commit().await?;
    let fields = [
        "id",
        "public_model",
        "provider",
        "state",
        "started_at",
        "input_tokens",
        "output_tokens",
        "price_id",
        "cost_microusd",
        "reserved_microusd",
        "cost_status",
    ];
    let mut csv = format!("{}\r\n", fields.join(","));
    for row in &data {
        let cells: Vec<String> = fields
            .iter()
            .map(|field| {
                let v = &row[*field];
                csv_cell(&match v {
                    Value::Null => String::new(),
                    Value::String(v) => v.clone(),
                    _ => v.to_string(),
                })
            })
            .collect();
        csv.push_str(&cells.join(","));
        csv.push_str("\r\n");
    }
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"usage.csv\"".to_owned(),
            ),
            (header::CACHE_CONTROL, "no-store".to_owned()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
            (
                header::HeaderName::from_static("x-export-limit"),
                limit.to_string(),
            ),
            (
                header::HeaderName::from_static("x-export-offset"),
                offset.to_string(),
            ),
            (
                header::HeaderName::from_static("x-export-rows"),
                data.len().to_string(),
            ),
        ],
        csv,
    )
        .into_response())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reconcile {
    input_tokens: i64,
    output_tokens: i64,
    evidence: String,
}
async fn reconcile(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, execution)): Path<(Uuid, Uuid)>,
    Json(p): Json<Reconcile>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::operator_lock(&mut tx, &u).await?;
    if !(0..=i64::from(i32::MAX)).contains(&p.input_tokens)
        || !(0..=i64::from(i32::MAX)).contains(&p.output_tokens)
        || p.evidence.trim().is_empty()
        || p.evidence.chars().count() > 200
        || p.evidence.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM inference_executions WHERE organization_id=$1 AND workspace_id=$2 AND id=$3")
        .bind(a.org).bind(ws).bind(execution).fetch_optional(&mut *tx).await?.ok_or_else(missing)?;
    tx.commit().await?;
    // Core owns the organization lock and atomic reconciliation ledger/audit transaction.
    crate::governance::resolve_usage(
        &s,
        a.org,
        execution,
        Usage {
            input_tokens: Some(p.input_tokens as u64),
            output_tokens: Some(p.output_tokens as u64),
        },
        &p.evidence,
        u.user_id,
    )
    .await
    .map_err(|error| match error {
        InferenceError::InvalidRequest => invalid(),
        InferenceError::Configuration => ApiError(
            StatusCode::CONFLICT,
            "Execution has no reconcilable pinned price",
        ),
        _ => ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Reconciliation unavailable",
        ),
    })?;
    Ok(ok())
}
