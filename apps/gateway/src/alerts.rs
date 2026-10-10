//! Alerts (docs/alerts.md): a bounded background evaluator turns alert rules
//! into incidents (`alert_events`) and emails recipients through the
//! installation's SMTP relay.
//!
//! - Rule kinds: budget thresholds (% of any applicable stacked workspace or
//!   key budget), installation spend thresholds (% of a reference amount per
//!   period; never blocks anything: there are no installation-wide limits),
//!   spend spikes (last hour vs the trailing 7-day hourly average), error rate
//!   and failing provider connections. Personal workspaces get built-in budget
//!   alerts for their owner only.
//! - Exact integer micro-USD math. Spend is settled actual plus in-flight
//!   pending holds (as admission counts them); unknown-cost reservations are
//!   never counted as zero or as spend: they raise a separate flag/count.
//! - Idempotent: at most one open incident per rule and subject (a partial
//!   unique index), so repeated evaluation never fires twice. An incident
//!   resolves once when its condition clears; a changed threshold level
//!   supersedes it with a new one.
//! - One replica evaluates at a time (transaction advisory lock); no
//!   installation lock is taken, so admission is never blocked. Each rule runs
//!   in its own savepoint: one failing rule cannot resolve or block others.
//! - Email is sent after commit, never under a lock on shared state; outcomes
//!   are recorded as counts and a category. Failures never crash the loop.
//! - No prompt data, key names or owner identities appear in incidents.
use std::time::Duration;

use chrono::{DateTime, DurationRound, TimeDelta, Utc};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::{
    email::{OutgoingEmail, SmtpSettings, TlsMode},
    governance::BudgetPeriod,
    store::Store,
};

pub const INTERVAL_VAR: &str = "GATEWAY_ALERT_INTERVAL_SECONDS";
pub const DEFAULT_INTERVAL_SECONDS: u64 = 60;
/// Transaction advisory lock: one evaluating replica at a time.
const EVALUATION_LOCK: i64 = 72419507;
/// Built-in personal budget alert thresholds (owner only).
pub const PERSONAL_THRESHOLDS: [i32; 2] = [80, 100];
pub const PERSONAL_BUILTIN: &str = "personal_budget";
/// Built-in installation incident (0018): SCIM tried to remove platform
/// access from the last active Platform Admin and was refused. Also its kind
/// and subject: at most one is open at a time.
pub const SCIM_LAST_ADMIN: &str = "scim_last_admin";
pub const SCIM_LAST_ADMIN_SUMMARY: &str = "SCIM tried to remove the last Platform Admin";
/// Built-in installation incident: a history table has fewer than
/// `crate::partitions::ALERT_BELOW_MONTHS` future month partitions (0030).
pub const PARTITIONS_MISSING: &str = "partitions_missing";
/// Built-in installation incident: `history verify` found history rows whose
/// parents are missing or belong to another scope (0034).
pub const HISTORY_ORPHANS: &str = "history_orphans";
/// Built-in installation incidents (no rule; Platform Admins are notified).
pub const INSTALLATION_BUILTINS: [&str; 3] = [SCIM_LAST_ADMIN, PARTITIONS_MISSING, HISTORY_ORPHANS];
/// Attempt failures that indicate the upstream (not the request) is failing.
pub const UPSTREAM_FAILURES: [&str; 4] = [
    "upstream_unavailable",
    "timeout_error",
    "invalid_upstream_response",
    "provider_configuration_error",
];
/// The spend-spike baseline: the 7 days (168 hours) before the last hour.
pub const BASELINE_HOURS: i128 = 168;
/// Evaluation output bounds (per rule) and delivery bounds (per tick).
const MAX_CONDITIONS: usize = 5000;
const MAX_RULES: i64 = 2000;
const DELIVERIES_PER_TICK: i64 = 25;
const MAX_RECIPIENTS: i64 = 50;

/// `GATEWAY_ALERT_INTERVAL_SECONDS`: default 60, `0` disables, at most 86400.
pub fn interval_from_env() -> anyhow::Result<Option<Duration>> {
    parse_interval(std::env::var(INTERVAL_VAR).ok().as_deref())
}
pub fn parse_interval(value: Option<&str>) -> anyhow::Result<Option<Duration>> {
    let Some(value) = value else {
        return Ok(Some(Duration::from_secs(DEFAULT_INTERVAL_SECONDS)));
    };
    let seconds: u64 = value
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid {INTERVAL_VAR}"))?;
    anyhow::ensure!(seconds <= 86_400, "{INTERVAL_VAR} must be 0..86400");
    Ok((seconds > 0).then(|| Duration::from_secs(seconds)))
}

// ---------- Exact arithmetic (unit-tested) ----------

/// Highest threshold reached (`used/amount >= t/100`), exactly. Zero budgets
/// (deny-all) never alert.
pub fn threshold_level(used: i128, amount: i128, thresholds: &[i32]) -> Option<i32> {
    if amount <= 0 {
        return None;
    }
    thresholds
        .iter()
        .copied()
        .filter(|t| used * 100 >= amount * i128::from(*t))
        .max()
}
/// Whole percent used, rounded down.
pub fn percent_floor(used: i128, amount: i128) -> Option<i128> {
    (amount > 0).then(|| used.max(0) * 100 / amount)
}
/// Last-hour spend reaches `factor_percent/100` times the trailing hourly
/// average (`baseline_total / 168`) and the absolute floor.
pub fn spike_firing(
    current: i128,
    baseline_total: i128,
    factor_percent: i64,
    min_spend: i64,
) -> bool {
    current >= i128::from(min_spend)
        && current * BASELINE_HOURS * 100 >= baseline_total * i128::from(factor_percent)
}
/// `failed/finished >= rate%` with at least `min_requests` finished attempts.
pub fn rate_firing(failed: i64, finished: i64, rate_percent: i32, min_requests: i32) -> bool {
    finished > 0
        && finished >= i64::from(min_requests)
        && i128::from(failed) * 100 >= i128::from(finished) * i128::from(rate_percent)
}
/// Exact dollars: at least two decimals, at most six, never rounded.
pub fn usd(micro: i128) -> String {
    let micro = micro.max(0);
    let mut fraction = format!("{:06}", micro % 1_000_000);
    while fraction.len() > 2 && fraction.ends_with('0') {
        fraction.pop();
    }
    format!("${}.{fraction}", micro / 1_000_000)
}
/// "4.2" (tenths, rounded down) for last hour / hourly average, if any baseline.
pub fn spike_ratio(current: i128, baseline_total: i128) -> Option<String> {
    (baseline_total > 0).then(|| {
        let tenths = current.max(0) * BASELINE_HOURS * 10 / baseline_total;
        format!("{}.{}", tenths / 10, tenths % 10)
    })
}

// ---------- Rules ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Budget,
    Spike,
    ErrorRate,
    Provider,
    /// A batch ended failed or expired (0021).
    BatchFailed,
    /// An unfinished batch made no progress for `window_minutes` (0021).
    BatchStalled,
    /// Installation-wide spend in a period reaches a share of a reference
    /// amount (0026). A notification only: nothing is ever denied by it.
    Spend,
    /// One workspace or key lineage near its scoped-admission ceiling:
    /// sustained admission rate or lock-wait p95 (0036; installation only).
    AdmissionCeiling,
}
impl Kind {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "budget_threshold" => Some(Self::Budget),
            "spend_spike" => Some(Self::Spike),
            "error_rate" => Some(Self::ErrorRate),
            "provider_failing" => Some(Self::Provider),
            "batch_failed" => Some(Self::BatchFailed),
            "batch_stalled" => Some(Self::BatchStalled),
            "spend_threshold" => Some(Self::Spend),
            "admission_ceiling" => Some(Self::AdmissionCeiling),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Budget => "budget_threshold",
            Self::Spike => "spend_spike",
            Self::ErrorRate => "error_rate",
            Self::Provider => "provider_failing",
            Self::BatchFailed => "batch_failed",
            Self::BatchStalled => "batch_stalled",
            Self::Spend => "spend_threshold",
            Self::AdmissionCeiling => "admission_ceiling",
        }
    }
}
/// Stacked-budget layers a budget rule can watch. There is no installation
/// layer (0026); installation spend has its own rule kind ([`Kind::Spend`]).
pub const BUDGET_LAYERS: [&str; 4] = ["type", "override", "local", "key"];
fn layer_label(layer: &str) -> &'static str {
    match layer {
        "type" => "Type default",
        "override" => "Platform override",
        "local" => "Workspace",
        _ => "API key",
    }
}
fn period_word(period: &str) -> &'static str {
    match period {
        "day" => "daily",
        "week" => "weekly",
        "month" => "monthly",
        _ => "lifetime",
    }
}

#[derive(sqlx::FromRow, Clone, Debug)]
struct Rule {
    id: Uuid,
    workspace_id: Option<Uuid>,
    kind: String,
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
}

/// One firing condition produced by evaluation.
#[derive(Clone, Debug)]
pub(crate) struct Condition {
    pub(crate) subject: String,
    pub(crate) level: i32,
    pub(crate) critical: bool,
    pub(crate) workspace_id: Option<Uuid>,
    pub(crate) connection_id: Option<Uuid>,
    pub(crate) summary: String,
    pub(crate) details: Value,
}

#[derive(Clone, Copy)]
enum Source {
    Rule(Uuid),
    Builtin,
}
impl Source {
    fn rule(self) -> Option<Uuid> {
        match self {
            Self::Rule(id) => Some(id),
            Self::Builtin => None,
        }
    }
    fn builtin(self) -> Option<&'static str> {
        match self {
            Self::Rule(_) => None,
            Self::Builtin => Some(PERSONAL_BUILTIN),
        }
    }
}

/// Which workspaces a budget evaluation covers.
#[derive(Clone, Copy)]
enum Workspaces {
    /// Every enabled Team/Project (installation rules).
    Shared,
    /// Every enabled personal workspace (built-in alerts).
    Personal,
    One(Uuid),
}

// ---------- Evaluation ----------

type BudgetRow = (Uuid, String, Option<Uuid>, String, i64, String, i64);

const WORKSPACE_BUDGETS: &str = r#"WITH ws AS (SELECT id,kind FROM workspaces WHERE disabled_at IS NULL AND CASE $1::text WHEN 'shared' THEN kind IN ('team','project') WHEN 'personal' THEN kind='personal' ELSE id=$2 AND kind IN ('team','project') END),
b AS (
 SELECT ws.id workspace_id,'type'::text layer,NULL::uuid lineage,p.period,p.amount_microusd FROM ws JOIN policy_budgets p ON p.layer='type' AND p.kind=ws.kind WHERE NOT EXISTS(SELECT 1 FROM workspace_platform_policy_overrides o WHERE o.workspace_id=ws.id)
 UNION ALL SELECT ws.id,'override',NULL,p.period,p.amount_microusd FROM ws JOIN workspace_platform_policy_overrides o ON o.workspace_id=ws.id JOIN policy_budgets p ON p.layer='override' AND p.workspace_id=ws.id
 UNION ALL SELECT ws.id,'local',NULL,p.period,p.amount_microusd FROM ws JOIN policy_budgets p ON p.layer='local' AND p.workspace_id=ws.id
 UNION ALL SELECT ws.id,'key',p.governance_key_id,p.period,p.amount_microusd FROM ws JOIN policy_budgets p ON p.layer='key' AND p.workspace_id=ws.id WHERE EXISTS(SELECT 1 FROM api_keys k WHERE k.workspace_id=ws.id AND k.governance_key_id=p.governance_key_id AND k.revoked_at IS NULL)
),
win AS (SELECT * FROM (VALUES ('day',$4::timestamptz),('week',$5::timestamptz),('month',$6::timestamptz),('lifetime',$7::timestamptz)) v(period,start_at))
SELECT b.workspace_id,b.layer,b.lineage,b.period,b.amount_microusd,
 coalesce(t.settled_microusd+t.held_microusd-t.held_unknown_microusd,0)::text,
 coalesce(t.unknown+t.unresolved-t.unresolved_unknown,0)::bigint
FROM b JOIN win ON win.period=b.period
LEFT JOIN budget_totals t ON t.scope_kind=CASE WHEN b.lineage IS NULL THEN 'workspace' ELSE 'key' END
 AND t.scope_id=coalesce(b.lineage,b.workspace_id) AND t.period=b.period AND t.period_start=win.start_at
WHERE b.amount_microusd>0 AND b.layer=ANY($3)
ORDER BY b.workspace_id,b.layer,b.lineage,b.period LIMIT 5001"#;

fn windows(now: DateTime<Utc>) -> [(DateTime<Utc>, DateTime<Utc>); 4] {
    BudgetPeriod::ALL.map(|p| p.window(now))
}

#[allow(clippy::too_many_arguments)]
fn budget_condition(
    subject: String,
    workspace_id: Option<Uuid>,
    layer: &str,
    period: &str,
    amount: i64,
    used: i128,
    unknown: i64,
    thresholds: &[i32],
    now: DateTime<Utc>,
) -> Option<Condition> {
    let level = threshold_level(used, i128::from(amount), thresholds)?;
    let (start, end) = BudgetPeriod::parse(period)?.window(now);
    let lifetime = period == "lifetime";
    Some(Condition {
        subject,
        level,
        critical: level >= 100,
        workspace_id,
        connection_id: None,
        summary: format!(
            "{} {} budget reached {level}%",
            layer_label(layer),
            period_word(period)
        ),
        details: json!({
            "layer": layer,
            "period": period,
            "threshold_percent": level,
            "used_percent": percent_floor(used, i128::from(amount)).map(|p| p.to_string()),
            "used_microusd": used.to_string(),
            "budget_microusd": amount.to_string(),
            "unknown_cost_requests": unknown,
            "window_start": (!lifetime).then_some(start),
            "window_end": (!lifetime).then_some(end),
        }),
    })
}

async fn budget_conditions(
    tx: &mut Transaction<'_, Postgres>,
    set: Workspaces,
    layers: &[String],
    thresholds: &[i32],
    now: DateTime<Utc>,
) -> Result<Vec<Condition>, sqlx::Error> {
    let w = windows(now);
    let mut out = Vec::new();
    let (mode, one) = match set {
        Workspaces::Shared => ("shared", None),
        Workspaces::Personal => ("personal", None),
        Workspaces::One(id) => ("one", Some(id)),
    };
    let rows: Vec<BudgetRow> = sqlx::query_as(WORKSPACE_BUDGETS)
        .bind(mode)
        .bind(one)
        .bind(layers)
        .bind(w[0].0)
        .bind(w[1].0)
        .bind(w[2].0)
        .bind(w[3].0)
        .fetch_all(&mut **tx)
        .await?;
    if rows.len() > MAX_CONDITIONS {
        tracing::warn!("alert budget evaluation truncated");
    }
    for (workspace, layer, lineage, period, amount, used, unknown) in
        rows.into_iter().take(MAX_CONDITIONS)
    {
        let used: i128 = used.parse().unwrap_or(0);
        let subject = format!(
            "{layer}:{workspace}:{}:{period}",
            lineage.map_or_else(|| "-".to_owned(), |l| l.to_string())
        );
        out.extend(budget_condition(
            subject,
            Some(workspace),
            &layer,
            &period,
            amount,
            used,
            unknown,
            thresholds,
            now,
        ));
    }
    Ok(out)
}

/// Installation spend threshold (0026): installation-wide spend in the
/// current UTC window of the rule's period (settled actual plus pending holds,
/// summed over every workspace's maintained totals, exact integer micro-USD)
/// against the rule's reference amount. Unknown-cost requests are reported
/// in the details, never counted as zero spend. The subject is the former
/// installation-budget subject, so a rule converted in place by 0026 keeps
/// its open incident.
async fn spend_conditions(
    tx: &mut Transaction<'_, Postgres>,
    rule: &Rule,
    now: DateTime<Utc>,
) -> Result<Vec<Condition>, sqlx::Error> {
    let (Some(period), Some(amount), Some(thresholds)) = (
        rule.spend_period.as_deref().and_then(BudgetPeriod::parse),
        rule.spend_amount_microusd,
        rule.thresholds.as_deref(),
    ) else {
        return Ok(vec![]);
    };
    let (start, end) = period.window(now);
    let (used, pending, unknown): (String, String, i64) =
        sqlx::query_as(crate::governance::totals::INSTALLATION_SPEND)
            .bind(period.as_str())
            .bind(period.bucket_start(now))
            .fetch_one(&mut **tx)
            .await?;
    let used: i128 = used.parse().unwrap_or(0);
    let Some(level) = threshold_level(used, i128::from(amount), thresholds) else {
        return Ok(vec![]);
    };
    let lifetime = period == BudgetPeriod::Lifetime;
    Ok(vec![Condition {
        subject: format!("installation:{}", period.as_str()),
        level,
        critical: level >= 100,
        workspace_id: None,
        connection_id: None,
        summary: format!(
            "Installation {} spend reached {level}%",
            period_word(period.as_str())
        ),
        details: json!({
            "period": period.as_str(),
            "threshold_percent": level,
            "used_percent": percent_floor(used, i128::from(amount)).map(|p| p.to_string()),
            "used_microusd": used.to_string(),
            "pending_microusd": pending,
            "spend_amount_microusd": amount.to_string(),
            "unknown_cost_requests": unknown,
            "window_start": (!lifetime).then_some(start),
            "window_end": (!lifetime).then_some(end),
        }),
    }])
}

/// Settled spend of the last hour ($2..$3) and of the baseline ($1..$2), and
/// the last hour's unknown-cost attempts, optionally of one workspace ($4).
/// The 7-day baseline reads hourly rollups (exact: `omg_usage_rows` uses an
/// hour's rollup only when it matches raw history in this snapshot).
pub(crate) const SPIKE: &str = "SELECT coalesce(sum(known_cost_microusd) FILTER(WHERE w='current' AND accounting_state='settled'),0)::text,coalesce(sum(known_cost_microusd) FILTER(WHERE w='baseline' AND accounting_state='settled'),0)::text,coalesce(sum(attempts) FILTER(WHERE w='current' AND accounting_state='unknown'),0)::bigint FROM (SELECT 'baseline' w,u.* FROM omg_usage_rows($1,$2) u UNION ALL SELECT 'current',u.* FROM omg_usage_rows($2,$3) u) x WHERE ($4::uuid IS NULL OR workspace_id=$4)";
/// The former raw-history spike query (parity oracle for [`SPIKE`]).
#[cfg(test)]
pub(crate) const SPIKE_RAW: &str = "SELECT coalesce(sum(actual_microusd) FILTER(WHERE admitted_at>=$2 AND state='settled'),0)::text,coalesce(sum(actual_microusd) FILTER(WHERE admitted_at<$2 AND state='settled'),0)::text,count(*) FILTER(WHERE admitted_at>=$2 AND state='unknown') FROM governance_reservations WHERE admitted_at>=$1 AND admitted_at<$3 AND ($4::uuid IS NULL OR workspace_id=$4)";

async fn spike_conditions(
    tx: &mut Transaction<'_, Postgres>,
    rule: &Rule,
    now: DateTime<Utc>,
) -> Result<Vec<Condition>, sqlx::Error> {
    let (Some(factor), Some(floor)) = (rule.spike_factor_percent, rule.min_spend_microusd) else {
        return Ok(vec![]);
    };
    let hour = now - TimeDelta::hours(1);
    let start = hour - TimeDelta::hours(BASELINE_HOURS as i64);
    // Settled spend only: in-flight holds are upper estimates, unknown cost is flagged.
    let (current, baseline, unknown): (String, String, i64) = sqlx::query_as(SPIKE)
        .bind(start)
        .bind(hour)
        .bind(now)
        .bind(rule.workspace_id)
        .fetch_one(&mut **tx)
        .await?;
    let current: i128 = current.parse().unwrap_or(0);
    let baseline: i128 = baseline.parse().unwrap_or(0);
    if !spike_firing(current, baseline, i64::from(factor), floor) {
        return Ok(vec![]);
    }
    Ok(vec![Condition {
        subject: "spend_spike".into(),
        level: 1,
        critical: false,
        workspace_id: rule.workspace_id,
        connection_id: None,
        summary: format!("Spend spike: {} in the last hour", usd(current)),
        details: json!({
            "current_microusd": current.to_string(),
            "baseline_total_microusd": baseline.to_string(),
            "baseline_hourly_microusd": (baseline / BASELINE_HOURS).to_string(),
            "ratio": spike_ratio(current, baseline),
            "factor_percent": factor,
            "min_spend_microusd": floor.to_string(),
            "unknown_cost_requests": unknown,
        }),
    }])
}

async fn error_rate_conditions(
    tx: &mut Transaction<'_, Postgres>,
    rule: &Rule,
    now: DateTime<Utc>,
) -> Result<Vec<Condition>, sqlx::Error> {
    let (Some(window), Some(rate), Some(min)) = (
        rule.window_minutes,
        rule.error_rate_percent,
        rule.min_requests,
    ) else {
        return Ok(vec![]);
    };
    let (finished, failed): (i64, i64) = sqlx::query_as("SELECT count(*) FILTER(WHERE state<>'started'),count(*) FILTER(WHERE state='failed') FROM inference_executions WHERE started_at>=$1 AND started_at<$2 AND ($3::uuid IS NULL OR workspace_id=$3)")
        .bind(now - TimeDelta::minutes(i64::from(window)))
        .bind(now)
        .bind(rule.workspace_id)
        .fetch_one(&mut **tx)
        .await?;
    if !rate_firing(failed, finished, rate, min) {
        return Ok(vec![]);
    }
    let observed = failed * 100 / finished.max(1);
    Ok(vec![Condition {
        subject: "error_rate".into(),
        level: 1,
        critical: false,
        workspace_id: rule.workspace_id,
        connection_id: None,
        summary: format!("Error rate {observed}% over {window} min"),
        details: json!({
            "failed": failed,
            "finished": finished,
            "observed_percent": observed,
            "threshold_percent": rate,
            "min_requests": min,
            "window_minutes": window,
        }),
    }])
}

async fn provider_conditions(
    tx: &mut Transaction<'_, Postgres>,
    rule: &Rule,
    now: DateTime<Utc>,
) -> Result<Vec<Condition>, sqlx::Error> {
    let Some(window) = rule.window_minutes else {
        return Ok(vec![]);
    };
    let since = now - TimeDelta::minutes(i64::from(window));
    let connections: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM provider_connections WHERE enabled AND ($1::uuid IS NULL OR id=$1) ORDER BY id LIMIT 500")
        .bind(rule.provider_connection_id)
        .fetch_all(&mut **tx)
        .await?;
    let mut out = Vec::new();
    for connection in connections {
        // Successes and upstream failures only: request-specific rejections and cancellations neither count nor reset.
        let relevant = "e.started_at>=$2 AND e.started_at<$3 AND (e.state='succeeded' OR (e.state='failed' AND e.error_code=ANY($4)))";
        let (finished, failed): (i64, i64) = sqlx::query_as(&format!("SELECT count(*),count(*) FILTER(WHERE e.state='failed') FROM inference_executions e JOIN deployments d ON d.id=e.deployment_id WHERE d.provider_connection_id=$1 AND {relevant}"))
            .bind(connection)
            .bind(since)
            .bind(now)
            .bind(&UPSTREAM_FAILURES[..])
            .fetch_one(&mut **tx)
            .await?;
        let mut consecutive = 0i64;
        if let Some(n) = rule.consecutive_failures {
            let latest: Vec<String> = sqlx::query_scalar(&format!("SELECT e.state FROM inference_executions e JOIN deployments d ON d.id=e.deployment_id WHERE d.provider_connection_id=$1 AND {relevant} ORDER BY e.completed_at DESC NULLS LAST,e.started_at DESC,e.id LIMIT $5"))
                .bind(connection)
                .bind(since)
                .bind(now)
                .bind(&UPSTREAM_FAILURES[..])
                .bind(i64::from(n))
                .fetch_all(&mut **tx)
                .await?;
            consecutive = latest.iter().take_while(|s| *s == "failed").count() as i64;
            if consecutive < i64::from(n) {
                consecutive = 0;
            }
        }
        let by_rate = matches!((rule.error_rate_percent, rule.min_requests), (Some(rate), Some(min)) if rate_firing(failed, finished, rate, min));
        if consecutive == 0 && !by_rate {
            continue;
        }
        let summary = if consecutive > 0 {
            format!("Connection failing: {consecutive} upstream failures in a row")
        } else {
            format!(
                "Connection failing: {}% upstream errors over {window} min",
                failed * 100 / finished.max(1)
            )
        };
        out.push(Condition {
            subject: format!("connection:{connection}"),
            level: 1,
            critical: true,
            workspace_id: None,
            connection_id: Some(connection),
            summary,
            details: json!({
                "consecutive_failures": (consecutive > 0).then_some(consecutive),
                "failed": failed,
                "finished": finished,
                "window_minutes": window,
                "threshold_consecutive": rule.consecutive_failures,
                "threshold_percent": rule.error_rate_percent,
            }),
        });
    }
    Ok(out)
}

/// How long a failed batch keeps its incident open.
pub const BATCH_FAILED_HOURS: i64 = 24;
/// Batch alerts watch one workspace (workspace rules) or every Team/Project
/// (installation rules; personal batches are private).
const BATCH_SCOPE: &str = "j.kind='batch' AND w.disabled_at IS NULL AND CASE WHEN $1::uuid IS NULL THEN w.kind IN('team','project') ELSE j.workspace_id=$1 END";

/// (id, workspace, state, error code, mode, total, completed, failed).
type FailedBatch = (
    Uuid,
    Uuid,
    String,
    Option<String>,
    Option<String>,
    Option<i32>,
    Option<i32>,
    Option<i32>,
);
/// (id, workspace, mode, total, done, last progress).
type StalledBatch = (
    Uuid,
    Uuid,
    Option<String>,
    Option<i32>,
    Option<i32>,
    DateTime<Utc>,
);

/// One incident per batch that ended failed or expired in the last day.
async fn batch_failed_conditions(
    tx: &mut Transaction<'_, Postgres>,
    rule: &Rule,
    now: DateTime<Utc>,
) -> Result<Vec<Condition>, sqlx::Error> {
    let rows: Vec<FailedBatch> = sqlx::query_as(&format!("SELECT j.id,j.workspace_id,j.state,j.error_code,j.batch_mode,j.request_total,j.request_completed,j.request_failed FROM async_jobs j JOIN workspaces w ON w.id=j.workspace_id WHERE {BATCH_SCOPE} AND j.state IN('failed','expired') AND j.completed_at>$2 AND j.completed_at<=$3 ORDER BY j.completed_at DESC LIMIT 100"))
        .bind(rule.workspace_id)
        .bind(now - TimeDelta::hours(BATCH_FAILED_HOURS))
        .bind(now)
        .fetch_all(&mut **tx)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, ws, state, code, mode, total, completed, failed)| {
            let expired = state == "expired";
            Condition {
                subject: format!("batch:{id}"),
                level: 1,
                critical: false,
                workspace_id: Some(ws),
                connection_id: None,
                summary: if expired {
                    "Batch expired before it finished".into()
                } else if code.as_deref() == Some("budget_exceeded") {
                    "Batch stopped: a budget was exhausted".into()
                } else {
                    "Batch failed".into()
                },
                details: json!({
                    "batch_id": crate::jobs::types::client_id("batch_", id),
                    "state": state,
                    "error_code": code,
                    "mode": mode.unwrap_or_else(|| "native".into()),
                    "total": total,
                    "completed": completed,
                    "failed": failed,
                }),
            }
        })
        .collect())
}

/// One incident per unfinished batch without progress for the window. A
/// gateway-run batch legitimately waiting for its routes (time window, live
/// traffic, server load, concurrency: `last_waited_at`, 0022) is not
/// stalled; its clock starts when the wait ends. An unreadable server load
/// signal is not a legitimate wait.
async fn batch_stalled_conditions(
    tx: &mut Transaction<'_, Postgres>,
    rule: &Rule,
    now: DateTime<Utc>,
) -> Result<Vec<Condition>, sqlx::Error> {
    let Some(window) = rule.window_minutes else {
        return Ok(vec![]);
    };
    let rows: Vec<StalledBatch> = sqlx::query_as(&format!("SELECT j.id,j.workspace_id,j.batch_mode,j.request_total,coalesce(j.request_completed,0)+coalesce(j.request_failed,0),greatest(coalesce(j.last_progress_at,j.in_progress_at,j.created_at),j.last_waited_at) FROM async_jobs j JOIN workspaces w ON w.id=j.workspace_id WHERE {BATCH_SCOPE} AND j.settled_at IS NULL AND j.state IN('queued','in_progress') AND greatest(coalesce(j.last_progress_at,j.in_progress_at,j.created_at),j.last_waited_at)<=$2 ORDER BY 6 LIMIT 100"))
        .bind(rule.workspace_id)
        .bind(now - TimeDelta::minutes(i64::from(window)))
        .fetch_all(&mut **tx)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, ws, mode, total, done, since)| Condition {
            subject: format!("batch:{id}"),
            level: 1,
            critical: false,
            workspace_id: Some(ws),
            connection_id: None,
            summary: format!("Batch stalled: no progress for {window} min"),
            details: json!({
                "batch_id": crate::jobs::types::client_id("batch_", id),
                "mode": mode.unwrap_or_else(|| "native".into()),
                "total": total,
                "done": done,
                "minutes_without_progress": (now - since).num_minutes().max(0),
                "window_minutes": window,
            }),
        })
        .collect())
}

/// Admissions a scope needs in the window before its lock-wait p95 counts.
pub const CEILING_MIN_REQUESTS: i64 = 100;
/// At most this many scopes per evaluation (the busiest first).
const MAX_CEILING_SCOPES: i64 = 100;

/// Scopes near their admission ceiling over the complete UTC minutes
/// [`$1`, `$2`) (`$3` minutes): every minute at or above `$4` admissions/s
/// (`rate_minute_counters`, interactive admissions), or with at least
/// [`CEILING_MIN_REQUESTS`] admissions of which 5 % or more waited at least
/// the `$5`-th lock-wait bucket (`admission_lock_waits`; lock-wait p95 at or
/// above the threshold). Minutes are probed by primary key per candidate.
/// Returns the scope, its workspace and the window's counts.
pub(crate) const CEILING: &str = r#"WITH hot AS (SELECT scope_kind,scope_id FROM rate_minute_counters
  WHERE $4::bigint IS NOT NULL AND minute_start>=$1 AND minute_start<$2 AND requests>=$4::bigint*60
  GROUP BY 1,2 HAVING count(*)=$3),
 lw AS (SELECT scope_kind,scope_id,ARRAY[sum(waits[1]),sum(waits[2]),sum(waits[3]),sum(waits[4]),sum(waits[5]),sum(waits[6]),sum(waits[7]),sum(waits[8])]::bigint[] waits
  FROM admission_lock_waits WHERE $5::int IS NOT NULL AND minute_start>=$1 AND minute_start<$2 GROUP BY 1,2),
 c AS (SELECT scope_kind,scope_id FROM hot UNION SELECT scope_kind,scope_id FROM lw),
 x AS (SELECT c.scope_kind,c.scope_id,coalesce(lw.waits,'{0,0,0,0,0,0,0,0}') waits,m.requests,m.hot_minutes
  FROM c LEFT JOIN lw USING(scope_kind,scope_id)
  CROSS JOIN LATERAL (SELECT coalesce(sum(r.requests),0)::bigint requests,
    count(*) FILTER(WHERE $4::bigint IS NOT NULL AND r.requests>=$4::bigint*60) hot_minutes
   FROM generate_series($1::timestamptz,$2::timestamptz-interval '1 minute',interval '1 minute') g(m)
   JOIN rate_minute_counters r ON r.minute_start=g.m AND r.scope_kind=c.scope_kind AND r.scope_id=c.scope_id) m)
SELECT x.scope_kind,x.scope_id,w.id,w.kind,w.name,x.requests,x.hot_minutes,x.waits
FROM x LEFT JOIN api_keys k ON x.scope_kind='key' AND k.id=x.scope_id
JOIN workspaces w ON w.id=CASE WHEN x.scope_kind='workspace' THEN x.scope_id ELSE k.workspace_id END
WHERE x.hot_minutes=$3 OR ($5::int IS NOT NULL AND x.requests>=$6 AND x.waits[$5]*20>=x.requests)
ORDER BY x.requests DESC,x.scope_kind,x.scope_id LIMIT $7"#;

type CeilingRow = (String, Uuid, Uuid, String, String, i64, i64, Vec<i64>);

/// Lock-wait p95 lower bound (ms) from the bucket counts: the largest bound
/// that 5 % or more of the window's admissions reached, if any.
pub fn p95_at_least_ms(waits: &[i64], requests: i64) -> Option<i32> {
    if requests <= 0 {
        return None;
    }
    crate::governance::pressure::WAIT_BUCKETS_MS
        .iter()
        .zip(waits)
        .filter(|(_, n)| **n * 20 >= requests)
        .map(|(b, _)| *b)
        .next_back()
}

/// One incident per workspace or key lineage near its admission ceiling.
/// The incident identifies the scope for Platform Admins: a Team/Project by
/// its workspace (`alert_events.workspace_id`, name in the details); a
/// personal workspace only as "a personal workspace" (no id, name or owner),
/// and keys only by their workspace (never key ids or names).
async fn ceiling_conditions(
    tx: &mut Transaction<'_, Postgres>,
    rule: &Rule,
    now: DateTime<Utc>,
) -> Result<Vec<Condition>, sqlx::Error> {
    let Some(window) = rule.window_minutes else {
        return Ok(vec![]);
    };
    let rate = rule.ceiling_requests_per_second;
    let wait_index = rule.ceiling_lock_wait_ms.and_then(|ms| {
        crate::governance::pressure::WAIT_BUCKETS_MS
            .iter()
            .position(|b| *b == ms)
            .map(|i| i as i32 + 1)
    });
    if rate.is_none() && wait_index.is_none() {
        return Ok(vec![]);
    }
    let hi = now.duration_trunc(TimeDelta::minutes(1)).unwrap_or(now);
    let lo = hi - TimeDelta::minutes(i64::from(window));
    let rows: Vec<CeilingRow> = sqlx::query_as(CEILING)
        .bind(lo)
        .bind(hi)
        .bind(i64::from(window))
        .bind(rate.map(i64::from))
        .bind(wait_index)
        .bind(CEILING_MIN_REQUESTS)
        .bind(MAX_CEILING_SCOPES)
        .fetch_all(&mut **tx)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(kind, id, ws, ws_kind, ws_name, requests, hot_minutes, waits)| {
            let personal = ws_kind == "personal";
            let key = kind == "key";
            let p95 = p95_at_least_ms(&waits, requests);
            let by_rate = rate.is_some() && hot_minutes == i64::from(window);
            let by_wait = rule
                .ceiling_lock_wait_ms
                .is_some_and(|t| p95.is_some_and(|p| p >= t) && requests >= CEILING_MIN_REQUESTS);
            let what = match (key, personal) {
                (false, false) => "A workspace",
                (false, true) => "A personal workspace",
                (true, false) => "An API key",
                (true, true) => "An API key in a personal workspace",
            };
            let seconds = i64::from(window) * 60;
            Condition {
                subject: format!("{kind}:{id}"),
                level: 1,
                critical: false,
                workspace_id: (!personal).then_some(ws),
                connection_id: None,
                summary: format!("{what} is near its admission ceiling"),
                details: json!({
                    "scope": kind,
                    "personal": personal,
                    "workspace_name": (!personal).then_some(ws_name),
                    "workspace_kind": ws_kind,
                    "window_minutes": window,
                    "admissions": requests,
                    // Tenths of admissions per second, rounded down.
                    "admissions_per_second": format!("{}.{}", requests * 10 / seconds / 10, requests * 10 / seconds % 10),
                    "by_rate": by_rate,
                    "by_lock_wait": by_wait,
                    "threshold_requests_per_second": rate,
                    "lock_wait_p95_at_least_ms": p95,
                    "threshold_lock_wait_ms": rule.ceiling_lock_wait_ms,
                }),
            }
        })
        .collect())
}

async fn evaluate_rule(
    tx: &mut Transaction<'_, Postgres>,
    rule: &Rule,
    now: DateTime<Utc>,
) -> Result<Vec<Condition>, sqlx::Error> {
    match Kind::parse(&rule.kind) {
        Some(Kind::Budget) => {
            let set = rule
                .workspace_id
                .map_or(Workspaces::Shared, Workspaces::One);
            budget_conditions(
                tx,
                set,
                rule.budget_layers.as_deref().unwrap_or_default(),
                rule.thresholds.as_deref().unwrap_or_default(),
                now,
            )
            .await
        }
        Some(Kind::Spike) => spike_conditions(tx, rule, now).await,
        Some(Kind::ErrorRate) => error_rate_conditions(tx, rule, now).await,
        Some(Kind::Provider) => provider_conditions(tx, rule, now).await,
        Some(Kind::BatchFailed) => batch_failed_conditions(tx, rule, now).await,
        Some(Kind::BatchStalled) => batch_stalled_conditions(tx, rule, now).await,
        Some(Kind::Spend) => spend_conditions(tx, rule, now).await,
        Some(Kind::AdmissionCeiling) => ceiling_conditions(tx, rule, now).await,
        None => Ok(vec![]),
    }
}

/// Counts of one reconciliation.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Changes {
    pub fired: usize,
    pub resolved: usize,
}

/// Apply `conditions` to the open incidents of `source`: fire new subjects,
/// resolve cleared ones, supersede changed levels. Idempotent.
async fn reconcile(
    tx: &mut Transaction<'_, Postgres>,
    source: Source,
    kind: Kind,
    conditions: Vec<Condition>,
) -> Result<Changes, sqlx::Error> {
    let open: Vec<(Uuid, String, i32)> = sqlx::query_as("SELECT id,subject_key,level FROM alert_events WHERE resolved_at IS NULL AND rule_id IS NOT DISTINCT FROM $1 AND builtin IS NOT DISTINCT FROM $2 FOR UPDATE")
        .bind(source.rule())
        .bind(source.builtin())
        .fetch_all(&mut **tx)
        .await?;
    let mut changes = Changes::default();
    for (id, subject, _) in &open {
        if !conditions.iter().any(|c| &c.subject == subject) {
            resolve(tx, *id, "cleared").await?;
            changes.resolved += 1;
        }
    }
    for c in conditions {
        match open.iter().find(|(_, subject, _)| *subject == c.subject) {
            Some((_, _, level)) if *level == c.level => continue,
            Some((id, ..)) => resolve(tx, *id, "superseded").await?,
            None => {}
        }
        let id = Uuid::new_v4();
        // Never fire for a rule that was disabled or deleted meanwhile.
        let inserted = sqlx::query("INSERT INTO alert_events(id,rule_id,builtin,kind,subject_key,level,severity,workspace_id,provider_connection_id,summary,details) SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11 WHERE $2::uuid IS NULL OR EXISTS(SELECT 1 FROM alert_rules WHERE id=$2 AND enabled AND deleted_at IS NULL) ON CONFLICT DO NOTHING")
            .bind(id)
            .bind(source.rule())
            .bind(source.builtin())
            .bind(kind.as_str())
            .bind(&c.subject)
            .bind(c.level)
            .bind(if c.critical { "critical" } else { "warning" })
            .bind(c.workspace_id)
            .bind(c.connection_id)
            .bind(&c.summary)
            .bind(&c.details)
            .execute(&mut **tx)
            .await?
            .rows_affected();
        if inserted == 1 {
            sqlx::query(
                "INSERT INTO alert_deliveries(id,event_id,transition) VALUES($1,$2,'fired')",
            )
            .bind(Uuid::new_v4())
            .bind(id)
            .execute(&mut **tx)
            .await?;
            changes.fired += 1;
        }
    }
    Ok(changes)
}

pub(crate) async fn resolve(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    resolution: &str,
) -> Result<(), sqlx::Error> {
    let changed = sqlx::query("UPDATE alert_events SET resolved_at=clock_timestamp(),resolution=$2 WHERE id=$1 AND resolved_at IS NULL")
        .bind(id)
        .bind(resolution)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    if changed == 1 && resolution == "cleared" {
        sqlx::query(
            "INSERT INTO alert_deliveries(id,event_id,transition) VALUES($1,$2,'resolved')",
        )
        .bind(Uuid::new_v4())
        .bind(id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Resolve open incidents of disabled/deleted rules, or rules whose workspace
/// was disabled, without email. Shared with management (disable/delete).
pub(crate) async fn retire_inactive(
    tx: &mut Transaction<'_, Postgres>,
    rule: Option<Uuid>,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query("UPDATE alert_events e SET resolved_at=clock_timestamp(),resolution='rule_disabled' FROM alert_rules r WHERE e.rule_id=r.id AND e.resolved_at IS NULL AND ($1::uuid IS NULL OR r.id=$1) AND (NOT r.enabled OR r.deleted_at IS NOT NULL OR EXISTS(SELECT 1 FROM workspaces w WHERE w.id=r.workspace_id AND w.disabled_at IS NOT NULL))")
        .bind(rule)
        .execute(&mut **tx)
        .await?
        .rows_affected())
}

/// What one evaluation did. `None` from [`evaluate_once`] means another replica
/// is evaluating right now.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub rules: usize,
    pub fired: usize,
    pub resolved: usize,
    pub failed_rules: usize,
}

/// One bounded evaluation of every enabled rule and the built-in personal budget alerts.
///
/// A short claim transaction takes the evaluation lock with `try` (another
/// replica evaluating means this tick does nothing), retires inactive
/// incidents and lists the rules. Each rule is then evaluated and reconciled
/// in its own short transaction that waits for the same lock, so no snapshot
/// spans the whole tick (vacuum is not held back) and one rule's incidents
/// are never reconciled concurrently. Budget rules read maintained totals.
pub async fn evaluate_once(store: &Store) -> anyhow::Result<Option<Report>> {
    Ok(evaluate_once_fenced(store, None).await?)
}

/// [`evaluate_once`] as the `alerts` lease holder: the claim and every rule
/// transaction are fenced to the term (a replica whose term ended commits
/// nothing more). The tick lock still excludes a concurrent operator
/// `alerts evaluate --once`.
pub async fn evaluate_once_fenced(
    store: &Store,
    fence: Option<&crate::leases::Fence>,
) -> Result<Option<Report>, sqlx::Error> {
    let mut tx = crate::db::begin(&store.pool).await?;
    crate::leases::fence(&mut tx, fence).await?;
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(EVALUATION_LOCK)
        .fetch_one(&mut *tx)
        .await?;
    if !locked {
        return Ok(None);
    }
    sqlx::query("SET LOCAL statement_timeout='10s'")
        .execute(&mut *tx)
        .await?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    // Deliveries a crashed process left behind are recorded, not resent forever.
    sqlx::query("UPDATE alert_deliveries SET status='failed',error='interrupted',completed_at=clock_timestamp() WHERE status='pending' AND created_at<now()-interval '1 day'")
        .execute(&mut *tx)
        .await?;
    let mut report = Report {
        resolved: retire_inactive(&mut tx, None).await? as usize
            + resolve_scim_last_admin(&mut tx).await?,
        ..Report::default()
    };
    let rules: Vec<Rule> = sqlx::query_as("SELECT r.id,r.workspace_id,r.kind,r.budget_layers,r.thresholds,r.spike_factor_percent,r.min_spend_microusd,r.window_minutes,r.error_rate_percent,r.min_requests,r.consecutive_failures,r.provider_connection_id,r.spend_period,r.spend_amount_microusd,r.ceiling_requests_per_second,r.ceiling_lock_wait_ms FROM alert_rules r LEFT JOIN workspaces w ON w.id=r.workspace_id WHERE r.enabled AND r.deleted_at IS NULL AND (r.workspace_id IS NULL OR w.disabled_at IS NULL) ORDER BY r.created_at,r.id LIMIT $1")
        .bind(MAX_RULES)
        .fetch_all(&mut *tx)
        .await?;
    let mut jobs: Vec<(Source, Kind, Option<Rule>)> = rules
        .into_iter()
        .filter_map(|r| Some((Source::Rule(r.id), Kind::parse(&r.kind)?, Some(r))))
        .collect();
    jobs.push((Source::Builtin, Kind::Budget, None));
    tx.commit().await?;
    for (source, kind, rule) in jobs {
        report.rules += 1;
        let mut tx = crate::db::begin(&store.pool).await?;
        crate::leases::fence(&mut tx, fence).await?;
        let outcome = async {
            sqlx::query("SET LOCAL statement_timeout='10s'")
                .execute(&mut *tx)
                .await?;
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(EVALUATION_LOCK)
                .execute(&mut *tx)
                .await?;
            let conditions = match &rule {
                Some(rule) => evaluate_rule(&mut tx, rule, now).await?,
                None => {
                    let layers = ["type", "override", "local", "key"].map(String::from);
                    budget_conditions(
                        &mut tx,
                        Workspaces::Personal,
                        &layers,
                        &PERSONAL_THRESHOLDS,
                        now,
                    )
                    .await?
                }
            };
            reconcile(&mut tx, source, kind, conditions).await
        }
        .await;
        match outcome {
            Ok(changes) => {
                tx.commit().await?;
                report.fired += changes.fired;
                report.resolved += changes.resolved;
            }
            Err(_) => {
                tx.rollback().await?;
                report.failed_rules += 1;
            }
        }
    }
    Ok(Some(report))
}

/// Raise (or keep) the SCIM last-admin incident. `resource` is `user` or
/// `group` only: no names, emails or ids. Idempotent while one is open.
pub(crate) async fn fire_scim_last_admin(
    tx: &mut Transaction<'_, Postgres>,
    resource: &str,
) -> Result<bool, sqlx::Error> {
    let id = Uuid::new_v4();
    let inserted = sqlx::query("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary,details) VALUES($1,$2,$2,$2,1,'critical',$3,jsonb_build_object('resource',$4::text)) ON CONFLICT DO NOTHING")
        .bind(id)
        .bind(SCIM_LAST_ADMIN)
        .bind(SCIM_LAST_ADMIN_SUMMARY)
        .bind(resource)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    if inserted == 1 {
        sqlx::query("INSERT INTO alert_deliveries(id,event_id,transition) VALUES($1,$2,'fired')")
            .bind(Uuid::new_v4())
            .bind(id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(inserted == 1)
}

/// The SCIM last-admin incident clears once a second active Platform Admin
/// exists (the installation is no longer one SCIM change away from lockout).
async fn resolve_scim_last_admin(tx: &mut Transaction<'_, Postgres>) -> Result<usize, sqlx::Error> {
    let open: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM alert_events WHERE builtin=$1 AND resolved_at IS NULL AND (SELECT count(*) FROM effective_platform_roles WHERE role='admin')>=2 FOR UPDATE")
        .bind(SCIM_LAST_ADMIN)
        .fetch_all(&mut **tx)
        .await?;
    for id in &open {
        resolve(tx, *id, "cleared").await?;
    }
    Ok(open.len())
}

// ---------- Delivery ----------

type SmtpRow = (
    Option<String>,
    Option<i32>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
);
async fn relay(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<(Option<SmtpSettings>, String), sqlx::Error> {
    let (host, port, tls, username, password_ref, from_address, from_name, installation): SmtpRow =
        sqlx::query_as("SELECT s.smtp_host,s.smtp_port,s.smtp_tls,s.smtp_username,s.smtp_password_ref,s.smtp_from_address,s.smtp_from_name,i.name FROM installation_settings s JOIN installation i ON i.singleton WHERE s.singleton")
            .fetch_one(&mut **tx)
            .await?;
    let settings = (|| {
        Some(SmtpSettings {
            host: host?,
            port: u16::try_from(port?).ok()?,
            tls: TlsMode::parse(tls.as_deref()?)?,
            username,
            password_ref,
            from_address: from_address?,
            from_name,
        })
    })();
    Ok((settings, installation))
}

fn public_url() -> Option<String> {
    std::env::var("GATEWAY_PUBLIC_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_owned())
        .filter(|v| {
            (v.starts_with("https://") || v.starts_with("http://"))
                && v.len() <= 512
                && !v.chars().any(|c| c.is_control() || c.is_whitespace())
        })
}

/// Facts an alert email may contain: server-generated summary and scope
/// labels only (no key names, owner identity or request content).
#[derive(Debug, Clone)]
pub struct EmailFacts {
    pub installation: String,
    pub summary: String,
    pub resolved: bool,
    pub critical: bool,
    /// "Installation", "Team Platform", "Connection OpenAI", "Your personal workspace".
    pub scope: String,
    /// Rule name, or `None` for built-in alerts.
    pub rule: Option<String>,
    pub at: DateTime<Utc>,
}
pub fn email_text(f: &EmailFacts, link: Option<&str>) -> (String, String) {
    let clean = |s: &str| {
        s.chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect::<String>()
    };
    let state = if f.resolved {
        "Resolved"
    } else if f.critical {
        "Critical"
    } else {
        "Alert"
    };
    let subject = clean(&format!(
        "[{}] {state}: {} ({})",
        f.installation, f.summary, f.scope
    ));
    let mut body = format!(
        "{state}: {}\r\n\r\nWhere: {}\r\nRule: {}\r\n{}: {} UTC\r\n",
        clean(&f.summary),
        clean(&f.scope),
        f.rule
            .as_deref()
            .map_or_else(|| "Built-in budget alert".to_owned(), clean),
        if f.resolved { "Resolved" } else { "Since" },
        f.at.format("%Y-%m-%d %H:%M"),
    );
    match link {
        Some(url) => body.push_str(&format!("\r\nDetails: {url}/notifications\r\n")),
        None => body.push_str("\r\nOpen the gateway and choose Notifications for details.\r\n"),
    }
    body.push_str(&format!(
        "\r\nYou receive this because you are a recipient of this alert in {}.\r\n",
        clean(&f.installation)
    ));
    (subject, body)
}

#[derive(sqlx::FromRow)]
struct Pending {
    transition: String,
    summary: String,
    severity: String,
    fired_at: DateTime<Utc>,
    resolved_at: Option<DateTime<Utc>>,
    rule_id: Option<Uuid>,
    rule_name: Option<String>,
    workspace_name: Option<String>,
    workspace_kind: Option<String>,
    connection_name: Option<String>,
    personal_workspace: Option<Uuid>,
    event_id: Uuid,
    builtin: Option<String>,
}

/// Send one pending delivery while holding its row lock (no double sends
/// across replicas). Returns false when nothing was pending.
async fn deliver_one(store: &Store, id: Uuid) -> anyhow::Result<bool> {
    let mut tx = crate::db::begin(&store.pool).await?;
    let row: Option<Pending> = sqlx::query_as("SELECT d.transition,e.summary,e.severity,e.fired_at,e.resolved_at,e.rule_id,r.name rule_name,w.name workspace_name,w.kind workspace_kind,p.name connection_name,CASE WHEN e.builtin IS NOT NULL THEN e.workspace_id END personal_workspace,e.id event_id,e.builtin FROM alert_deliveries d JOIN alert_events e ON e.id=d.event_id LEFT JOIN alert_rules r ON r.id=e.rule_id LEFT JOIN workspaces w ON w.id=e.workspace_id LEFT JOIN provider_connections p ON p.id=e.provider_connection_id WHERE d.id=$1 AND d.status='pending' FOR UPDATE OF d SKIP LOCKED")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(Pending {
        transition,
        summary,
        severity,
        fired_at,
        resolved_at,
        rule_id: rule,
        rule_name,
        workspace_name: ws_name,
        workspace_kind: ws_kind,
        connection_name: connection,
        personal_workspace: personal,
        event_id,
        builtin,
    }) = row
    else {
        return Ok(false);
    };
    let recipients: Vec<String> = sqlx::query_scalar("SELECT DISTINCT lower(x.email) FROM (
 SELECT unnest(r.notify_emails) email FROM alert_rules r WHERE r.id=$1
 UNION ALL SELECT u.email FROM alert_rules r JOIN effective_workspace_memberships m ON m.workspace_id=r.workspace_id AND m.role IN ('owner','admin') JOIN users u ON u.id=m.user_id WHERE r.id=$1 AND r.notify_workspace_admins
 UNION ALL SELECT u.email FROM alert_rules r JOIN effective_platform_roles p ON p.role='admin' JOIN users u ON u.id=p.user_id WHERE r.id=$1 AND r.notify_platform_admins
 UNION ALL SELECT u.email FROM workspaces w JOIN users u ON u.id=w.owner_user_id JOIN effective_platform_roles p ON p.user_id=u.id WHERE w.id=$2 AND w.kind='personal' AND w.disabled_at IS NULL
 UNION ALL SELECT u.email FROM alert_events e JOIN effective_platform_roles p ON p.role='admin' JOIN users u ON u.id=p.user_id WHERE e.id=$4 AND e.builtin=ANY($5)
) x WHERE x.email IS NOT NULL ORDER BY 1 LIMIT $3")
        .bind(rule)
        .bind(personal)
        .bind(MAX_RECIPIENTS)
        .bind(event_id)
        .bind(&INSTALLATION_BUILTINS[..])
        .fetch_all(&mut *tx)
        .await?;
    let (settings, installation) = relay(&mut tx).await?;
    let finish = |status: &'static str, sent: i32, failed: i32, error: Option<&'static str>| {
        sqlx::query("UPDATE alert_deliveries SET status=$2,recipients=$3,sent=$4,failed=$5,error=$6,completed_at=clock_timestamp() WHERE id=$1 AND status='pending'")
            .bind(id)
            .bind(status)
            .bind(recipients.len() as i32)
            .bind(sent)
            .bind(failed)
            .bind(error)
    };
    let Some(settings) = settings else {
        finish("not_configured", 0, 0, None)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(true);
    };
    if recipients.is_empty() {
        finish("no_recipients", 0, 0, None)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(true);
    }
    let scope = if personal.is_some() {
        "Your personal workspace".to_owned()
    } else if let Some(connection) = connection {
        format!("Connection {connection}")
    } else if let Some(name) = ws_name {
        format!(
            "{} {name}",
            if ws_kind.as_deref() == Some("project") {
                "Project"
            } else {
                "Team"
            }
        )
    } else {
        "Installation".to_owned()
    };
    let resolved = transition == "resolved";
    let (subject, body) = email_text(
        &EmailFacts {
            installation,
            summary,
            resolved,
            critical: severity == "critical",
            scope,
            rule: rule_name.or_else(|| match builtin.as_deref() {
                Some(SCIM_LAST_ADMIN) => Some("Built-in SCIM safeguard".to_owned()),
                Some(PARTITIONS_MISSING) => Some("Built-in partition check".to_owned()),
                Some(HISTORY_ORPHANS) => Some("Built-in history check".to_owned()),
                _ => None,
            }),
            at: if resolved {
                resolved_at.unwrap_or(fired_at)
            } else {
                fired_at
            },
        },
        public_url().as_deref(),
    );
    let (mut sent, mut failed, mut error) = (0, 0, None);
    for to in &recipients {
        match settings
            .send(OutgoingEmail {
                to: to.clone(),
                subject: subject.clone(),
                body: body.clone(),
            })
            .await
        {
            Ok(()) => sent += 1,
            Err(e) => {
                failed += 1;
                error.get_or_insert(e.as_str());
            }
        }
    }
    let status = match (sent, failed) {
        (_, 0) => "sent",
        (0, _) => "failed",
        _ => "partial",
    };
    finish(status, sent, failed, error)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// Deliver up to `limit` pending emails (oldest first). Failures are recorded
/// per delivery and never stop the others.
pub async fn deliver_pending(store: &Store, limit: i64) -> usize {
    let Ok(ids) = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM alert_deliveries WHERE status='pending' ORDER BY created_at,id LIMIT $1",
    )
    .bind(limit.clamp(1, 1000))
    .fetch_all(&store.pool)
    .await
    else {
        tracing::warn!("alert delivery queue unavailable");
        return 0;
    };
    let mut done = 0;
    for id in ids {
        match deliver_one(store, id).await {
            Ok(true) => done += 1,
            Ok(false) => {}
            Err(_) => tracing::warn!("alert email delivery could not be recorded; retrying later"),
        }
    }
    done
}

/// Background evaluator for `serve`.
pub fn start(store: Store, every: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            // With leases (serve), only the `alerts` lease holder evaluates:
            // before P5 each replica evaluated whenever its own tick found the
            // tick lock free, so N replicas evaluated about N times per interval.
            let evaluated = match store.leases() {
                Some(leases) => match leases.held(crate::leases::Lease::Alerts) {
                    Some(fence) => {
                        tokio::time::timeout(
                            Duration::from_secs(60),
                            evaluate_once_fenced(&store, Some(&fence)),
                        )
                        .await
                    }
                    None => Ok(Ok(None)),
                },
                None => {
                    tokio::time::timeout(
                        Duration::from_secs(60),
                        evaluate_once_fenced(&store, None),
                    )
                    .await
                }
            };
            match evaluated {
                Ok(Err(error)) if crate::leases::is_fenced(&error) => {
                    crate::metrics::METRICS.observe_alert_run("skipped", 0)
                }
                Ok(Ok(Some(report))) => {
                    crate::metrics::METRICS.observe_alert_run("ok", report.failed_rules);
                    if report.fired + report.resolved + report.failed_rules > 0 {
                        tracing::info!(
                            fired = report.fired,
                            resolved = report.resolved,
                            failed_rules = report.failed_rules,
                            "alerts evaluated"
                        );
                    }
                }
                Ok(Ok(None)) => crate::metrics::METRICS.observe_alert_run("skipped", 0),
                _ => {
                    crate::metrics::METRICS.observe_alert_run("failed", 0);
                    tracing::warn!("alert evaluation incomplete; retrying next interval")
                }
            }
            if tokio::time::timeout(
                Duration::from_secs(300),
                deliver_pending(&store, DELIVERIES_PER_TICK),
            )
            .await
            .is_err()
            {
                tracing::warn!(
                    "alert email delivery timed out; pending emails retry next interval"
                );
            }
        }
    })
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn thresholds_are_exact_integer_math() {
        let t = [50, 80, 100];
        assert_eq!(threshold_level(0, 100, &t), None);
        assert_eq!(threshold_level(49, 100, &t), None);
        assert_eq!(threshold_level(50, 100, &t), Some(50));
        // 79.999999% is not 80%.
        assert_eq!(threshold_level(7_999_999, 10_000_000, &t), Some(50));
        assert_eq!(threshold_level(8_000_000, 10_000_000, &t), Some(80));
        assert_eq!(threshold_level(10_000_000, 10_000_000, &t), Some(100));
        assert_eq!(threshold_level(30_000_000, 10_000_000, &t), Some(100));
        // Deny-all (zero) budgets never alert.
        assert_eq!(threshold_level(5, 0, &t), None);
        // Large values do not overflow.
        assert_eq!(
            threshold_level(i128::from(i64::MAX), i128::from(i64::MAX), &t),
            Some(100)
        );
        assert_eq!(percent_floor(1, 3), Some(33));
        assert_eq!(percent_floor(1, 0), None);
    }

    #[test]
    fn spikes_compare_against_the_hourly_average_and_a_floor() {
        // Baseline $16.80 over 168 hours = $0.10/hour; 3x = $0.30.
        let baseline = 16_800_000;
        assert!(!spike_firing(299_999, baseline, 300, 1));
        assert!(spike_firing(300_000, baseline, 300, 1));
        // The absolute floor gates small amounts, including with no baseline.
        assert!(!spike_firing(999_999, 0, 300, 1_000_000));
        assert!(spike_firing(1_000_000, 0, 300, 1_000_000));
        assert_eq!(spike_ratio(300_000, baseline).as_deref(), Some("3.0"));
        assert_eq!(spike_ratio(1, 0), None);
    }

    #[test]
    fn error_rates_need_volume() {
        assert!(!rate_firing(5, 9, 20, 10));
        assert!(rate_firing(2, 10, 20, 10));
        assert!(!rate_firing(1, 10, 20, 10));
        assert!(!rate_firing(0, 0, 1, 1));
    }

    #[test]
    fn money_is_exact() {
        assert_eq!(usd(0), "$0.00");
        assert_eq!(usd(8_100), "$0.0081");
        assert_eq!(usd(12_400_000), "$12.40");
        assert_eq!(usd(1_234_567), "$1.234567");
    }

    #[test]
    fn interval_parsing() {
        assert_eq!(parse_interval(None).unwrap(), Some(Duration::from_secs(60)));
        assert_eq!(parse_interval(Some("0")).unwrap(), None);
        assert_eq!(
            parse_interval(Some(" 15 ")).unwrap(),
            Some(Duration::from_secs(15))
        );
        assert!(parse_interval(Some("-1")).is_err());
        assert!(parse_interval(Some("86401")).is_err());
        assert!(parse_interval(Some("soon")).is_err());
    }

    #[test]
    fn emails_carry_only_scope_labels_and_strip_controls() {
        let facts = EmailFacts {
            installation: "Acme AI".into(),
            summary: "Workspace monthly budget reached 80%".into(),
            resolved: false,
            critical: false,
            scope: "Team\r\nBcc: x@example.test".into(),
            rule: None,
            at: DateTime::<Utc>::UNIX_EPOCH,
        };
        let (subject, body) = email_text(&facts, Some("https://gw.example.test"));
        assert!(!subject.contains('\n') && !subject.contains('\r'));
        assert!(subject.starts_with("[Acme AI] Alert: Workspace monthly budget reached 80%"));
        assert!(body.contains("Built-in budget alert"));
        assert!(body.contains("https://gw.example.test/notifications"));
        let (subject, _) = email_text(
            &EmailFacts {
                resolved: true,
                ..facts
            },
            None,
        );
        assert!(subject.contains("Resolved:"));
    }
}

#[cfg(all(test, feature = "integration-tests"))]
mod tests;
