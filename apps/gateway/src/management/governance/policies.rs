use super::*;
use crate::governance::{BudgetPeriod, budget_consumption};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

/// One budget per period; every entry is enforced over its own window.
pub(crate) type Budgets = BTreeMap<BudgetPeriod, i64>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BudgetInput {
    period: String,
    amount_microusd: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Policy {
    #[serde(deserialize_with = "nullable")]
    requests_per_minute: Option<i64>,
    #[serde(deserialize_with = "nullable")]
    tokens_per_minute: Option<i64>,
    #[serde(deserialize_with = "nullable")]
    concurrent_requests: Option<i64>,
    /// "Jobs at once" (0018). Optional for older clients: absent keeps the
    /// stored value; explicit null clears it (tighten-only rules still apply).
    #[serde(default, deserialize_with = "supplied")]
    concurrent_jobs: Option<Option<i64>>,
    /// "Storage" quota in bytes (0020): type default, platform override and
    /// workspace local layers only. Absent keeps the stored value; explicit
    /// null clears it (tighten-only rules still apply).
    #[serde(default, deserialize_with = "supplied")]
    storage_bytes: Option<Option<i64>>,
    /// Deprecated single budget (amount per `budget_period`). Required unless
    /// `budgets` is supplied; then it must be absent.
    #[serde(default, deserialize_with = "supplied")]
    monthly_budget_microusd: Option<Option<String>>,
    /// Deprecated; only with `monthly_budget_microusd`. Explicit null rejected.
    #[serde(default, deserialize_with = "present")]
    budget_period: Option<String>,
    /// Full replacement set of stacked budgets (one per period).
    #[serde(default, deserialize_with = "present")]
    budgets: Option<Vec<BudgetInput>>,
}
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}
fn supplied<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct Limits {
    pub(crate) requests_per_minute: Option<i64>,
    pub(crate) tokens_per_minute: Option<i64>,
    pub(crate) concurrent_requests: Option<i64>,
    /// Concurrent active async jobs (video + batch).
    pub(crate) concurrent_jobs: Option<i64>,
    /// File store quota in bytes (workspace layers only; never installation/key).
    pub(crate) storage_bytes: Option<i64>,
    pub(crate) budgets: Budgets,
}
type Row = (
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);
const COLS: &str = "requests_per_minute,tokens_per_minute,concurrent_requests,concurrent_jobs";
/// Largest storage quota (1 PiB); rate limits stay within i32.
const MAX_STORAGE_BYTES: i64 = 1 << 50;
/// The storage column of a layer table: real on workspace layers, NULL elsewhere.
fn storage_col(scope: &Scope) -> &'static str {
    match scope {
        Scope::Type(_) | Scope::Override(_) | Scope::Local(_) => "storage_bytes",
        Scope::Installation | Scope::Key(..) => "NULL::bigint",
    }
}
impl Limits {
    fn rates(&self) -> [Option<i64>; 5] {
        [
            self.requests_per_minute,
            self.tokens_per_minute,
            self.concurrent_requests,
            self.concurrent_jobs,
            self.storage_bytes,
        ]
    }
    /// Deprecated single-budget mirror: the smallest amount (ties prefer the shorter period).
    pub(crate) fn legacy(&self) -> Option<(BudgetPeriod, i64)> {
        self.budgets
            .iter()
            .min_by_key(|(p, a)| (**a, **p))
            .map(|(p, a)| (*p, *a))
    }
    fn legacy_period(&self) -> Option<BudgetPeriod> {
        (self.budgets.len() == 1)
            .then(|| self.budgets.keys().next().copied())
            .flatten()
    }
}
fn rates(row: Option<Row>) -> Limits {
    let (r, t, c, j, st) = row.unwrap_or_default();
    Limits {
        requests_per_minute: r,
        tokens_per_minute: t,
        concurrent_requests: c,
        concurrent_jobs: j,
        storage_bytes: st,
        budgets: Budgets::new(),
    }
}
/// Storage scope of one policy layer.
#[derive(Clone, Debug)]
pub(crate) enum Scope {
    Installation,
    Type(String),
    Override(Uuid),
    Local(Uuid),
    Key(Uuid, Uuid),
}
impl Scope {
    fn parts(&self) -> (&'static str, Option<String>, Option<Uuid>, Option<Uuid>) {
        match self {
            Self::Installation => ("installation", None, None, None),
            Self::Type(k) => ("type", Some(k.clone()), None, None),
            Self::Override(w) => ("override", None, Some(*w), None),
            Self::Local(w) => ("local", None, Some(*w), None),
            Self::Key(w, k) => ("key", None, Some(*w), Some(*k)),
        }
    }
}
const SCOPE_WHERE: &str = "layer=$1 AND kind IS NOT DISTINCT FROM $2 AND workspace_id IS NOT DISTINCT FROM $3 AND governance_key_id IS NOT DISTINCT FROM $4";
pub(crate) async fn load_budgets(
    tx: &mut Transaction<'_, Postgres>,
    scope: &Scope,
) -> Result<Budgets, ApiError> {
    let (layer, kind, ws, key) = scope.parts();
    let rows: Vec<(String, i64)> = sqlx::query_as(&format!(
        "SELECT period,amount_microusd FROM policy_budgets WHERE {SCOPE_WHERE}"
    ))
    .bind(layer)
    .bind(kind)
    .bind(ws)
    .bind(key)
    .fetch_all(&mut **tx)
    .await?;
    rows.into_iter()
        .map(|(p, a)| {
            BudgetPeriod::parse(&p).map(|p| (p, a)).ok_or(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Stored policy is invalid",
            ))
        })
        .collect()
}
async fn store_budgets(
    tx: &mut Transaction<'_, Postgres>,
    scope: &Scope,
    budgets: &Budgets,
) -> Result<(), ApiError> {
    let (layer, kind, ws, key) = scope.parts();
    sqlx::query(&format!("DELETE FROM policy_budgets WHERE {SCOPE_WHERE}"))
        .bind(layer)
        .bind(kind.clone())
        .bind(ws)
        .bind(key)
        .execute(&mut **tx)
        .await?;
    for (period, amount) in budgets {
        sqlx::query("INSERT INTO policy_budgets(layer,kind,workspace_id,governance_key_id,period,amount_microusd) VALUES($1,$2,$3,$4,$5,$6)").bind(layer).bind(kind.clone()).bind(ws).bind(key).bind(period.as_str()).bind(amount).execute(&mut **tx).await?;
    }
    Ok(())
}
impl Policy {
    /// `stored` is the existing layer; `default_period` is used by a legacy
    /// body without `budget_period` when no single stored budget exists.
    fn limits(&self, stored: &Limits, default_period: BudgetPeriod) -> Result<Limits, ApiError> {
        if [
            self.requests_per_minute,
            self.tokens_per_minute,
            self.concurrent_requests,
            self.concurrent_jobs.flatten(),
        ]
        .into_iter()
        .flatten()
        .any(|n| !positive_limit(n))
            || self
                .storage_bytes
                .flatten()
                .is_some_and(|n| !(1..=MAX_STORAGE_BYTES).contains(&n))
        {
            return Err(invalid());
        }
        let budgets = match (&self.budgets, &self.monthly_budget_microusd) {
            (Some(list), None) if self.budget_period.is_none() => parse_budgets(list)?,
            (None, Some(amount)) => {
                if stored.budgets.len() > 1 {
                    return Err(stacked_legacy());
                }
                let period = match &self.budget_period {
                    Some(p) => BudgetPeriod::parse(p).ok_or_else(invalid)?,
                    None => stored.legacy_period().unwrap_or(default_period),
                };
                amount
                    .as_deref()
                    .map(|n| money(n, true))
                    .transpose()?
                    .map(|a| Budgets::from([(period, a)]))
                    .unwrap_or_default()
            }
            _ => return Err(invalid()),
        };
        Ok(Limits {
            requests_per_minute: self.requests_per_minute,
            tokens_per_minute: self.tokens_per_minute,
            concurrent_requests: self.concurrent_requests,
            concurrent_jobs: self.concurrent_jobs.unwrap_or(stored.concurrent_jobs),
            storage_bytes: self.storage_bytes.unwrap_or(stored.storage_bytes),
            budgets,
        })
    }
}
pub(crate) fn json_budgets(b: &Budgets) -> Value {
    Value::Array(
        b.iter()
            .map(|(p, a)| json!({"period":p.as_str(),"amount_microusd":a.to_string()}))
            .collect(),
    )
}
pub(crate) fn json_limits(l: &Limits) -> Value {
    let legacy = l.legacy();
    json!({"requests_per_minute":l.requests_per_minute,"tokens_per_minute":l.tokens_per_minute,"concurrent_requests":l.concurrent_requests,"concurrent_jobs":l.concurrent_jobs,"storage_bytes":l.storage_bytes,"monthly_budget_microusd":legacy.map(|(_,a)|a.to_string()),"budget_period":legacy.map_or("month",|(p,_)|p.as_str()),"budgets":json_budgets(&l.budgets)})
}
/// Rate limits and same-period budgets take the minimum. Budgets of different
/// periods are all enforced independently.
pub(crate) fn compose(a: &Limits, b: &Limits) -> Limits {
    fn min(a: Option<i64>, b: Option<i64>) -> Option<i64> {
        match (a, b) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
    let mut budgets = a.budgets.clone();
    for (p, amount) in &b.budgets {
        budgets
            .entry(*p)
            .and_modify(|v| *v = (*v).min(*amount))
            .or_insert(*amount);
    }
    Limits {
        requests_per_minute: min(a.requests_per_minute, b.requests_per_minute),
        tokens_per_minute: min(a.tokens_per_minute, b.tokens_per_minute),
        concurrent_requests: min(a.concurrent_requests, b.concurrent_requests),
        concurrent_jobs: min(a.concurrent_jobs, b.concurrent_jobs),
        storage_bytes: min(a.storage_bytes, b.storage_bytes),
        budgets,
    }
}
/// Tighten-only validation for local/key layers.
///
/// Parents (`invalid`): a rate cap may not exceed any parent cap, and a budget
/// for period P may not exceed a parent's budget for the same period P. Budgets
/// of different periods are independent (each is enforced over its own
/// window, so a child can never loosen a parent).
///
/// Stored caps (`denied`): a stored rate cap or a stored budget for period P
/// can never be cleared or raised.
///
/// Rejections carry a stable `error.reason` and the period/limit name, never
/// an amount (see `POLICY_REASONS`).
fn validate_tighten(new: &Limits, parents: &[&Limits], old: &Limits) -> Result<(), ApiError> {
    const RATE_NAMES: [&str; 5] = [
        "requests_per_minute",
        "tokens_per_minute",
        "concurrent_requests",
        "concurrent_jobs",
        "storage_bytes",
    ];
    let bad = StatusCode::BAD_REQUEST;
    let forbidden = StatusCode::FORBIDDEN;
    for parent in parents {
        for ((n, p), name) in new.rates().into_iter().zip(parent.rates()).zip(RATE_NAMES) {
            if n.zip(p).is_some_and(|(n, p)| n > p) {
                return Err(policy_rejection(bad, "exceeds_parent_rate", name));
            }
        }
        if let Some((p, _)) = new
            .budgets
            .iter()
            .find(|(p, n)| parent.budgets.get(p).is_some_and(|pa| *n > pa))
        {
            return Err(policy_rejection(bad, "exceeds_parent_budget", p.as_str()));
        }
    }
    for ((o, n), name) in old.rates().into_iter().zip(new.rates()).zip(RATE_NAMES) {
        if o.is_some_and(|o| n.is_none_or(|n| n > o)) {
            return Err(policy_rejection(
                forbidden,
                "stored_rate_loosen_not_allowed",
                name,
            ));
        }
    }
    for (p, o) in &old.budgets {
        match new.budgets.get(p) {
            None => {
                return Err(policy_rejection(
                    forbidden,
                    "period_change_not_allowed",
                    p.as_str(),
                ));
            }
            Some(n) if n > o => {
                return Err(policy_rejection(
                    forbidden,
                    "stored_budget_raise_not_allowed",
                    p.as_str(),
                ));
            }
            Some(_) => {}
        }
    }
    Ok(())
}
/// Optional key policy supplied at key creation (absent fields inherit).
/// Validated exactly like the key policy PUT against an empty stored layer.
pub(crate) fn initial_key_limits(
    rates: [Option<i64>; 4],
    budgets: Option<&[BudgetInput]>,
) -> Result<Option<Limits>, ApiError> {
    if rates.into_iter().flatten().any(|n| !positive_limit(n)) {
        return Err(invalid());
    }
    let budgets = budgets.map(parse_budgets).transpose()?.unwrap_or_default();
    let [
        requests_per_minute,
        tokens_per_minute,
        concurrent_requests,
        concurrent_jobs,
    ] = rates;
    let l = Limits {
        requests_per_minute,
        tokens_per_minute,
        concurrent_requests,
        concurrent_jobs,
        storage_bytes: None,
        budgets,
    };
    Ok((l != Limits::default()).then_some(l))
}
/// Tighten-only check of a new key's limits under the workspace's platform
/// and local layers, then storage in the caller's transaction (atomic with
/// the key). Audited like the key policy PUT.
pub(crate) async fn check_initial_key_limits(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    l: &Limits,
) -> Result<(), ApiError> {
    let current = layers(tx, ws).await?;
    validate_tighten(l, &[&current.platform, &current.local], &Limits::default())
}
pub(crate) async fn store_initial_key_limits(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    ws: Uuid,
    lineage: Uuid,
    l: &Limits,
) -> Result<(), ApiError> {
    put_layer(tx, &Scope::Key(ws, lineage), l).await?;
    resources::audit(
        tx,
        u,
        Some(ws),
        "policy.key_updated",
        "key",
        Some(lineage),
        period_audit(l, &Limits::default()),
    )
    .await
}
fn parse_budgets(list: &[BudgetInput]) -> Result<Budgets, ApiError> {
    if list.len() > BudgetPeriod::ALL.len() {
        return Err(invalid());
    }
    let mut out = Budgets::new();
    for b in list {
        let period = BudgetPeriod::parse(&b.period).ok_or_else(invalid)?;
        if out
            .insert(period, money(&b.amount_microusd, true)?)
            .is_some()
        {
            return Err(invalid());
        }
    }
    Ok(out)
}
async fn get_rates<
    T: for<'q> sqlx::Encode<'q, Postgres> + sqlx::Type<Postgres> + Send + 'static,
>(
    tx: &mut Transaction<'_, Postgres>,
    table: &str,
    col: &str,
    id: T,
    storage: &str,
) -> Result<Limits, ApiError> {
    Ok(rates(
        sqlx::query_as(&format!(
            "SELECT {COLS},{storage} FROM {table} WHERE {col}=$1"
        ))
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?,
    ))
}
async fn get_layer(tx: &mut Transaction<'_, Postgres>, scope: &Scope) -> Result<Limits, ApiError> {
    let st = storage_col(scope);
    let mut l = match scope {
        Scope::Installation => {
            get_rates(tx, "installation_policy", "singleton", true, st).await?
        }
        Scope::Type(k) => {
            get_rates(tx, "workspace_type_policies", "kind", k.clone(), st).await?
        }
        Scope::Override(w) => {
            get_rates(
                tx,
                "workspace_platform_policy_overrides",
                "workspace_id",
                *w,
                st,
            )
            .await?
        }
        Scope::Local(w) => {
            get_rates(tx, "workspace_local_policies", "workspace_id", *w, st).await?
        }
        Scope::Key(w, k) => rates(
            sqlx::query_as(&format!(
                "SELECT {COLS},{st} FROM key_policies WHERE workspace_id=$1 AND governance_key_id=$2"
            ))
            .bind(w)
            .bind(k)
            .fetch_optional(&mut **tx)
            .await?,
        ),
    };
    l.budgets = load_budgets(tx, scope).await?;
    Ok(l)
}
async fn put_layer(
    tx: &mut Transaction<'_, Postgres>,
    scope: &Scope,
    l: &Limits,
) -> Result<(), ApiError> {
    let sets = "requests_per_minute=excluded.requests_per_minute,tokens_per_minute=excluded.tokens_per_minute,concurrent_requests=excluded.concurrent_requests,concurrent_jobs=excluded.concurrent_jobs";
    // Installation and key layers have no storage column; workspace layers
    // store it like the other limits.
    let q = |table: &str, col: &str| {
        format!(
            "INSERT INTO {table}({col},{COLS}) VALUES($1,$2,$3,$4,$5) ON CONFLICT({col}) DO UPDATE SET {sets}"
        )
    };
    let qs = |table: &str, col: &str| {
        format!(
            "INSERT INTO {table}({col},{COLS},storage_bytes) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT({col}) DO UPDATE SET {sets},storage_bytes=excluded.storage_bytes"
        )
    };
    fn rates<'q>(
        q: sqlx::query::Query<'q, Postgres, sqlx::postgres::PgArguments>,
        l: &Limits,
    ) -> sqlx::query::Query<'q, Postgres, sqlx::postgres::PgArguments> {
        q.bind(l.requests_per_minute)
            .bind(l.tokens_per_minute)
            .bind(l.concurrent_requests)
            .bind(l.concurrent_jobs)
    }
    match scope {
        Scope::Installation => {
            rates(
                sqlx::query(&q("installation_policy", "singleton")).bind(true),
                l,
            )
            .execute(&mut **tx)
            .await?;
        }
        Scope::Type(k) => {
            rates(
                sqlx::query(&qs("workspace_type_policies", "kind")).bind(k),
                l,
            )
            .bind(l.storage_bytes)
            .execute(&mut **tx)
            .await?;
        }
        Scope::Override(w) => {
            rates(
                sqlx::query(&qs("workspace_platform_policy_overrides", "workspace_id")).bind(w),
                l,
            )
            .bind(l.storage_bytes)
            .execute(&mut **tx)
            .await?;
        }
        Scope::Local(w) => {
            rates(
                sqlx::query(&qs("workspace_local_policies", "workspace_id")).bind(w),
                l,
            )
            .bind(l.storage_bytes)
            .execute(&mut **tx)
            .await?;
        }
        Scope::Key(w, k) => {
            rates(sqlx::query(&format!("INSERT INTO key_policies(workspace_id,governance_key_id,{COLS}) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(workspace_id,governance_key_id) DO UPDATE SET {sets}")).bind(w).bind(k), l)
                .execute(&mut **tx)
                .await?;
        }
    }
    store_budgets(tx, scope, &l.budgets).await
}
/// Typed, allowlisted audit metadata: the legacy new/previous budget periods and the budget count.
fn period_audit(new: &Limits, old: &Limits) -> Value {
    json!({"budget_period":new.legacy().map_or("month",|(p,_)|p.as_str()),"previous_budget_period":old.legacy().map_or("month",|(p,_)|p.as_str()),"count":new.budgets.len()})
}
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
pub(crate) struct Layers {
    pub(crate) platform: Limits,
    pub(crate) local: Limits,
    pub(crate) type_default: Limits,
    pub(crate) override_present: bool,
    pub(crate) source: &'static str,
    pub(crate) kind: String,
    pub(crate) created_at: DateTime<Utc>,
}
pub(crate) async fn layers(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
) -> Result<Layers, ApiError> {
    layers_with(tx, ws, false).await
}
/// [`layers`], optionally also for a disabled workspace. Only read-only
/// platform views pass `include_disabled`; mutations keep refusing them.
pub(crate) async fn layers_with(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    include_disabled: bool,
) -> Result<Layers, ApiError> {
    let (kind, created_at): (String, DateTime<Utc>) = sqlx::query_as(
        "SELECT kind,created_at FROM workspaces WHERE id=$1 AND ($2 OR disabled_at IS NULL)",
    )
    .bind(ws)
    .bind(include_disabled)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(missing)?;
    let over: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM workspace_platform_policy_overrides WHERE workspace_id=$1)",
    )
    .bind(ws)
    .fetch_one(&mut **tx)
    .await?;
    let type_default = get_layer(tx, &Scope::Type(kind.clone())).await?;
    let (platform, source) = if over {
        (
            get_layer(tx, &Scope::Override(ws)).await?,
            "workspace_override",
        )
    } else {
        (type_default.clone(), "type_default")
    };
    Ok(Layers {
        platform,
        local: get_layer(tx, &Scope::Local(ws)).await?,
        type_default,
        override_present: over,
        source,
        kind,
        created_at,
    })
}
pub(crate) async fn key_limits(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    key: Uuid,
) -> Result<Limits, ApiError> {
    get_layer(tx, &Scope::Key(ws, key)).await
}
pub(crate) async fn installation_limits(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<Limits, ApiError> {
    get_layer(tx, &Scope::Installation).await
}
/// One budget-window entry: `(layer, budgets, key lineage, usage visible, scope created_at)`.
pub(crate) type WindowEntry<'a> = (&'a str, &'a Budgets, Option<Uuid>, bool, DateTime<Utc>);
/// Every applicable budget (layer x period) with its own current UTC window.
/// Consumption (settled actual plus active holds by admission time) is
/// included only where the caller may see that scope's activity: workspace-wide
/// usage for platform readers and workspace administrators, key-lineage usage
/// to whoever may read that key's policy. Installation headroom is never included.
pub(crate) async fn budget_windows(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    entries: &[WindowEntry<'_>],
) -> Result<Vec<Value>, ApiError> {
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?;
    let mut out = Vec::new();
    for (layer, budgets, lineage, visible, created) in entries {
        for (period, amount) in budgets.iter() {
            let (start, end) = period.window(now);
            let (used, unresolved) = if *visible {
                let (used, unresolved) = budget_consumption(tx, ws, *lineage, *period, now).await?;
                (Some(used), Some(unresolved))
            } else {
                (None, None)
            };
            let exhausted = used
                .as_deref()
                .and_then(|u| u.parse::<i128>().ok())
                .map(|u| u >= i128::from(*amount));
            let lifetime = *period == BudgetPeriod::Lifetime;
            out.push(json!({"layer":layer,"period":period.as_str(),"amount_microusd":amount.to_string(),"monthly_budget_microusd":amount.to_string(),"budget_period":period.as_str(),"window_start":if lifetime {*created} else {start},"window_end":if lifetime {None} else {Some(end)},"usage_visible":visible,"used_microusd":used,"unresolved_usage":unresolved,"exhausted":exhausted}));
        }
    }
    Ok(out)
}
/// The workspace's storage quota and (when the caller may see workspace-wide
/// usage) the bytes counted against it: live files plus uploads in progress.
pub(crate) async fn storage_json(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    usage_visible: bool,
) -> Result<Value, ApiError> {
    let u = crate::filestore::files::workspace_storage(&mut **tx, ws)
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Management storage unavailable",
            )
        })?;
    Ok(
        json!({"quota_bytes":u.quota_bytes,"used_bytes":usage_visible.then_some(u.used_bytes),"usage_visible":usage_visible}),
    )
}
/// Creation time of a key lineage (its first credential).
pub(crate) async fn lineage_created(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    lineage: Uuid,
) -> Result<DateTime<Utc>, ApiError> {
    Ok(sqlx::query_scalar(
        "SELECT min(created_at) FROM api_keys WHERE workspace_id=$1 AND governance_key_id=$2",
    )
    .bind(ws)
    .bind(lineage)
    .fetch_one(&mut **tx)
    .await?)
}
/// The key lineage's primary (smallest) key-layer budget and its current use.
pub(crate) async fn key_usage(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    lineage: Uuid,
) -> Result<Value, ApiError> {
    let k = key_limits(tx, ws, lineage).await?;
    let (period, limit) = k
        .legacy()
        .map_or((BudgetPeriod::Month, None), |(p, a)| (p, Some(a)));
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?;
    let (start, end) = period.window(now);
    let (used, unresolved) = budget_consumption(tx, ws, Some(lineage), period, now).await?;
    let lifetime = period == BudgetPeriod::Lifetime;
    let start = if lifetime {
        lineage_created(tx, ws, lineage).await?
    } else {
        start
    };
    Ok(
        json!({"period":period.as_str(),"used_microusd":used,"limit_microusd":limit.map(|a|a.to_string()),"window_start":start,"window_end":if lifetime {None} else {Some(end)},"unresolved_usage":unresolved}),
    )
}
/// Platform, local and key-lineage budget windows for one key (key usage always visible).
pub(crate) async fn lineage_budget_windows(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    lineage: Uuid,
    workspace_usage: bool,
) -> Result<Vec<Value>, ApiError> {
    let l = layers(tx, ws).await?;
    let k = key_limits(tx, ws, lineage).await?;
    let created = lineage_created(tx, ws, lineage).await?;
    budget_windows(
        tx,
        ws,
        &[
            (
                "platform",
                &l.platform.budgets,
                None,
                workspace_usage,
                l.created_at,
            ),
            (
                "local",
                &l.local.budgets,
                None,
                workspace_usage,
                l.created_at,
            ),
            ("key", &k.budgets, Some(lineage), true, created),
        ],
    )
    .await
}
/// Workspace-wide platform and local budget windows (usage always included;
/// callers must hold workspace-wide visibility).
pub(crate) async fn workspace_budget_windows(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
) -> Result<Vec<Value>, ApiError> {
    let l = layers(tx, ws).await?;
    budget_windows(
        tx,
        ws,
        &[
            ("platform", &l.platform.budgets, None, true, l.created_at),
            ("local", &l.local.budgets, None, true, l.created_at),
        ],
    )
    .await
}
async fn response(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    key: Option<Uuid>,
    local_response: bool,
    workspace_usage: bool,
    platform: bool,
) -> Result<Value, ApiError> {
    let Layers {
        platform: p,
        local: l,
        type_default,
        source,
        created_at,
        ..
    } = layers_with(tx, ws, platform).await?;
    let k = match key {
        Some(k) => key_limits(tx, ws, k).await?,
        None => Limits::default(),
    };
    let key_created = match key {
        Some(k) => lineage_created(tx, ws, k).await?,
        None => created_at,
    };
    let mut entries: Vec<WindowEntry> = vec![
        ("platform", &p.budgets, None, workspace_usage, created_at),
        ("local", &l.budgets, None, workspace_usage, created_at),
    ];
    if key.is_some() {
        entries.push(("key", &k.budgets, key, true, key_created));
    }
    let budgets = budget_windows(tx, ws, &entries).await?;
    let storage = storage_json(tx, ws, workspace_usage).await?;
    let mut value = json!({"storage":storage,"policy":json_limits(if key.is_some(){&k}else if local_response{&l}else{&p}),"effective":json_limits(&compose(&compose(&p,&l),&k)),"mode":if source=="workspace_override"{"replace"}else{"inherit"},"provenance":{"platform_source":source,"platform":json_limits(&p),"local":json_limits(&l),"key":key.map(|_|json_limits(&k))},"budgets":budgets});
    // Platform readers also see the live type default behind an override.
    if platform {
        value["provenance"]["type_default"] = json_limits(&type_default);
    }
    Ok(value)
}
pub(super) async fn installation_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
) -> ApiResult {
    let mut tx = platform_tx(&s, &u, false).await?;
    let p = get_layer(&mut tx, &Scope::Installation).await?;
    tx.commit().await?;
    Ok(Json(json!({"policy":json_limits(&p)})))
}
pub(super) async fn put_installation_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(p): Json<Policy>,
) -> ApiResult {
    if p.storage_bytes.flatten().is_some() {
        return Err(invalid());
    }
    let mut tx = platform_tx(&s, &u, true).await?;
    let old = get_layer(&mut tx, &Scope::Installation).await?;
    let l = p.limits(&old, BudgetPeriod::Month)?;
    put_layer(&mut tx, &Scope::Installation, &l).await?;
    resources::audit(
        &mut tx,
        &u,
        None,
        "policy.installation_updated",
        "installation",
        None,
        period_audit(&l, &old),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
fn kind(k: &str) -> Result<(), ApiError> {
    if matches!(k, "personal" | "team" | "project") {
        Ok(())
    } else {
        Err(invalid())
    }
}
pub(super) async fn type_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(k): Path<String>,
) -> ApiResult {
    kind(&k)?;
    let mut tx = platform_tx(&s, &u, false).await?;
    let p = get_layer(&mut tx, &Scope::Type(k)).await?;
    tx.commit().await?;
    Ok(Json(json!({"policy":json_limits(&p)})))
}
pub(super) async fn put_type_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(k): Path<String>,
    Json(p): Json<Policy>,
) -> ApiResult {
    kind(&k)?;
    let mut tx = platform_tx(&s, &u, true).await?;
    let scope = Scope::Type(k.clone());
    let old = get_layer(&mut tx, &scope).await?;
    let l = p.limits(&old, BudgetPeriod::Month)?;
    put_layer(&mut tx, &scope, &l).await?;
    let mut metadata = period_audit(&l, &old);
    metadata["kind"] = json!(k);
    resources::audit(
        &mut tx,
        &u,
        None,
        "policy.type_updated",
        "workspace_type",
        None,
        metadata,
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
pub(super) async fn platform_workspace_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
) -> ApiResult {
    let mut tx = platform_tx(&s, &u, false).await?;
    // Platform readers may see workspace usage/cost totals (never request detail).
    let value = response(&mut tx, ws, None, false, true, true).await?;
    tx.commit().await?;
    Ok(Json(value))
}
pub(super) async fn put_platform_workspace_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(p): Json<Policy>,
) -> ApiResult {
    let mut tx = platform_tx(&s, &u, true).await?;
    // A new override starts from the layer it replaces (legacy single budget period).
    let current = layers(&mut tx, ws).await?;
    let l = p.limits(
        &current.platform,
        current
            .platform
            .legacy()
            .map_or(BudgetPeriod::Month, |(p, _)| p),
    )?;
    put_layer(&mut tx, &Scope::Override(ws), &l).await?;
    resources::audit(
        &mut tx,
        &u,
        Some(ws),
        "policy.override_updated",
        "workspace",
        Some(ws),
        period_audit(&l, &current.platform),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
pub(super) async fn reset_platform_workspace_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
) -> ApiResult {
    let mut tx = platform_tx(&s, &u, true).await?;
    layers(&mut tx, ws).await?;
    store_budgets(&mut tx, &Scope::Override(ws), &Budgets::new()).await?;
    sqlx::query("DELETE FROM workspace_platform_policy_overrides WHERE workspace_id=$1")
        .bind(ws)
        .execute(&mut *tx)
        .await?;
    resources::audit(
        &mut tx,
        &u,
        Some(ws),
        "policy.override_reset",
        "workspace",
        Some(ws),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
pub(super) async fn workspace_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    let value = response(&mut tx, ws, None, true, a.view_all_activity, false).await?;
    tx.commit().await?;
    Ok(Json(value))
}
pub(super) async fn put_workspace_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Json(p): Json<Policy>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    // Personal limits are platform-controlled (type default, platform override,
    // installation); per-key caps remain available on each key.
    if a.kind == "personal" {
        return Err(personal_limits());
    }
    if !a.admin && !a.owner {
        return Err(denied());
    }
    let current = layers(&mut tx, ws).await?;
    let l = p.limits(&current.local, BudgetPeriod::Month)?;
    validate_tighten(&l, &[&current.platform], &current.local)?;
    put_layer(&mut tx, &Scope::Local(ws), &l).await?;
    resources::audit(
        &mut tx,
        &u,
        Some(ws),
        "policy.local_updated",
        "workspace",
        Some(ws),
        period_audit(&l, &current.local),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
pub(crate) async fn lineage(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    ws: Uuid,
    key: Uuid,
    all: bool,
) -> Result<Uuid, ApiError> {
    sqlx::query_scalar("SELECT governance_key_id FROM api_keys WHERE workspace_id=$1 AND id=$2 AND ($3 OR issued_to_user_id=$4)").bind(ws).bind(key).bind(all).bind(u.user_id).fetch_optional(&mut **tx).await?.ok_or_else(missing)
}
pub(super) async fn key_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, key)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    let key = lineage(&mut tx, &u, ws, key, a.admin || a.owner).await?;
    let value = response(&mut tx, ws, Some(key), true, a.view_all_activity, false).await?;
    tx.commit().await?;
    Ok(Json(value))
}
pub(super) async fn put_key_policy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, key)): Path<(Uuid, Uuid)>,
    Json(p): Json<Policy>,
) -> ApiResult {
    // Storage is a workspace quota; keys have no storage layer.
    if p.storage_bytes.flatten().is_some() {
        return Err(invalid());
    }
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    let key = lineage(&mut tx, &u, ws, key, a.admin || a.owner).await?;
    let current = layers(&mut tx, ws).await?;
    let old = key_limits(&mut tx, ws, key).await?;
    let l = p.limits(&old, BudgetPeriod::Month)?;
    validate_tighten(&l, &[&current.platform, &current.local], &old)?;
    put_layer(&mut tx, &Scope::Key(ws, key), &l).await?;
    resources::audit(
        &mut tx,
        &u,
        Some(ws),
        "policy.key_updated",
        "key",
        Some(key),
        period_audit(&l, &old),
    )
    .await?;
    tx.commit().await?;
    Ok(ok())
}
#[cfg(test)]
mod tests {
    use super::*;
    use BudgetPeriod::{Day, Lifetime, Month, Week};
    fn code(r: Result<(), ApiError>) -> Option<StatusCode> {
        r.err().map(|e| e.0)
    }
    fn rpm(n: i64) -> Limits {
        Limits {
            requests_per_minute: Some(n),
            ..Limits::default()
        }
    }
    fn budgets(list: &[(BudgetPeriod, i64)]) -> Limits {
        Limits {
            budgets: list.iter().copied().collect(),
            ..Limits::default()
        }
    }
    fn empty() -> Limits {
        Limits::default()
    }
    #[test]
    fn masked_caps_cannot_be_erased() {
        assert!(validate_tighten(&rpm(11), &[&rpm(5)], &rpm(10)).is_err());
        assert!(validate_tighten(&empty(), &[&rpm(5)], &rpm(10)).is_err());
        assert!(validate_tighten(&rpm(4), &[&rpm(5)], &rpm(10)).is_ok());
        assert_eq!(compose(&rpm(5), &empty()), rpm(5));
    }
    fn jobs(n: i64) -> Limits {
        Limits {
            concurrent_jobs: Some(n),
            ..Limits::default()
        }
    }
    #[test]
    fn jobs_at_once_is_a_tighten_only_composed_limit() {
        // A child may not exceed a parent, nor raise or remove a stored cap.
        let e = validate_tighten(&jobs(3), &[&jobs(2)], &empty()).unwrap_err();
        assert_eq!(
            (e.0, e.1),
            (
                StatusCode::BAD_REQUEST,
                "concurrent_jobs exceeds a parent limit"
            )
        );
        for new in [jobs(3), empty()] {
            let e = validate_tighten(&new, &[], &jobs(2)).unwrap_err();
            assert_eq!(
                (e.0, e.1),
                (
                    StatusCode::FORBIDDEN,
                    "A stored concurrent_jobs cap cannot be raised or removed"
                )
            );
        }
        assert!(validate_tighten(&jobs(1), &[&jobs(2)], &jobs(2)).is_ok());
        // Absent child limits inherit; composition takes the minimum.
        assert_eq!(compose(&jobs(2), &empty()), jobs(2));
        assert_eq!(compose(&jobs(2), &jobs(1)), jobs(1));
        assert_eq!(json_limits(&jobs(2))["concurrent_jobs"], 2);
        assert_eq!(json_limits(&empty())["concurrent_jobs"], Value::Null);
        // Older clients omit the field: the stored value is kept. Null clears;
        // zero and negatives are invalid.
        let stored = jobs(4);
        let keep = body(json!({"budgets":[]})).unwrap();
        assert_eq!(
            keep.limits(&stored, Month).unwrap().concurrent_jobs,
            Some(4)
        );
        let clear = body(json!({"budgets":[],"concurrent_jobs":null})).unwrap();
        assert_eq!(clear.limits(&stored, Month).unwrap().concurrent_jobs, None);
        let set = body(json!({"budgets":[],"concurrent_jobs":3})).unwrap();
        assert_eq!(set.limits(&stored, Month).unwrap().concurrent_jobs, Some(3));
        for bad in [0, -1] {
            let p = body(json!({"budgets":[],"concurrent_jobs":bad})).unwrap();
            assert!(p.limits(&stored, Month).is_err());
        }
        assert!(
            initial_key_limits([None, None, None, Some(0)], None).is_err()
                && initial_key_limits([None, None, None, Some(1)], None)
                    .unwrap()
                    .is_some_and(|l| l == jobs(1))
        );
    }
    #[test]
    fn budget_tightening_is_per_period() {
        // Same period: a larger child amount is rejected.
        assert_eq!(
            code(validate_tighten(
                &budgets(&[(Month, 11)]),
                &[&budgets(&[(Month, 10)])],
                &empty()
            )),
            Some(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            code(validate_tighten(
                &budgets(&[(Lifetime, 11)]),
                &[&budgets(&[(Lifetime, 10)])],
                &empty()
            )),
            Some(StatusCode::BAD_REQUEST)
        );
        // Different periods are independent (a day larger than a week, too).
        assert!(
            validate_tighten(&budgets(&[(Day, 11)]), &[&budgets(&[(Week, 10)])], &empty()).is_ok()
        );
        assert!(
            validate_tighten(
                &budgets(&[(Month, 50)]),
                &[&budgets(&[(Day, 10)])],
                &empty()
            )
            .is_ok()
        );
        // Each parent is checked independently (key under platform + local).
        assert_eq!(
            code(validate_tighten(
                &budgets(&[(Day, 8)]),
                &[&budgets(&[(Month, 100)]), &budgets(&[(Day, 7)])],
                &empty()
            )),
            Some(StatusCode::BAD_REQUEST)
        );
        // Stacked: every period checked against the same period only.
        assert!(
            validate_tighten(
                &budgets(&[(Day, 5), (Month, 100)]),
                &[&budgets(&[(Day, 5), (Week, 1), (Month, 100)])],
                &empty()
            )
            .is_ok()
        );
        // Stored budgets may be kept/lowered and new periods added.
        assert!(
            validate_tighten(
                &budgets(&[(Day, 9), (Week, 30)]),
                &[],
                &budgets(&[(Day, 10)])
            )
            .is_ok()
        );
        // Raising or removing a stored period loosens it.
        for (new, old) in [
            (budgets(&[(Day, 11)]), budgets(&[(Day, 10)])),
            (budgets(&[(Week, 10)]), budgets(&[(Day, 10)])),
            (empty(), budgets(&[(Lifetime, 10)])),
            (budgets(&[(Day, 1)]), budgets(&[(Day, 1), (Month, 5)])),
        ] {
            assert_eq!(
                code(validate_tighten(&new, &[], &old)),
                Some(StatusCode::FORBIDDEN),
                "{new:?} {old:?}"
            );
        }
    }
    #[test]
    fn composed_budgets_take_per_period_minimum_and_legacy_mirror_is_smallest() {
        let c = compose(
            &budgets(&[(Month, 10), (Day, 3)]),
            &budgets(&[(Month, 5), (Week, 4)]),
        );
        assert_eq!(c, budgets(&[(Day, 3), (Week, 4), (Month, 5)]));
        assert_eq!(c.legacy(), Some((Day, 3)));
        assert_eq!(budgets(&[(Month, 5), (Day, 5)]).legacy(), Some((Day, 5)));
        assert_eq!(empty().legacy(), None);
        let v = json_limits(&c);
        assert_eq!(v["monthly_budget_microusd"], "3");
        assert_eq!(v["budget_period"], "day");
        assert_eq!(
            v["budgets"][2],
            json!({"period":"month","amount_microusd":"5"})
        );
        assert_eq!(json_limits(&empty())["budget_period"], "month");
    }
    fn body(extra: Value) -> Result<Policy, serde_json::Error> {
        let mut v =
            json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null});
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value::<Policy>(v)
    }
    #[test]
    fn legacy_body_period_is_optional_strict_and_defaults_to_stored() {
        let legacy = |extra: Value| {
            let mut v = json!({"monthly_budget_microusd":"5"});
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            body(v)
        };
        let week = budgets(&[(Week, 9)]);
        assert_eq!(
            legacy(json!({})).unwrap().limits(&week, Month).unwrap(),
            budgets(&[(Week, 5)])
        );
        assert_eq!(
            legacy(json!({})).unwrap().limits(&empty(), Month).unwrap(),
            budgets(&[(Month, 5)])
        );
        assert_eq!(
            legacy(json!({"budget_period":"day"}))
                .unwrap()
                .limits(&empty(), Month)
                .unwrap(),
            budgets(&[(Day, 5)])
        );
        assert!(legacy(json!({"budget_period":null})).is_err());
        assert!(legacy(json!({"budget_period":1})).is_err());
        for bad in ["year", "Day", "", "monthly"] {
            assert!(
                legacy(json!({ "budget_period": bad }))
                    .unwrap()
                    .limits(&empty(), Month)
                    .is_err()
            );
        }
        // Legacy body cannot silently drop stacked budgets.
        let e = legacy(json!({}))
            .unwrap()
            .limits(&budgets(&[(Day, 1), (Month, 2)]), Month)
            .unwrap_err();
        assert_eq!(e.0, StatusCode::CONFLICT);
        // Legacy null clears (subject to tighten-only rules elsewhere).
        assert_eq!(
            body(json!({"monthly_budget_microusd":null}))
                .unwrap()
                .limits(&week, Month)
                .unwrap(),
            empty()
        );
        // One of the two budget forms is required.
        assert!(body(json!({})).unwrap().limits(&empty(), Month).is_err());
    }
    #[test]
    fn stacked_body_is_strict() {
        let ok = body(json!({"budgets":[{"period":"day","amount_microusd":"1"},{"period":"lifetime","amount_microusd":"9"}]}))
            .unwrap()
            .limits(&empty(), Month)
            .unwrap();
        assert_eq!(ok, budgets(&[(Day, 1), (Lifetime, 9)]));
        assert_eq!(
            body(json!({"budgets":[]}))
                .unwrap()
                .limits(&budgets(&[(Day, 1), (Month, 2)]), Month)
                .unwrap(),
            empty()
        );
        for bad in [
            json!({"budgets":[{"period":"day","amount_microusd":"1"},{"period":"day","amount_microusd":"2"}]}),
            json!({"budgets":[{"period":"day","amount_microusd":"0"}]}),
            json!({"budgets":[{"period":"year","amount_microusd":"1"}]}),
            json!({"budgets":[],"monthly_budget_microusd":null}),
            json!({"budgets":[],"budget_period":"day"}),
        ] {
            assert!(
                body(bad.clone())
                    .ok()
                    .and_then(|p| p.limits(&empty(), Month).ok())
                    .is_none(),
                "{bad}"
            );
        }
        assert!(body(json!({"budgets":null})).is_err());
        assert!(body(json!({"budgets":[{"period":"day","amount_microusd":"1","x":1}]})).is_err());
    }
}
