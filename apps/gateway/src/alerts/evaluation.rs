//! What each rule kind watches: the queries and exact comparisons that turn one
//! rule (or the built-in personal budget alert) into its firing conditions.
use super::*;

// ---------- Evaluation ----------

pub(super) type BudgetRow = (Uuid, String, Option<Uuid>, String, i64, String, i64);

pub(super) const WORKSPACE_BUDGETS: &str = r#"WITH ws AS (SELECT id,kind FROM workspaces WHERE disabled_at IS NULL AND CASE $1::text WHEN 'shared' THEN kind IN ('team','project') WHEN 'personal' THEN kind='personal' ELSE id=$2 AND kind IN ('team','project') END),
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

pub(super) fn windows(now: DateTime<Utc>) -> [(DateTime<Utc>, DateTime<Utc>); 4] {
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

pub(super) async fn budget_conditions(
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

pub(super) async fn evaluate_rule(
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
        // Never a rule kind (no `alert_rules` row can have it); evaluated by
        // `key_expiring_tick` instead, outside the rule/builtin job loop.
        Some(Kind::KeyExpiring | Kind::AgedUnknownHold) => Ok(vec![]),
        None => Ok(vec![]),
    }
}
