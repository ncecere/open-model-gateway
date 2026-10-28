//! #48 browser-only, append-only provider-reported cost reconciliation.
use super::*;
use axum::extract::rejection::JsonRejection;
use chrono::{DateTime, Utc};
use serde::Serialize;
mod matching;
pub(super) mod parser;
use parser::{Upload, bounded};

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route("/api/v1/platform/provider-cost-imports", get(list_imports))
        .route(
            "/api/v1/platform/provider-cost-imports/{id}",
            get(get_import),
        )
        .route(
            "/api/v1/platform/provider-cost-imports/{id}/lines",
            get(lines),
        )
        .route(
            "/api/v1/platform/provider-cost-mappings",
            get(list_mappings).post(create_mapping),
        )
        .route(
            "/api/v1/platform/provider-cost-mappings/{id}",
            get(get_mapping),
        )
        .route(
            "/api/v1/platform/provider-reconciliations",
            get(list_reconciliations).post(reconcile),
        )
        .route(
            "/api/v1/platform/provider-reconciliations/{id}",
            get(get_reconciliation),
        )
}

pub(super) fn upload_routes() -> Router<Store> {
    Router::new()
        .route("/api/v1/platform/provider-cost-imports", post(import))
        .layer(axum::extract::DefaultBodyLimit::max(parser::MAX_BYTES))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            parser::MAX_BYTES,
        ))
}

// Do not format the rejection or call IntoResponse: Axum's default response
// and rejection trace can include uploaded field names and values.
fn json_body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    body.map(|Json(value)| value).map_err(|rejection| {
        let status = rejection.status();
        let message = match status {
            StatusCode::PAYLOAD_TOO_LARGE => "Provider cost request body exceeds the size limit",
            StatusCode::UNSUPPORTED_MEDIA_TYPE => "Provider cost request requires application/json",
            StatusCode::UNPROCESSABLE_ENTITY => "Invalid provider cost request fields",
            _ => "Invalid provider cost JSON body",
        };
        ApiError(status, message)
    })
}

fn conflict() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "Overlapping coverage or invalid supersession; select the current exact revision",
    )
}
// Catalog shared first, then management-only installation and live User scope.
// No admission counters, budgets or rate-limit rows are touched.
async fn write_tx<'a>(
    s: &'a Store,
    u: &BrowserPrincipal,
) -> Result<Transaction<'a, Postgres>, ApiError> {
    let mut tx = resources::installation_tx(s).await?;
    locks::shared(&mut tx, [locks::Scope::User(u.user_id)]).await?;
    resources::platform_write(&mut tx, u).await?;
    sqlx::query("SET LOCAL statement_timeout='10s'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}
async fn account_lock(tx: &mut Transaction<'_, Postgres>, account: &str) -> Result<(), ApiError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('provider-cost:'||$1,72419548))")
        .bind(account)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn import(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    body: Result<Json<Upload>, JsonRejection>,
) -> ApiResult {
    let b = json_body(body)?;
    let mut tx = write_tx(&s, &u).await?;
    let parsed = parser::parse(&b)?;
    let evidence = serde_json::to_string(&b).map_err(|_| invalid())?;
    if evidence.len() > parser::MAX_BYTES {
        return Err(invalid());
    }
    account_lock(&mut tx, &b.account_id).await?;
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM provider_cost_imports WHERE account_id=$1 AND fingerprint=$2",
    )
    .bind(&b.account_id)
    .bind(&parsed.fingerprint)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(id) = existing {
        tx.commit().await?;
        return Ok(identifier(id));
    }
    let overlaps: Vec<(Uuid, DateTime<Utc>, DateTime<Utc>)> = sqlx::query_as("SELECT id,period_start,period_end FROM provider_cost_imports i WHERE account_id=$1 AND period_start<$3 AND period_end>$2 AND NOT EXISTS(SELECT FROM provider_cost_imports c WHERE c.supersedes=i.id)")
        .bind(&b.account_id).bind(parsed.start).bind(parsed.end).fetch_all(&mut *tx).await?;
    match (b.supersedes, overlaps.as_slice()) {
        (None, []) => {}
        (Some(previous), [(id, start, end)])
            if previous == *id && parsed.start == *start && parsed.end == *end => {}
        _ => return Err(conflict()),
    }
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO provider_cost_imports(id,account_id,period_start,period_end,exported_at,actor_user_id,evidence_reference,evidence_json,fingerprint,supersedes,line_count) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
        .bind(id).bind(&b.account_id).bind(parsed.start).bind(parsed.end).bind(parsed.exported).bind(u.user_id)
        .bind(&b.evidence_reference).bind(evidence).bind(parsed.fingerprint).bind(b.supersedes).bind(parsed.lines.len() as i32)
        .execute(&mut *tx).await?;
    // Chunked inserts keep 50k-line uploads bounded in round trips/parameters.
    for (chunk_index, chunk) in parsed.lines.chunks(1000).enumerate() {
        let rows: Vec<Value> = chunk
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let mut v = serde_json::to_value(l).map_err(|_| invalid())?;
                v["line_number"] = json!(chunk_index * 1000 + i + 1);
                Ok::<_, ApiError>(v)
            })
            .collect::<Result<_, _>>()?;
        sqlx::query("INSERT INTO provider_cost_lines(import_id,line_number,source_page,source_pointer,bucket_start,bucket_end,dimensions,original_amount,amount_microusd,conversion_residual_microusd,dimension_key) SELECT $1,r.line_number,r.source_page,r.source_pointer,r.bucket_start,r.bucket_end,r.dimensions,r.original_amount,r.amount_microusd::numeric,r.conversion_residual_microusd,r.dimension_key FROM jsonb_to_recordset($2) AS r(line_number integer,source_page integer,source_pointer text,bucket_start timestamptz,bucket_end timestamptz,dimensions jsonb,original_amount text,amount_microusd text,conversion_residual_microusd text,dimension_key text)")
            .bind(id).bind(json!(rows)).execute(&mut *tx).await?;
    }
    audit(
        &mut tx,
        &u,
        None,
        "provider_cost.imported",
        "provider_cost_import",
        Some(id),
        json!({"line_count":parsed.lines.len()}),
    )
    .await?;
    tx.commit().await?;
    Ok(identifier(id))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Mapping {
    account_id: String,
    supersedes: Option<Uuid>,
    evidence_reference: String,
    bindings: Vec<Binding>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Binding {
    provider_workspace_id: Option<String>,
    connection_id: Uuid,
    deployment_id: Option<Uuid>,
    workspace_id: Option<Uuid>,
    effective_start: DateTime<Utc>,
    effective_end: DateTime<Utc>,
    exclusive: bool,
    non_omg_traffic: bool,
    attested: bool,
    evidence_reference: String,
    model_mappings: Vec<ModelMapping>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModelMapping {
    provider_model: String,
    upstream_model: String,
}

async fn create_mapping(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    body: Result<Json<Mapping>, JsonRejection>,
) -> ApiResult {
    let b = json_body(body)?;
    let mut tx = write_tx(&s, &u).await?;
    if !bounded(&b.account_id, 200)
        || !bounded(&b.evidence_reference, 256)
        || b.bindings.len() > 1000
    {
        return Err(invalid());
    }
    for (index, binding) in b.bindings.iter().enumerate() {
        if !binding.attested
            || !bounded(&binding.evidence_reference, 256)
            || binding
                .provider_workspace_id
                .as_ref()
                .is_some_and(|s| !bounded(s, 4096))
            || binding.effective_end <= binding.effective_start
            || binding.model_mappings.len() > 100
            || (binding.exclusive && (binding.non_omg_traffic || binding.workspace_id.is_none()))
        {
            return Err(invalid());
        }
        let mut names = std::collections::BTreeSet::new();
        for m in &binding.model_mappings {
            if !bounded(&m.provider_model, 512)
                || !bounded(&m.upstream_model, 512)
                || !names.insert(&m.provider_model)
            {
                return Err(invalid());
            }
        }
        // One effective binding per provider workspace. Also refuse overlapping
        // reverse ownership of a connection/deployment: never double-count an
        // estimate under two provider workspaces or pretend it proves ownership.
        for previous in &b.bindings[..index] {
            if binding.effective_start < previous.effective_end
                && binding.effective_end > previous.effective_start
                && (binding.provider_workspace_id == previous.provider_workspace_id
                    || (binding.connection_id == previous.connection_id
                        && (binding.deployment_id.is_none()
                            || previous.deployment_id.is_none()
                            || binding.deployment_id == previous.deployment_id)))
            {
                return Err(invalid());
            }
        }
        let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT FROM provider_connections c WHERE c.id=$1 AND c.provider='anthropic' AND ($2::uuid IS NULL OR EXISTS(SELECT FROM deployments d WHERE d.id=$2 AND d.provider_connection_id=c.id))) AND ($3::uuid IS NULL OR EXISTS(SELECT FROM workspaces WHERE id=$3))")
            .bind(binding.connection_id).bind(binding.deployment_id).bind(binding.workspace_id).fetch_one(&mut *tx).await?;
        if !valid {
            return Err(invalid());
        }
    }
    account_lock(&mut tx, &b.account_id).await?;
    let latest: Option<Uuid> = sqlx::query_scalar("SELECT id FROM provider_cost_mappings m WHERE account_id=$1 AND NOT EXISTS(SELECT FROM provider_cost_mappings c WHERE c.supersedes=m.id)")
        .bind(&b.account_id).fetch_optional(&mut *tx).await?;
    if latest != b.supersedes {
        return Err(conflict());
    }
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO provider_cost_mappings(id,account_id,supersedes,evidence_reference,actor_user_id,binding_count) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(id).bind(&b.account_id).bind(b.supersedes).bind(&b.evidence_reference).bind(u.user_id).bind(b.bindings.len() as i32).execute(&mut *tx).await?;
    for (index, binding) in b.bindings.iter().enumerate() {
        sqlx::query("INSERT INTO provider_cost_bindings(mapping_id,binding_number,provider_workspace_id,connection_id,deployment_id,workspace_id,effective_start,effective_end,exclusive,non_omg_traffic,attested,evidence_reference,model_mappings) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)")
            .bind(id).bind(index as i32+1).bind(&binding.provider_workspace_id).bind(binding.connection_id).bind(binding.deployment_id).bind(binding.workspace_id)
            .bind(binding.effective_start).bind(binding.effective_end).bind(binding.exclusive).bind(binding.non_omg_traffic).bind(binding.attested)
            .bind(&binding.evidence_reference).bind(json!(binding.model_mappings)).execute(&mut *tx).await?;
    }
    audit(
        &mut tx,
        &u,
        None,
        "provider_cost.mapped",
        "provider_cost_mapping",
        Some(id),
        json!({"bindings":b.bindings.len()}),
    )
    .await?;
    tx.commit().await?;
    Ok(identifier(id))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reconcile {
    import_id: Uuid,
    mapping_id: Uuid,
}
#[cfg(test)]
#[test]
fn management_openapi_money_provider_request_vectors() {
    let mapping = json!({"account_id":"account","supersedes":null,"evidence_reference":"receipt:map","bindings":[{"provider_workspace_id":"provider-workspace","connection_id":Uuid::nil(),"deployment_id":Uuid::nil(),"workspace_id":Uuid::nil(),"effective_start":"2025-08-01T00:00:00Z","effective_end":"2025-08-02T00:00:00Z","exclusive":true,"non_omg_traffic":false,"attested":true,"evidence_reference":"receipt:binding","model_mappings":[{"provider_model":"provider-model","upstream_model":"upstream-model"}]}]});
    super::openapi::requests::component_vectors::<Binding>(
        "MoneyProviderBinding",
        mapping["bindings"][0].clone(),
    );
    super::openapi::requests::component_vectors::<ModelMapping>(
        "MoneyProviderModelMapping",
        mapping["bindings"][0]["model_mappings"][0].clone(),
    );
    super::openapi::requests::vectors::<Mapping>(
        "POST",
        "/api/v1/platform/provider-cost-mappings",
        mapping,
    );
    super::openapi::requests::vectors::<Reconcile>(
        "POST",
        "/api/v1/platform/provider-reconciliations",
        json!({"import_id":Uuid::nil(),"mapping_id":Uuid::nil()}),
    );
}
async fn reconcile(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    body: Result<Json<Reconcile>, JsonRejection>,
) -> ApiResult {
    let b = json_body(body)?;
    let mut tx = write_tx(&s, &u).await?;
    let (account, start, end): (String, DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "SELECT account_id,period_start,period_end FROM provider_cost_imports WHERE id=$1",
    )
    .bind(b.import_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(missing)?;
    let mapping_account: String =
        sqlx::query_scalar("SELECT account_id FROM provider_cost_mappings WHERE id=$1")
            .bind(b.mapping_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(missing)?;
    if account != mapping_account {
        return Err(invalid());
    }
    let binding_json: Vec<Value> = sqlx::query_scalar("SELECT to_jsonb(b)-'mapping_id'-'binding_number' FROM provider_cost_bindings b WHERE mapping_id=$1 ORDER BY binding_number")
        .bind(b.mapping_id).fetch_all(&mut *tx).await?;
    let bindings: Vec<Binding> = binding_json
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()
        .map_err(|_| invalid())?;
    let snapshot =
        matching::snapshot(&mut tx, b.import_id, b.mapping_id, start, end, &bindings).await?;
    let observed = parser::timestamp(snapshot["observed_at"].as_str().ok_or_else(invalid)?)?;
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO provider_reconciliations(id,account_id,import_id,mapping_id,actor_user_id,observed_at,snapshot) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(id).bind(account).bind(b.import_id).bind(b.mapping_id).bind(u.user_id).bind(observed).bind(snapshot).execute(&mut *tx).await?;
    audit(
        &mut tx,
        &u,
        None,
        "provider_cost.reconciled",
        "provider_reconciliation",
        Some(id),
        json!({"import_id":b.import_id,"mapping_id":b.mapping_id}),
    )
    .await?;
    tx.commit().await?;
    Ok(identifier(id))
}

async fn list_records(s: Store, u: BrowserPrincipal, p: Page, table: &str) -> ApiResult {
    let (limit, offset) = p.bounds()?;
    let mut tx = resources::platform_read_tx(&s, &u).await?;
    let sql = format!(
        "SELECT coalesce(jsonb_agg(v ORDER BY v->>'id'),'[]') FROM (SELECT to_jsonb(t)-'evidence_json'-'fingerprint'-'snapshot' AS v FROM {table} t ORDER BY id LIMIT $1 OFFSET $2) q"
    );
    let data: Value = sqlx::query_scalar(&sql)
        .bind(limit)
        .bind(offset)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data,"limit":limit,"offset":offset})))
}
async fn get_record(s: Store, u: BrowserPrincipal, id: Uuid, table: &str) -> ApiResult {
    let mut tx = resources::platform_read_tx(&s, &u).await?;
    let sql = format!("SELECT to_jsonb(t)-'fingerprint' FROM {table} t WHERE id=$1");
    let mut data: Value = sqlx::query_scalar(&sql)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    if table == "provider_cost_mappings" {
        data["bindings"]=sqlx::query_scalar("SELECT coalesce(jsonb_agg(to_jsonb(b)-'mapping_id' ORDER BY binding_number),'[]') FROM provider_cost_bindings b WHERE mapping_id=$1")
            .bind(id).fetch_one(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Json(data))
}
async fn list_imports(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<Page>,
) -> ApiResult {
    list_records(s, u, p, "provider_cost_imports").await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DetailQuery {}
async fn get_import(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Query(_): Query<DetailQuery>,
) -> ApiResult {
    get_record(s, u, id, "provider_cost_imports").await
}
async fn list_mappings(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<Page>,
) -> ApiResult {
    list_records(s, u, p, "provider_cost_mappings").await
}
async fn get_mapping(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Query(_): Query<DetailQuery>,
) -> ApiResult {
    get_record(s, u, id, "provider_cost_mappings").await
}
async fn list_reconciliations(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(p): Query<Page>,
) -> ApiResult {
    list_records(s, u, p, "provider_reconciliations").await
}
async fn get_reconciliation(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Query(_): Query<DetailQuery>,
) -> ApiResult {
    get_record(s, u, id, "provider_reconciliations").await
}
async fn lines(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Query(p): Query<Page>,
) -> ApiResult {
    let (limit, offset) = p.bounds()?;
    let mut tx = resources::platform_read_tx(&s, &u).await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT FROM provider_cost_imports WHERE id=$1)")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if !exists {
        return Err(missing());
    }
    let data: Value=sqlx::query_scalar("SELECT coalesce(jsonb_agg(to_jsonb(q) ORDER BY line_number),'[]') FROM (SELECT line_number,source_page,source_pointer,bucket_start,bucket_end,dimensions,original_amount,amount_microusd::text,conversion_residual_microusd FROM provider_cost_lines WHERE import_id=$1 ORDER BY line_number LIMIT $2 OFFSET $3) q")
        .bind(id).bind(limit).bind(offset).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data,"limit":limit,"offset":offset})))
}
