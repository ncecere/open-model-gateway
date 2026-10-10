//! Alert rules, alert history and in-app notifications (docs/alerts.md).
//!
//! - Installation rules: Platform Admins write, Auditors read
//!   (`/platform/alerts/*`). Every rule kind; budget rules watch Team/Project
//!   budgets (never personal workspaces); installation spend rules watch total
//!   spend against a reference amount (there is no installation budget).
//! - Workspace rules: Team/Project admins write (actual membership); platform
//!   readers may read. Budget, spend-spike and error-rate kinds over that
//!   workspace only, never installation spend or connections.
//! - Personal workspaces have built-in budget alerts for their owner only;
//!   nothing is configurable and nobody else sees them.
//! - Notifications follow live authority: installation incidents for platform
//!   readers; workspace incidents for that workspace's admins (and platform
//!   readers when the rule notifies Platform Admins); built-in incidents for
//!   the owner. Payloads never carry key ids/names, owner identities or
//!   request content. Read state is per user.
use super::*;
use crate::alerts::{self as core, BUDGET_LAYERS, Kind, PERSONAL_THRESHOLDS};

pub(super) const PERSONAL_BUILTIN_ONLY: &str =
    "Personal workspaces have built-in budget alerts only";
pub(super) const RULE_LIMIT: &str = "This scope already has the maximum number of alert rules";
pub(super) const KIND_FIXED: &str = "An alert rule's kind can't be changed";
const RULES_PER_SCOPE: i64 = 50;
const EMAILS: usize = 10;

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route(
            "/api/v1/platform/alerts/rules",
            get(platform_rules).post(create_platform_rule),
        )
        .route(
            "/api/v1/platform/alerts/rules/{id}",
            get(platform_rule)
                .put(update_platform_rule)
                .delete(delete_platform_rule),
        )
        .route("/api/v1/platform/alerts/events", get(platform_events))
        .route(
            "/api/v1/workspaces/{ws}/alerts/rules",
            get(workspace_rules).post(create_workspace_rule),
        )
        .route(
            "/api/v1/workspaces/{ws}/alerts/rules/{id}",
            get(workspace_rule)
                .put(update_workspace_rule)
                .delete(delete_workspace_rule),
        )
        .route(
            "/api/v1/workspaces/{ws}/alerts/events",
            get(workspace_events),
        )
        .route("/api/v1/me/notifications", get(notifications))
        .route(
            "/api/v1/me/notifications/summary",
            get(notification_summary),
        )
        .route("/api/v1/me/notifications/read", post(mark_read))
}

// ---------- Rule shape ----------

const RULE_JSON: &str = "jsonb_build_object('id',r.id,'scope',r.scope,'workspace_id',r.workspace_id,'kind',r.kind,'name',r.name,'enabled',r.enabled,'budget_layers',to_jsonb(r.budget_layers),'thresholds',to_jsonb(r.thresholds),'spike_factor_percent',r.spike_factor_percent,'min_spend_microusd',r.min_spend_microusd::text,'window_minutes',r.window_minutes,'error_rate_percent',r.error_rate_percent,'min_requests',r.min_requests,'consecutive_failures',r.consecutive_failures,'provider_connection_id',r.provider_connection_id,'spend_period',r.spend_period,'spend_amount_microusd',r.spend_amount_microusd::text,'ceiling_requests_per_second',r.ceiling_requests_per_second,'ceiling_lock_wait_ms',r.ceiling_lock_wait_ms,'provider_connection',(SELECT jsonb_build_object('id',p.id,'name',p.name,'provider',p.provider) FROM provider_connections p WHERE p.id=r.provider_connection_id),'notify_workspace_admins',r.notify_workspace_admins,'notify_platform_admins',r.notify_platform_admins,'notify_emails',to_jsonb(r.notify_emails),'firing',(SELECT count(*) FROM alert_events e WHERE e.rule_id=r.id AND e.resolved_at IS NULL),'last_fired_at',(SELECT max(e.fired_at) FROM alert_events e WHERE e.rule_id=r.id),'created_at',r.created_at,'updated_at',r.updated_at)";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RuleInput {
    name: String,
    kind: String,
    #[serde(default = "enabled_default")]
    enabled: bool,
    budget_layers: Option<Vec<String>>,
    thresholds: Option<Vec<i32>>,
    spike_factor_percent: Option<i32>,
    /// Integer micro-USD string.
    min_spend_microusd: Option<String>,
    window_minutes: Option<i32>,
    error_rate_percent: Option<i32>,
    min_requests: Option<i32>,
    consecutive_failures: Option<i32>,
    provider_connection_id: Option<Uuid>,
    /// Installation spend rules: `day`, `week`, `month` or `lifetime`.
    spend_period: Option<String>,
    /// Installation spend rules: reference amount, integer micro-USD string.
    spend_amount_microusd: Option<String>,
    /// Admission ceiling rules: sustained admissions per second of one scope.
    ceiling_requests_per_second: Option<i32>,
    /// Admission ceiling rules: lock-wait p95 threshold (a bucket bound, ms).
    ceiling_lock_wait_ms: Option<i32>,
    #[serde(default)]
    notify_workspace_admins: bool,
    #[serde(default)]
    notify_platform_admins: bool,
    #[serde(default)]
    notify_emails: Vec<String>,
}
fn enabled_default() -> bool {
    true
}

/// A validated rule, exactly as stored.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Valid {
    name: String,
    kind: Kind,
    enabled: bool,
    budget_layers: Option<Vec<String>>,
    thresholds: Option<Vec<i32>>,
    spike_factor_percent: Option<i32>,
    min_spend_microusd: Option<i64>,
    window_minutes: Option<i32>,
    error_rate_percent: Option<i32>,
    min_requests: Option<i32>,
    consecutive_failures: Option<i32>,
    provider_connection_id: Option<Uuid>,
    spend_period: Option<String>,
    spend_amount_microusd: Option<i64>,
    ceiling_requests_per_second: Option<i32>,
    ceiling_lock_wait_ms: Option<i32>,
    notify_workspace_admins: bool,
    notify_platform_admins: bool,
    notify_emails: Vec<String>,
}

/// A positive integer micro-USD string of at most `digits` digits that fits i64.
fn microusd(value: &str, digits: usize) -> Result<i64, ApiError> {
    if value.is_empty() || value.len() > digits || !value.bytes().all(|c| c.is_ascii_digit()) {
        return Err(invalid());
    }
    let n: i64 = value.parse().map_err(|_| invalid())?;
    if n < 1 {
        return Err(invalid());
    }
    Ok(n)
}

/// Percent thresholds: 1–5 distinct values from 1 to 100, sorted.
fn thresholds(values: Option<Vec<i32>>) -> Result<Vec<i32>, ApiError> {
    let mut thresholds = values.ok_or_else(invalid)?;
    thresholds.sort_unstable();
    thresholds.dedup();
    if thresholds.is_empty()
        || thresholds.len() > 5
        || thresholds.iter().any(|t| !(1..=100).contains(t))
    {
        return Err(invalid());
    }
    Ok(thresholds)
}

/// Strict validation; `workspace` selects the narrower workspace-rule shape.
pub(super) fn validate(b: RuleInput, workspace: bool) -> Result<Valid, ApiError> {
    let name = b.name.trim().to_owned();
    if !valid_name(&name) {
        return Err(invalid());
    }
    let kind = Kind::parse(&b.kind).ok_or_else(invalid)?;
    if workspace && matches!(kind, Kind::Provider | Kind::Spend | Kind::AdmissionCeiling) {
        return Err(invalid());
    }
    let mut v = Valid {
        name,
        kind,
        enabled: b.enabled,
        budget_layers: None,
        thresholds: None,
        spike_factor_percent: None,
        min_spend_microusd: None,
        window_minutes: None,
        error_rate_percent: None,
        min_requests: None,
        consecutive_failures: None,
        provider_connection_id: None,
        spend_period: None,
        spend_amount_microusd: None,
        ceiling_requests_per_second: None,
        ceiling_lock_wait_ms: None,
        notify_workspace_admins: b.notify_workspace_admins,
        notify_platform_admins: b.notify_platform_admins,
        notify_emails: Vec::new(),
    };
    // Fields that do not belong to the kind must be absent.
    let budget = b.budget_layers.is_some() || b.thresholds.is_some();
    let spike = b.spike_factor_percent.is_some() || b.min_spend_microusd.is_some();
    let window =
        b.window_minutes.is_some() || b.error_rate_percent.is_some() || b.min_requests.is_some();
    let provider = b.consecutive_failures.is_some() || b.provider_connection_id.is_some();
    let spend = b.spend_period.is_some() || b.spend_amount_microusd.is_some();
    let ceiling = b.ceiling_requests_per_second.is_some() || b.ceiling_lock_wait_ms.is_some();
    // There is no installation budget layer any more (0026): say so plainly.
    if b.budget_layers
        .as_ref()
        .is_some_and(|l| l.iter().any(|l| l == "installation"))
    {
        return Err(super::installation_limits_removed(StatusCode::BAD_REQUEST));
    }
    let allowed = (!spend || kind == Kind::Spend) && (!ceiling || kind == Kind::AdmissionCeiling);
    let allowed = allowed
        && match kind {
            // Window plus a rate and/or lock-wait threshold; the incident
            // names the workspace, so email goes to Platform Admins only.
            Kind::AdmissionCeiling => {
                !budget
                    && !spike
                    && !provider
                    && b.error_rate_percent.is_none()
                    && b.min_requests.is_none()
                    && b.notify_emails.is_empty()
            }
            Kind::Budget => !spike && !window && !provider,
            Kind::Spend => b.budget_layers.is_none() && !spike && !window && !provider,
            Kind::Spike => !budget && !window && !provider,
            Kind::ErrorRate => !budget && !spike && !provider,
            Kind::Provider => !budget && !spike,
            Kind::BatchFailed => !budget && !spike && !window && !provider,
            Kind::BatchStalled => {
                !budget
                    && !spike
                    && !provider
                    && b.error_rate_percent.is_none()
                    && b.min_requests.is_none()
            }
        };
    if !allowed {
        return Err(invalid());
    }
    match kind {
        Kind::Budget => {
            let mut layers = b.budget_layers.ok_or_else(invalid)?;
            layers.sort_by_key(|l| BUDGET_LAYERS.iter().position(|x| x == l));
            layers.dedup();
            if layers.is_empty() || layers.iter().any(|l| !BUDGET_LAYERS.contains(&l.as_str())) {
                return Err(invalid());
            }
            v.budget_layers = Some(layers);
            v.thresholds = Some(thresholds(b.thresholds)?);
        }
        // Installation spend: percent thresholds of a reference amount per period.
        Kind::Spend => {
            let period = b.spend_period.ok_or_else(invalid)?;
            if crate::governance::BudgetPeriod::parse(&period).is_none() {
                return Err(invalid());
            }
            // Like budget amounts: any positive i64.
            v.spend_amount_microusd = Some(microusd(
                b.spend_amount_microusd.as_deref().ok_or_else(invalid)?,
                19,
            )?);
            v.spend_period = Some(period);
            v.thresholds = Some(thresholds(b.thresholds)?);
        }
        Kind::Spike => {
            let factor = b.spike_factor_percent.ok_or_else(invalid)?;
            let floor = microusd(&b.min_spend_microusd.ok_or_else(invalid)?, 15)?;
            if !(110..=100_000).contains(&factor) {
                return Err(invalid());
            }
            v.spike_factor_percent = Some(factor);
            v.min_spend_microusd = Some(floor);
        }
        // A batch failed or expired: no parameters.
        Kind::BatchFailed => {}
        // Minute counters are retained 10 minutes (0024).
        Kind::AdmissionCeiling => {
            let minutes = b.window_minutes.ok_or_else(invalid)?;
            if !(5..=10).contains(&minutes) {
                return Err(invalid());
            }
            v.window_minutes = Some(minutes);
            if let Some(rate) = b.ceiling_requests_per_second {
                if !(1..=100_000).contains(&rate) {
                    return Err(invalid());
                }
                v.ceiling_requests_per_second = Some(rate);
            }
            if let Some(ms) = b.ceiling_lock_wait_ms {
                if !crate::governance::pressure::WAIT_BUCKETS_MS.contains(&ms) {
                    return Err(invalid());
                }
                v.ceiling_lock_wait_ms = Some(ms);
            }
            if v.ceiling_requests_per_second.is_none() && v.ceiling_lock_wait_ms.is_none() {
                return Err(invalid());
            }
        }
        // No progress for `window_minutes`.
        Kind::BatchStalled => {
            let minutes = b.window_minutes.ok_or_else(invalid)?;
            if !(5..=1440).contains(&minutes) {
                return Err(invalid());
            }
            v.window_minutes = Some(minutes);
        }
        Kind::ErrorRate | Kind::Provider => {
            let minutes = b.window_minutes.ok_or_else(invalid)?;
            if !(5..=1440).contains(&minutes) {
                return Err(invalid());
            }
            v.window_minutes = Some(minutes);
            match (b.error_rate_percent, b.min_requests) {
                (Some(rate), Some(min))
                    if (1..=100).contains(&rate) && (1..=100_000).contains(&min) =>
                {
                    v.error_rate_percent = Some(rate);
                    v.min_requests = Some(min);
                }
                (None, None) if kind == Kind::Provider => {}
                _ => return Err(invalid()),
            }
            if kind == Kind::Provider {
                if let Some(n) = b.consecutive_failures {
                    if !(1..=100).contains(&n) {
                        return Err(invalid());
                    }
                    v.consecutive_failures = Some(n);
                }
                if v.consecutive_failures.is_none() && v.error_rate_percent.is_none() {
                    return Err(invalid());
                }
                v.provider_connection_id = b.provider_connection_id;
            }
        }
    }
    // Installation rules email Platform Admins or listed addresses; workspace admins
    // belong to workspace rules.
    if !workspace && v.notify_workspace_admins {
        return Err(invalid());
    }
    if b.notify_emails.len() > EMAILS {
        return Err(invalid());
    }
    let mut emails = Vec::new();
    for e in b.notify_emails {
        let e = e.trim().to_lowercase();
        if !directory::email_valid(&e) || e.parse::<lettre::Address>().is_err() {
            return Err(invalid());
        }
        if !emails.contains(&e) {
            emails.push(e);
        }
    }
    v.notify_emails = emails;
    Ok(v)
}

async fn check_connection(tx: &mut Transaction<'_, Postgres>, v: &Valid) -> Result<(), ApiError> {
    if let Some(id) = v.provider_connection_id {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM provider_connections WHERE id=$1)")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?;
        if !exists {
            return Err(invalid());
        }
    }
    Ok(())
}

async fn rule_json(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<Value, ApiError> {
    sqlx::query_scalar(&format!(
        "SELECT {RULE_JSON} FROM alert_rules r WHERE r.id=$1 AND r.deleted_at IS NULL"
    ))
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(missing)
}

async fn insert_rule(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    ws: Option<Uuid>,
    v: &Valid,
) -> Result<Uuid, ApiError> {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM alert_rules WHERE deleted_at IS NULL AND workspace_id IS NOT DISTINCT FROM $1")
        .bind(ws)
        .fetch_one(&mut **tx)
        .await?;
    if count >= RULES_PER_SCOPE {
        return Err(ApiError(StatusCode::CONFLICT, RULE_LIMIT));
    }
    check_connection(tx, v).await?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO alert_rules(id,scope,workspace_id,kind,name,enabled,budget_layers,thresholds,spike_factor_percent,min_spend_microusd,window_minutes,error_rate_percent,min_requests,consecutive_failures,provider_connection_id,notify_workspace_admins,notify_platform_admins,notify_emails,created_by,updated_by,spend_period,spend_amount_microusd,ceiling_requests_per_second,ceiling_lock_wait_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$19,$20,$21,$22,$23)")
        .bind(id)
        .bind(if ws.is_some() { "workspace" } else { "installation" })
        .bind(ws)
        .bind(v.kind.as_str())
        .bind(&v.name)
        .bind(v.enabled)
        .bind(&v.budget_layers)
        .bind(&v.thresholds)
        .bind(v.spike_factor_percent)
        .bind(v.min_spend_microusd)
        .bind(v.window_minutes)
        .bind(v.error_rate_percent)
        .bind(v.min_requests)
        .bind(v.consecutive_failures)
        .bind(v.provider_connection_id)
        .bind(v.notify_workspace_admins)
        .bind(v.notify_platform_admins)
        .bind(&v.notify_emails)
        .bind(u.user_id)
        .bind(&v.spend_period)
        .bind(v.spend_amount_microusd)
        .bind(v.ceiling_requests_per_second)
        .bind(v.ceiling_lock_wait_ms)
        .execute(&mut **tx)
        .await?;
    audit(
        tx,
        u,
        ws,
        "alert_rule.created",
        "alert_rule",
        Some(id),
        json!({"kind": v.kind.as_str(), "enabled": v.enabled, "count": v.notify_emails.len()}),
    )
    .await?;
    Ok(id)
}

async fn update_rule(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    ws: Option<Uuid>,
    id: Uuid,
    v: &Valid,
) -> Result<(), ApiError> {
    let kind: String = sqlx::query_scalar("SELECT kind FROM alert_rules WHERE id=$1 AND deleted_at IS NULL AND workspace_id IS NOT DISTINCT FROM $2 FOR UPDATE")
        .bind(id)
        .bind(ws)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(missing)?;
    if kind != v.kind.as_str() {
        return Err(ApiError(StatusCode::CONFLICT, KIND_FIXED));
    }
    check_connection(tx, v).await?;
    sqlx::query("UPDATE alert_rules SET name=$2,enabled=$3,budget_layers=$4,thresholds=$5,spike_factor_percent=$6,min_spend_microusd=$7,window_minutes=$8,error_rate_percent=$9,min_requests=$10,consecutive_failures=$11,provider_connection_id=$12,notify_workspace_admins=$13,notify_platform_admins=$14,notify_emails=$15,updated_by=$16,spend_period=$17,spend_amount_microusd=$18,ceiling_requests_per_second=$19,ceiling_lock_wait_ms=$20,updated_at=now() WHERE id=$1")
        .bind(id)
        .bind(&v.name)
        .bind(v.enabled)
        .bind(&v.budget_layers)
        .bind(&v.thresholds)
        .bind(v.spike_factor_percent)
        .bind(v.min_spend_microusd)
        .bind(v.window_minutes)
        .bind(v.error_rate_percent)
        .bind(v.min_requests)
        .bind(v.consecutive_failures)
        .bind(v.provider_connection_id)
        .bind(v.notify_workspace_admins)
        .bind(v.notify_platform_admins)
        .bind(&v.notify_emails)
        .bind(u.user_id)
        .bind(&v.spend_period)
        .bind(v.spend_amount_microusd)
        .bind(v.ceiling_requests_per_second)
        .bind(v.ceiling_lock_wait_ms)
        .execute(&mut **tx)
        .await?;
    // A disabled rule's open incidents close now (silently), not on the next tick.
    core::retire_inactive(tx, Some(id)).await?;
    audit(
        tx,
        u,
        ws,
        "alert_rule.updated",
        "alert_rule",
        Some(id),
        json!({"kind": v.kind.as_str(), "enabled": v.enabled, "count": v.notify_emails.len()}),
    )
    .await?;
    Ok(())
}

async fn delete_rule(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    ws: Option<Uuid>,
    id: Uuid,
) -> Result<(), ApiError> {
    let changed = sqlx::query("UPDATE alert_rules SET deleted_at=now(),enabled=false,updated_by=$3,updated_at=now() WHERE id=$1 AND deleted_at IS NULL AND workspace_id IS NOT DISTINCT FROM $2")
        .bind(id)
        .bind(ws)
        .bind(u.user_id)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    if changed == 0 {
        return Err(missing());
    }
    core::retire_inactive(tx, Some(id)).await?;
    audit(
        tx,
        u,
        ws,
        "alert_rule.deleted",
        "alert_rule",
        Some(id),
        json!({}),
    )
    .await?;
    Ok(())
}

async fn list_rules(
    tx: &mut Transaction<'_, Postgres>,
    ws: Option<Uuid>,
) -> Result<Vec<Value>, ApiError> {
    Ok(sqlx::query_scalar(&format!("SELECT {RULE_JSON} FROM alert_rules r WHERE r.deleted_at IS NULL AND r.workspace_id IS NOT DISTINCT FROM $1 ORDER BY lower(r.name),r.id LIMIT 200"))
        .bind(ws)
        .fetch_all(&mut **tx)
        .await?)
}

// ---------- Installation rules ----------

async fn platform_tx<'a>(
    s: &'a Store,
    u: &BrowserPrincipal,
    write: bool,
) -> Result<Transaction<'a, Postgres>, ApiError> {
    let mut tx = resources::installation_tx(s).await?;
    if write {
        resources::platform_write(&mut tx, u.user_id).await?;
    } else {
        resources::platform_read(&mut tx, u.user_id).await?;
    }
    Ok(tx)
}
async fn platform_rules(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
) -> ApiResult {
    let mut tx = platform_tx(&s, &u, false).await?;
    let data = list_rules(&mut tx, None).await?;
    tx.commit().await?;
    Ok(Json(json!({"data": data})))
}
async fn platform_rule(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut tx = platform_tx(&s, &u, false).await?;
    let v = scoped_rule(&mut tx, None, id).await?;
    tx.commit().await?;
    Ok(Json(v))
}
async fn scoped_rule(
    tx: &mut Transaction<'_, Postgres>,
    ws: Option<Uuid>,
    id: Uuid,
) -> Result<Value, ApiError> {
    sqlx::query_scalar(&format!("SELECT {RULE_JSON} FROM alert_rules r WHERE r.id=$1 AND r.deleted_at IS NULL AND r.workspace_id IS NOT DISTINCT FROM $2"))
        .bind(id)
        .bind(ws)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(missing)
}
async fn create_platform_rule(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<RuleInput>,
) -> ApiResult {
    let mut tx = platform_tx(&s, &u, true).await?;
    let v = validate(b, false)?;
    let id = insert_rule(&mut tx, &u, None, &v).await?;
    let out = rule_json(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Json(out))
}
async fn update_platform_rule(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Json(b): Json<RuleInput>,
) -> ApiResult {
    let mut tx = platform_tx(&s, &u, true).await?;
    let v = validate(b, false)?;
    update_rule(&mut tx, &u, None, id, &v).await?;
    let out = rule_json(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Json(out))
}
async fn delete_platform_rule(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut tx = platform_tx(&s, &u, true).await?;
    delete_rule(&mut tx, &u, None, id).await?;
    tx.commit().await?;
    Ok(ok())
}

// ---------- Workspace rules ----------

/// Read: a personal owner, a Team/Project admin, or a platform reader (shared only).
async fn workspace_read<'a>(
    s: &'a Store,
    u: &BrowserPrincipal,
    ws: Uuid,
) -> Result<(Transaction<'a, Postgres>, resources::WorkspaceAccess), ApiError> {
    let (tx, a) = resources::workspace_read_tx(s, u, ws).await?;
    if !(a.admin || (a.platform_reader && shared(&a.kind))) {
        return Err(denied());
    }
    Ok((tx, a))
}
/// Write: a Team/Project admin with actual membership. Personal rules are built in.
async fn workspace_write<'a>(
    s: &'a Store,
    u: &BrowserPrincipal,
    ws: Uuid,
) -> Result<Transaction<'a, Postgres>, ApiError> {
    let (tx, a) = resources::workspace_tx(s, u, ws).await?;
    if a.kind == "personal" {
        return Err(ApiError(StatusCode::FORBIDDEN, PERSONAL_BUILTIN_ONLY));
    }
    resources::manage(&a)?;
    Ok(tx)
}
fn builtin_json() -> Value {
    json!({"kind": "budget_threshold", "thresholds": PERSONAL_THRESHOLDS, "budget_layers": ["type", "override", "local", "key"]})
}
async fn workspace_rules(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
) -> ApiResult {
    let (mut tx, a) = workspace_read(&s, &u, ws).await?;
    let personal = a.kind == "personal";
    let data = if personal {
        vec![]
    } else {
        list_rules(&mut tx, Some(ws)).await?
    };
    tx.commit().await?;
    Ok(Json(
        json!({"data": data, "builtin": personal.then(builtin_json), "writable": !personal && a.admin && !a.disabled}),
    ))
}
async fn workspace_rule(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    let (mut tx, _) = workspace_read(&s, &u, ws).await?;
    let v = scoped_rule(&mut tx, Some(ws), id).await?;
    tx.commit().await?;
    Ok(Json(v))
}
async fn create_workspace_rule(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(b): Json<RuleInput>,
) -> ApiResult {
    let mut tx = workspace_write(&s, &u, ws).await?;
    let v = validate(b, true)?;
    let id = insert_rule(&mut tx, &u, Some(ws), &v).await?;
    let out = rule_json(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Json(out))
}
async fn update_workspace_rule(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, Uuid)>,
    Json(b): Json<RuleInput>,
) -> ApiResult {
    let mut tx = workspace_write(&s, &u, ws).await?;
    let v = validate(b, true)?;
    update_rule(&mut tx, &u, Some(ws), id, &v).await?;
    let out = rule_json(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Json(out))
}
async fn delete_workspace_rule(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    let mut tx = workspace_write(&s, &u, ws).await?;
    delete_rule(&mut tx, &u, Some(ws), id).await?;
    tx.commit().await?;
    Ok(ok())
}

// ---------- History ----------

/// One incident. Workspace references are Team/Project or the viewer's own
/// personal workspace; no key, owner or request details.
const EVENT_JSON: &str = "jsonb_build_object('id',e.id,'rule',CASE WHEN r.id IS NULL THEN NULL ELSE jsonb_build_object('id',r.id,'name',r.name,'scope',r.scope,'deleted',r.deleted_at IS NOT NULL) END,'builtin',e.builtin IS NOT NULL,'kind',e.kind,'state',CASE WHEN e.resolved_at IS NULL THEN 'firing' ELSE 'resolved' END,'severity',e.severity,'level',e.level,'summary',e.summary,'details',e.details,'workspace',CASE WHEN w.id IS NULL THEN NULL ELSE jsonb_build_object('id',w.id,'name',w.name,'kind',w.kind) END,'connection',CASE WHEN p.id IS NULL THEN NULL ELSE jsonb_build_object('id',p.id,'name',p.name,'provider',p.provider) END,'fired_at',e.fired_at,'resolved_at',e.resolved_at,'resolution',e.resolution,'email',(SELECT jsonb_build_object('status',d.status,'recipients',d.recipients,'sent',d.sent,'failed',d.failed,'error',d.error) FROM alert_deliveries d WHERE d.event_id=e.id AND d.transition='fired'))";
const EVENT_FROM: &str = "FROM alert_events e LEFT JOIN alert_rules r ON r.id=e.rule_id LEFT JOIN workspaces w ON w.id=e.workspace_id LEFT JOIN provider_connections p ON p.id=e.provider_connection_id";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EventQuery {
    /// `firing` or `resolved`.
    status: Option<String>,
    kind: Option<String>,
    rule_id: Option<Uuid>,
    limit: Option<i64>,
    offset: Option<i64>,
}
impl EventQuery {
    fn checked(&self) -> Result<(i64, i64), ApiError> {
        if self
            .status
            .as_deref()
            .is_some_and(|s| !matches!(s, "firing" | "resolved"))
            || self.kind.as_deref().is_some_and(|k| {
                Kind::parse(k).is_none() && !crate::alerts::INSTALLATION_BUILTINS.contains(&k)
            })
        {
            return Err(invalid());
        }
        Page {
            limit: self.limit,
            offset: self.offset,
        }
        .bounds()
    }
}
/// `$1..$5` are fixed: status, kind, rule, limit+1, offset; `scope` uses `$6`.
async fn events(
    tx: &mut Transaction<'_, Postgres>,
    q: &EventQuery,
    scope: &str,
    ws: Option<Uuid>,
) -> ApiResult {
    let (l, o) = q.checked()?;
    let mut data: Vec<Value> = sqlx::query_scalar(&format!("SELECT {EVENT_JSON} {EVENT_FROM} WHERE {scope} AND ($1::text IS NULL OR ($1='firing')=(e.resolved_at IS NULL)) AND ($2::text IS NULL OR e.kind=$2) AND ($3::uuid IS NULL OR e.rule_id=$3) ORDER BY e.fired_at DESC,e.id LIMIT $4 OFFSET $5"))
        .bind(&q.status)
        .bind(&q.kind)
        .bind(q.rule_id)
        .bind(l + 1)
        .bind(o)
        .bind(ws)
        .fetch_all(&mut **tx)
        .await?;
    let has_more = data.len() as i64 > l;
    data.truncate(l as usize);
    Ok(Json(json!({"data": data, "has_more": has_more})))
}
/// Admission ceiling incidents (0036) identify the workspace for Platform
/// Admins only: other readers (Auditors) get the incident without the
/// workspace reference or name. Personal workspaces are never named at all.
fn redact_ceiling_scope(data: &mut Value, admin: bool) {
    if admin {
        return;
    }
    for e in data.as_array_mut().into_iter().flatten() {
        if e["kind"] == Kind::AdmissionCeiling.as_str() {
            e["workspace"] = Value::Null;
            if let Some(d) = e.get_mut("details").and_then(Value::as_object_mut) {
                d.remove("workspace_name");
            }
        }
    }
}
async fn platform_events(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(q): Query<EventQuery>,
) -> ApiResult {
    let mut tx = platform_tx(&s, &u, false).await?;
    let admin = resources::platform_role(&mut tx, u.user_id).await? == "admin";
    let mut out = events(
        &mut tx,
        &q,
        // Installation rules plus the built-in installation incidents (SCIM
        // last-admin safeguard, missing history partitions).
        "(r.scope='installation' OR e.builtin IN ('scim_last_admin','partitions_missing','history_orphans')) AND $6::uuid IS NULL",
        None,
    )
    .await?;
    tx.commit().await?;
    redact_ceiling_scope(&mut out.0["data"], admin);
    Ok(out)
}
async fn workspace_events(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(q): Query<EventQuery>,
) -> ApiResult {
    let (mut tx, _) = workspace_read(&s, &u, ws).await?;
    // This workspace's rules, or its built-in alerts (personal: owner only, enforced above).
    let out = events(
        &mut tx,
        &q,
        "e.workspace_id=$6 AND (r.scope='workspace' OR e.builtin IS NOT NULL)",
        Some(ws),
    )
    .await?;
    tx.commit().await?;
    Ok(out)
}

// ---------- Notifications ----------

/// Incidents the caller may see, by live authority (`$1` caller, `$2` platform reader).
const VISIBLE: &str = "e.fired_at>now()-interval '90 days' AND ((e.builtin IS NOT NULL AND w.kind='personal' AND w.owner_user_id=$1 AND w.disabled_at IS NULL) OR (r.scope='installation' AND $2) OR (e.builtin IN ('scim_last_admin','partitions_missing','history_orphans') AND $2) OR (r.scope='workspace' AND w.kind IN ('team','project') AND w.disabled_at IS NULL AND (EXISTS(SELECT 1 FROM effective_workspace_memberships m WHERE m.workspace_id=e.workspace_id AND m.user_id=$1 AND m.role IN ('owner','admin')) OR (r.notify_platform_admins AND $2))))";

/// Notifications never take the installation lock (the bell polls).
async fn viewer<'a>(
    s: &'a Store,
    u: &BrowserPrincipal,
) -> Result<(Transaction<'a, Postgres>, bool, bool), ApiError> {
    let mut tx = crate::db::begin(&s.pool).await?;
    let role = resources::platform_role(&mut tx, u.user_id).await?;
    Ok((
        tx,
        matches!(role.as_str(), "admin" | "auditor"),
        role == "admin",
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NotificationQuery {
    /// `unread` lists unread notifications only.
    status: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}
async fn notifications(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(q): Query<NotificationQuery>,
) -> ApiResult {
    let (l, o) = Page {
        limit: q.limit,
        offset: q.offset,
    }
    .bounds()?;
    let unread = match q.status.as_deref() {
        None => false,
        Some("unread") => true,
        Some(_) => return Err(invalid()),
    };
    let (mut tx, reader, admin) = viewer(&s, &u).await?;
    let mut data: Vec<Value> = sqlx::query_scalar(&format!("SELECT {EVENT_JSON}||jsonb_build_object('read',x.event_id IS NOT NULL) {EVENT_FROM} LEFT JOIN alert_reads x ON x.event_id=e.id AND x.user_id=$1 WHERE {VISIBLE} AND (NOT $3 OR x.event_id IS NULL) ORDER BY e.fired_at DESC,e.id LIMIT $4 OFFSET $5"))
        .bind(u.user_id)
        .bind(reader)
        .bind(unread)
        .bind(l + 1)
        .bind(o)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    let has_more = data.len() as i64 > l;
    data.truncate(l as usize);
    let mut data = Value::Array(data);
    redact_ceiling_scope(&mut data, admin);
    Ok(Json(json!({"data": data, "has_more": has_more})))
}
async fn notification_summary(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
) -> ApiResult {
    let (mut tx, reader, _) = viewer(&s, &u).await?;
    let (unread, firing): (i64, i64) = sqlx::query_as(&format!("SELECT count(*) FILTER(WHERE NOT EXISTS(SELECT 1 FROM alert_reads x WHERE x.event_id=e.id AND x.user_id=$1)),count(*) FILTER(WHERE e.resolved_at IS NULL) {EVENT_FROM} WHERE {VISIBLE}"))
        .bind(u.user_id)
        .bind(reader)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({"unread": unread, "firing": firing})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReadInput {
    ids: Option<Vec<Uuid>>,
    #[serde(default)]
    all: bool,
}
async fn mark_read(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<ReadInput>,
) -> ApiResult {
    if b.all == b.ids.is_some()
        || b.ids
            .as_ref()
            .is_some_and(|ids| ids.is_empty() || ids.len() > 200)
    {
        return Err(invalid());
    }
    let (mut tx, reader, _) = viewer(&s, &u).await?;
    // Only incidents the caller can see; others are ignored, not disclosed.
    let marked = sqlx::query(&format!("INSERT INTO alert_reads(user_id,event_id) SELECT $1,e.id {EVENT_FROM} WHERE {VISIBLE} AND ($3::uuid[] IS NULL OR e.id=ANY($3)) AND NOT EXISTS(SELECT 1 FROM alert_reads x WHERE x.event_id=e.id AND x.user_id=$1) ORDER BY e.fired_at DESC LIMIT 1000 ON CONFLICT DO NOTHING"))
        .bind(u.user_id)
        .bind(reader)
        .bind(&b.ids)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(Json(json!({"marked": marked})))
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    fn input(v: Value) -> RuleInput {
        serde_json::from_value(v).unwrap()
    }
    #[test]
    fn rules_are_strict_per_kind_and_scope() {
        let budget = json!({"name":" Budgets ","kind":"budget_threshold","budget_layers":["key","type","local","key"],"thresholds":[100,50,80,80],"notify_platform_admins":true,"notify_emails":[" Ops@Example.test ","ops@example.test"]});
        let v = validate(input(budget.clone()), false).unwrap();
        assert_eq!(v.name, "Budgets");
        assert_eq!(
            v.budget_layers.as_deref().unwrap(),
            ["type", "local", "key"]
        );
        assert_eq!(v.thresholds.as_deref().unwrap(), [50, 80, 100]);
        assert_eq!(v.notify_emails, ["ops@example.test"]);
        // There is no installation budget layer (0026), in any scope, with a
        // stable reason.
        for workspace in [false, true] {
            let err = validate(
                input(json!({"name":"x","kind":"budget_threshold","budget_layers":["installation","local"],"thresholds":[80]})),
                workspace,
            )
            .unwrap_err();
            assert_eq!(err.0, StatusCode::BAD_REQUEST);
            assert_eq!(err.1, crate::management::INSTALLATION_LIMITS_REMOVED);
        }
        // Installation spend rules: percent thresholds of a reference amount.
        let spend = json!({"name":"Spend","kind":"spend_threshold","spend_period":"month","spend_amount_microusd":"5000000000","thresholds":[100,50],"notify_platform_admins":true});
        let s = validate(input(spend.clone()), false).unwrap();
        assert_eq!(
            (
                s.spend_period.as_deref(),
                s.spend_amount_microusd,
                s.thresholds.as_deref()
            ),
            (Some("month"), Some(5_000_000_000), Some(&[50, 100][..]))
        );
        // Installation only.
        assert!(validate(input(spend), true).is_err());
        for bad in [
            json!({"name":"x","kind":"spend_threshold","spend_period":"year","spend_amount_microusd":"1","thresholds":[50]}),
            json!({"name":"x","kind":"spend_threshold","spend_period":"day","spend_amount_microusd":"0","thresholds":[50]}),
            json!({"name":"x","kind":"spend_threshold","spend_period":"day","spend_amount_microusd":"1.5","thresholds":[50]}),
            json!({"name":"x","kind":"spend_threshold","spend_period":"day","spend_amount_microusd":"1"}),
            json!({"name":"x","kind":"spend_threshold","spend_amount_microusd":"1","thresholds":[50]}),
            json!({"name":"x","kind":"spend_threshold","spend_period":"day","spend_amount_microusd":"1","thresholds":[50],"budget_layers":["local"]}),
            json!({"name":"x","kind":"budget_threshold","budget_layers":["local"],"thresholds":[50],"spend_period":"day"}),
        ] {
            assert!(validate(input(bad.clone()), false).is_err(), "{bad}");
        }
        // Workspace rules never watch connections.
        assert!(validate(input(json!({"name":"x","kind":"provider_failing","window_minutes":15,"consecutive_failures":5})), true).is_err());
        for bad in [
            json!({"name":"x","kind":"budget_threshold","budget_layers":["local"],"thresholds":[0]}),
            json!({"name":"x","kind":"budget_threshold","budget_layers":["local"],"thresholds":[101]}),
            json!({"name":"x","kind":"budget_threshold","budget_layers":[],"thresholds":[50]}),
            json!({"name":"x","kind":"budget_threshold","budget_layers":["local"],"thresholds":[50],"window_minutes":10}),
            json!({"name":"x","kind":"spend_spike","spike_factor_percent":100,"min_spend_microusd":"1"}),
            json!({"name":"x","kind":"spend_spike","spike_factor_percent":300,"min_spend_microusd":"0"}),
            json!({"name":"x","kind":"spend_spike","spike_factor_percent":300,"min_spend_microusd":"1.5"}),
            json!({"name":"x","kind":"error_rate","window_minutes":4,"error_rate_percent":10,"min_requests":1}),
            json!({"name":"x","kind":"error_rate","window_minutes":15,"error_rate_percent":10}),
            json!({"name":"x","kind":"provider_failing","window_minutes":15}),
            json!({"name":"x","kind":"bogus"}),
            json!({"name":"","kind":"spend_spike","spike_factor_percent":300,"min_spend_microusd":"1"}),
            json!({"name":"x","kind":"spend_spike","spike_factor_percent":300,"min_spend_microusd":"1","notify_emails":["not-an-email"]}),
            json!({"name":"x","kind":"spend_spike","spike_factor_percent":300,"min_spend_microusd":"1","notify_workspace_admins":true}),
        ] {
            assert!(validate(input(bad.clone()), false).is_err(), "{bad}");
        }
        let provider = json!({"name":"Upstream","kind":"provider_failing","window_minutes":15,"consecutive_failures":5});
        assert_eq!(
            validate(input(provider.clone()), false)
                .unwrap()
                .consecutive_failures,
            Some(5)
        );
        assert!(validate(input(provider), true).is_err());
        let spike = json!({"name":"Spike","kind":"spend_spike","spike_factor_percent":300,"min_spend_microusd":"1000000","notify_workspace_admins":true});
        assert_eq!(
            validate(input(spike), true).unwrap().min_spend_microusd,
            Some(1_000_000)
        );
    }
}
