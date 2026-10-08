use super::*;
use crate::auth::NewApiKey;

/// Display status: revoked (permanent) > expired > disabled (reversible) > active.
pub(super) const KEY_STATUS: &str = "CASE WHEN k.revoked_at IS NOT NULL THEN 'revoked' WHEN k.expires_at IS NOT NULL AND k.expires_at<=now() THEN 'expired' WHEN k.disabled_at IS NOT NULL THEN 'disabled' ELSE 'active' END";
/// Latest admitted attempt of this credential (not the whole lineage).
pub(super) const LAST_USED: &str =
    "(SELECT max(e.started_at) FROM inference_executions e WHERE e.api_key_id=k.id)";
fn key_json() -> String {
    format!(
        "jsonb_build_object('id',k.id,'name',k.name,'issued_to_user_id',k.issued_to_user_id,'service_account_id',k.service_account_id,'created_at',k.created_at,'expires_at',k.expires_at,'revoked_at',k.revoked_at,'disabled_at',k.disabled_at,'status',{KEY_STATUS},'last_used_at',{LAST_USED},'lineage_id',k.governance_key_id,'model_ids',CASE WHEN r.governance_key_id IS NULL THEN NULL ELSE ARRAY(SELECT s.model_id FROM key_model_selections s WHERE s.workspace_id=k.workspace_id AND s.governance_key_id=k.governance_key_id ORDER BY s.model_id) END)"
    )
}
/// Adds `usage` (the lineage's primary key budget and current use) to key rows.
pub(super) async fn with_usage(
    tx: &mut Transaction<'_, Postgres>,
    rows: &mut [Value],
    workspace_field: Option<&str>,
    ws: Uuid,
) -> Result<(), ApiError> {
    for row in rows.iter_mut() {
        let lineage = row["lineage_id"]
            .as_str()
            .and_then(|v| Uuid::parse_str(v).ok())
            .ok_or_else(invalid)?;
        let ws = match workspace_field {
            Some(f) => row[f]["id"]
                .as_str()
                .and_then(|v| Uuid::parse_str(v).ok())
                .ok_or_else(invalid)?,
            None => ws,
        };
        row["usage"] = governance::key_usage(tx, ws, lineage).await?;
    }
    Ok(())
}
pub(super) async fn keys(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<Page>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::detail_access(&a)?;
    let (l, o) = p.bounds()?;
    let mut data:Vec<Value>=sqlx::query_scalar(&format!("SELECT {} FROM api_keys k LEFT JOIN key_model_restrictions r ON r.workspace_id=k.workspace_id AND r.governance_key_id=k.governance_key_id WHERE k.workspace_id=$1 AND ($2 OR k.issued_to_user_id=$3) ORDER BY k.created_at DESC,k.id LIMIT $4 OFFSET $5",key_json())).bind(ws).bind(a.admin).bind(u.user_id).bind(l).bind(o).fetch_all(&mut *tx).await?;
    with_usage(&mut tx, &mut data, None, ws).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
/// One key row with the list's visibility: members their own human keys,
/// shared administrators (personal owner) every key; otherwise 404.
pub(super) async fn key(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::detail_access(&a)?;
    let row: Value = sqlx::query_scalar(&format!("SELECT {} FROM api_keys k LEFT JOIN key_model_restrictions r ON r.workspace_id=k.workspace_id AND r.governance_key_id=k.governance_key_id WHERE k.workspace_id=$1 AND k.id=$4 AND ($2 OR k.issued_to_user_id=$3)",key_json())).bind(ws).bind(a.admin).bind(u.user_id).bind(id).fetch_optional(&mut *tx).await?.ok_or_else(missing)?;
    let mut rows = [row];
    with_usage(&mut tx, &mut rows, None, ws).await?;
    tx.commit().await?;
    let [row] = rows;
    Ok(Json(row))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct KeyUpdate {
    disabled: bool,
}
/// Reversible disable/enable. Own human keys, or any key for shared
/// administrators (personal owner: own keys). Revoked keys never re-enable.
pub(super) async fn update_key(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, Uuid)>,
    Json(b): Json<KeyUpdate>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::detail_access(&a)?;
    let (revoked, issued): (bool, Option<Uuid>) = sqlx::query_as("SELECT revoked_at IS NOT NULL,issued_to_user_id FROM api_keys WHERE workspace_id=$1 AND id=$2 AND ($3 OR issued_to_user_id=$4) FOR UPDATE").bind(ws).bind(id).bind(a.admin).bind(u.user_id).fetch_optional(&mut *tx).await?.ok_or_else(missing)?;
    if !(a.admin || (a.member && issued == Some(u.user_id))) {
        return Err(denied());
    }
    if revoked {
        return Err(key_revoked());
    }
    sqlx::query("UPDATE api_keys SET disabled_at=CASE WHEN $3 THEN coalesce(disabled_at,now()) ELSE NULL END WHERE workspace_id=$1 AND id=$2").bind(ws).bind(id).bind(b.disabled).execute(&mut *tx).await?;
    audit(
        &mut tx,
        &u,
        Some(ws),
        if b.disabled {
            "key.disabled"
        } else {
            "key.enabled"
        },
        "key",
        Some(id),
        json!({"disabled":b.disabled}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
/// Usage totals of one key lineage between two instants (started_at), as a JSON object.
const KEY_TOTALS: &str = "jsonb_build_object('spend_microusd',coalesce(sum(r.actual_microusd),0)::text,'held_microusd',coalesce(sum(r.held_microusd) FILTER(WHERE r.actual_microusd IS NULL AND r.state IN('pending','unknown')),0)::text,'requests',count(DISTINCT e.root_request_id)::text,'attempts',count(e.id)::text,'unresolved_attempts',count(e.id) FILTER(WHERE r.actual_microusd IS NULL)::text)";
/// Key detail: 30-day daily series, today/week/month totals and every
/// applicable budget window. Key lineage scope (rotations continue the series).
pub(super) async fn key_stats(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::detail_access(&a)?;
    let (lineage, status, last_used): (Uuid, String, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(&format!("SELECT k.governance_key_id,{KEY_STATUS},{LAST_USED} FROM api_keys k WHERE k.workspace_id=$1 AND k.id=$2 AND ($3 OR k.issued_to_user_id=$4)")).bind(ws).bind(id).bind(a.admin).bind(u.user_id).fetch_optional(&mut *tx).await?.ok_or_else(missing)?;
    let now: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let scope = "FROM inference_executions e LEFT JOIN governance_reservations r ON r.execution_id=e.id WHERE e.workspace_id=$1 AND e.api_key_id IN(SELECT id FROM api_keys WHERE workspace_id=$1 AND governance_key_id=$2)";
    let daily: Value = sqlx::query_scalar(&format!("SELECT coalesce(jsonb_agg(jsonb_build_object('date',d::date::text,'spend_microusd',t->>'spend_microusd','held_microusd',t->>'held_microusd','requests',t->>'requests','unresolved_attempts',t->>'unresolved_attempts') ORDER BY d),'[]'::jsonb) FROM generate_series(($3::timestamptz AT TIME ZONE 'UTC')::date-29,($3::timestamptz AT TIME ZONE 'UTC')::date,interval '1 day') d CROSS JOIN LATERAL (SELECT {KEY_TOTALS} t {scope} AND e.started_at>=(d::date::timestamp AT TIME ZONE 'UTC') AND e.started_at<((d::date+1)::timestamp AT TIME ZONE 'UTC')) x")).bind(ws).bind(lineage).bind(now).fetch_one(&mut *tx).await?;
    let mut totals = serde_json::Map::new();
    for (name, period) in [
        ("today", crate::governance::BudgetPeriod::Day),
        ("week", crate::governance::BudgetPeriod::Week),
        ("month", crate::governance::BudgetPeriod::Month),
    ] {
        let (start, end) = period.window(now);
        let v: Value = sqlx::query_scalar(&format!(
            "SELECT {KEY_TOTALS} {scope} AND e.started_at>=$3 AND e.started_at<$4"
        ))
        .bind(ws)
        .bind(lineage)
        .bind(start)
        .bind(end)
        .fetch_one(&mut *tx)
        .await?;
        totals.insert(name.into(), v);
    }
    let budgets =
        governance::lineage_budget_windows(&mut tx, ws, lineage, a.view_all_activity).await?;
    tx.commit().await?;
    Ok(Json(
        json!({"key_id":id,"lineage_id":lineage,"status":status,"last_used_at":last_used,"daily":daily,"totals":totals,"budgets":budgets}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NewKey {
    name: String,
    expires_in_days: i32,
    service_account_id: Option<Uuid>,
    model_ids: Option<Vec<Uuid>>,
    /// Optional initial key-lineage policy (absent/null fields inherit);
    /// validated like the key policy PUT and stored atomically with the key.
    requests_per_minute: Option<i64>,
    tokens_per_minute: Option<i64>,
    concurrent_requests: Option<i64>,
    budgets: Option<Vec<governance::BudgetInput>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Expiry {
    expires_in_days: i32,
}
fn expiry(days: i32) -> Result<(), ApiError> {
    if !(1..=365).contains(&days) {
        return Err(invalid());
    }
    Ok(())
}
async fn insert_key(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    user: Option<Uuid>,
    service: Option<Uuid>,
    name: &str,
    days: i32,
    lineage: Option<Uuid>,
) -> Result<NewApiKey, ApiError> {
    let key = NewApiKey::generate();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,service_account_id,name,secret_hash,expires_at,governance_key_id) VALUES($1,$2,$3,$4,$5,$6,now()+make_interval(days=>$7),$8)").bind(key.id).bind(ws).bind(user).bind(service).bind(name).bind(key.digest.as_slice()).bind(days).bind(lineage.unwrap_or(key.id)).execute(&mut **tx).await?;
    Ok(key)
}
pub(super) async fn create_key(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(mut b): Json<NewKey>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::detail_access(&a)?;
    expiry(b.expires_in_days)?;
    if !valid_name(&b.name) {
        return Err(invalid());
    }
    if let Some(ids) = &mut b.model_ids {
        if ids.len() > 200 {
            return Err(invalid());
        }
        ids.sort_unstable();
        ids.dedup();
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM models WHERE id=ANY($1) AND workspace_model_allowed($2,id)",
        )
        .bind(&*ids)
        .bind(ws)
        .fetch_one(&mut *tx)
        .await?;
        if n != ids.len() as i64 {
            return Err(invalid());
        }
    }
    if let Some(id) = b.service_account_id {
        resources::manage(&a)?;
        if !shared(&a.kind) {
            return Err(denied());
        }
        sqlx::query_scalar::<_,Uuid>("SELECT id FROM service_accounts WHERE workspace_id=$1 AND id=$2 AND disabled_at IS NULL FOR UPDATE").bind(ws).bind(id).fetch_optional(&mut *tx).await?.ok_or_else(missing)?;
    } else if !a.member {
        return Err(denied());
    } else {
        // Human keys follow Admin > Settings > General's maximum lifetime.
        super::settings::check_human_key_lifetime(&mut tx, b.expires_in_days).await?;
    }
    let limits = governance::initial_key_limits(
        [
            b.requests_per_minute,
            b.tokens_per_minute,
            b.concurrent_requests,
        ],
        b.budgets.as_deref(),
    )?;
    if let Some(l) = &limits {
        governance::check_initial_key_limits(&mut tx, ws, l).await?;
    }
    let key = insert_key(
        &mut tx,
        ws,
        if b.service_account_id.is_none() {
            Some(u.user_id)
        } else {
            None
        },
        b.service_account_id,
        &b.name,
        b.expires_in_days,
        None,
    )
    .await?;
    if let Some(ids) = &b.model_ids {
        sqlx::query(
            "INSERT INTO key_model_restrictions(workspace_id,governance_key_id) VALUES($1,$2)",
        )
        .bind(ws)
        .bind(key.id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO key_model_selections(workspace_id,governance_key_id,model_id) SELECT $1,$2,unnest($3::uuid[])").bind(ws).bind(key.id).bind(ids).execute(&mut *tx).await?;
    }
    if let Some(l) = &limits {
        governance::store_initial_key_limits(&mut tx, &u, ws, key.id, l).await?;
    }
    audit(
        &mut tx,
        &u,
        Some(ws),
        "key.created",
        "key",
        Some(key.id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    let policy = limits.as_ref().map(governance::json_limits);
    Ok(Json(
        json!({"id":key.id,"token":key.token,"model_ids":b.model_ids,"policy":policy}),
    ))
}
pub(super) async fn revoke_key(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::detail_access(&a)?;
    if sqlx::query("UPDATE api_keys SET revoked_at=coalesce(revoked_at,now()) WHERE workspace_id=$1 AND id=$2 AND ($3 OR issued_to_user_id=$4)").bind(ws).bind(id).bind(a.admin).bind(u.user_id).execute(&mut *tx).await?.rows_affected()!=1{return Err(missing())}
    audit(
        &mut tx,
        &u,
        Some(ws),
        "key.revoked",
        "key",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
pub(super) async fn rotate_key(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, Uuid)>,
    Json(b): Json<Expiry>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::detail_access(&a)?;
    expiry(b.expires_in_days)?;
    let(name,issued,service,lineage,disabled):(String,Option<Uuid>,Option<Uuid>,Uuid,bool)=sqlx::query_as("SELECT name,issued_to_user_id,service_account_id,governance_key_id,disabled_at IS NOT NULL FROM api_keys WHERE workspace_id=$1 AND id=$2 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at>now()) FOR UPDATE").bind(ws).bind(id).fetch_optional(&mut *tx).await?.ok_or_else(missing)?;
    if disabled {
        return Err(key_disabled());
    }
    // Shared inherited authority can revoke human keys, never mint credentials as another person.
    if let Some(service) = service {
        resources::manage(&a)?;
        sqlx::query_scalar::<_,Uuid>("SELECT id FROM service_accounts WHERE workspace_id=$1 AND id=$2 AND disabled_at IS NULL FOR UPDATE").bind(ws).bind(service).fetch_optional(&mut *tx).await?.ok_or_else(denied)?;
    } else if issued != Some(u.user_id) || !a.member {
        return Err(denied());
    } else {
        super::settings::check_human_key_lifetime(&mut tx, b.expires_in_days).await?;
    }
    let key = insert_key(
        &mut tx,
        ws,
        issued,
        service,
        &name,
        b.expires_in_days,
        Some(lineage),
    )
    .await?;
    sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &u,
        Some(ws),
        "key.rotated",
        "key",
        Some(key.id),
        json!({"rotation":true}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"id":key.id,"token":key.token})))
}
pub(super) async fn accounts(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(p): Query<Page>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::manage(&a)?;
    if !shared(&a.kind) {
        return Err(invalid());
    }
    let (l, o) = p.bounds()?;
    let data:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'name',name,'disabled_at',disabled_at,'created_at',created_at) FROM service_accounts WHERE workspace_id=$1 ORDER BY name,id LIMIT $2 OFFSET $3").bind(ws).bind(l).bind(o).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
pub(super) async fn create_account(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(b): Json<Name>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::manage(&a)?;
    if !shared(&a.kind) || !valid_name(&b.name) {
        return Err(invalid());
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO service_accounts(id,workspace_id,name) VALUES($1,$2,$3)")
        .bind(id)
        .bind(ws)
        .bind(b.name)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &u,
        Some(ws),
        "service_account.created",
        "service_account",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(identifier(id))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Disabled {
    disabled: bool,
}
pub(super) async fn update_account(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, Uuid)>,
    Json(b): Json<Disabled>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::manage(&a)?;
    if !shared(&a.kind) {
        return Err(invalid());
    }
    if sqlx::query("UPDATE service_accounts SET disabled_at=CASE WHEN $3 THEN coalesce(disabled_at,now()) ELSE NULL END WHERE workspace_id=$1 AND id=$2").bind(ws).bind(id).bind(b.disabled).execute(&mut *tx).await?.rows_affected()!=1{return Err(missing())}
    if b.disabled {
        sqlx::query("UPDATE api_keys SET revoked_at=coalesce(revoked_at,now()) WHERE workspace_id=$1 AND service_account_id=$2").bind(ws).bind(id).execute(&mut *tx).await?;
    }
    audit(
        &mut tx,
        &u,
        Some(ws),
        "service_account.updated",
        "service_account",
        Some(id),
        json!({"disabled":b.disabled}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
