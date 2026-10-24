//! Read-only review of aged unknown holds. A retained floor is never spend or
//! an upper bound. No reservation, ledger or totals writes belong here.
use super::*;
use axum::extract::RawQuery;
use chrono::{DateTime, Utc};
use std::collections::HashMap;

pub(super) const AGED: &str = "unknown_since <= observed_at - interval '24 hours'";

pub(super) struct Scope {
    pub(super) user: Uuid,
    pub(super) workspace: Option<Uuid>,
    pub(super) own: Option<Uuid>,
    pub(super) admin: bool,
}
// Logs-style position only, never a credential or visibility scope. Every
// page authorizes afresh and applies the full SQL predicate before aggregates.
// No process-local state: any replica can continue the same position.
const CURSOR_MAX_LEN: usize = 20 + 1 + 36; // signed i64 microseconds + '_' + UUID
fn cursor(since: DateTime<Utc>, id: Uuid) -> String {
    format!("{}_{}", since.timestamp_micros(), id)
}
/// PostgreSQL `timestamptz` spans 4713 BC (Julian day 0, proleptic Gregorian
/// -4713-11-24) to before 294277 AD. chrono is wider on the early side, and
/// binding an unrepresentable instant fails in the server (a 503, not a 400).
fn pg_timestamptz_representable(t: DateTime<Utc>) -> bool {
    use chrono::{Datelike, NaiveDate};
    let min = NaiveDate::from_ymd_opt(-4713, 11, 24)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|t| t.and_utc());
    min.is_some_and(|min| t >= min) && t.year() <= 294_276
}
fn position(value: &str) -> Result<(DateTime<Utc>, Uuid), ApiError> {
    if value.is_empty() || value.len() > CURSOR_MAX_LEN {
        return Err(invalid());
    }
    let (since, id) = crate::management::logs::cursor_parts(value)?;
    if !pg_timestamptz_representable(since) {
        return Err(invalid());
    }
    // Require the exact emitted spelling: no extra fields, whitespace, '+',
    // leading zeros, alternate UUID encodings or out-of-range timestamps.
    if cursor(since, id) != value {
        return Err(invalid());
    }
    Ok((since, id))
}
pub(super) struct Filters {
    limit: usize,
    cursor: Option<String>,
}
impl Filters {
    pub(super) fn parse(raw: Option<String>) -> Result<Self, ApiError> {
        let mut params = HashMap::new();
        if let Some(raw) = raw {
            if raw.len() > 4096 {
                return Err(invalid());
            }
            for pair in raw.split('&') {
                let (k, v) = pair.split_once('=').ok_or_else(invalid)?;
                let k = reports::decode(k)?;
                let v = reports::decode(v)?;
                if params.insert(k, v).is_some() {
                    return Err(invalid());
                }
            }
        }
        let limit = match params.remove("limit") {
            None => 50,
            Some(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => {
                s.parse().map_err(|_| invalid())?
            }
            _ => return Err(invalid()),
        };
        let cursor = params.remove("cursor");
        if !params.is_empty()
            || !(1..=200).contains(&limit)
            || cursor
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > CURSOR_MAX_LEN)
        {
            return Err(invalid());
        }
        Ok(Self { limit, cursor })
    }
}

// Authorization filters precede BOTH detail selection and aggregation. Project
// and Team are siblings; platform personal exposure is aggregated separately,
// never projected as a detail row. Ledger joins include the admission partition.
// Realtime holds are listed but never reconcilable: sessions are valued per
// response and `governance::resolve_usage` refuses an aggregate reconciliation.
const BASE: &str = "WITH clock AS MATERIALIZED (SELECT clock_timestamp() observed_at),
visible AS (
 SELECT r.execution_id,r.workspace_id,w.name workspace_name,w.kind workspace_kind,w.disabled_at IS NULL workspace_enabled,
 e.root_request_id,e.public_model,e.provider,e.workload_kind,e.state execution_state,
 e.billing_usage,r.price_id,p.pricing_version,r.held_microusd,r.unbounded_cost,
 coalesce(l.created_at,r.admitted_at) unknown_since,c.observed_at
 FROM clock c JOIN governance_reservations r ON r.state='unknown' AND r.admitted_at<=c.observed_at-interval '24 hours'
 JOIN workspaces w ON w.id=r.workspace_id
 JOIN api_keys k ON k.workspace_id=r.workspace_id AND k.id=r.api_key_id
 JOIN inference_executions e ON e.id=r.execution_id AND e.started_at=r.admitted_at AND e.workspace_id=r.workspace_id
 LEFT JOIN monetary_ledger l ON l.execution_id=r.execution_id AND l.admitted_at=r.admitted_at AND l.kind='unknown'
 LEFT JOIN deployment_prices p ON p.id=r.price_id
 WHERE ($1::uuid IS NULL OR r.workspace_id=$1)
 AND ($2::uuid IS NULL OR (k.issued_to_user_id=$2 AND k.service_account_id IS NULL))
 AND ($3 OR w.kind<>'personal' OR w.owner_user_id=$4)
), aged AS (SELECT *,floor(extract(epoch FROM observed_at-unknown_since)/3600)::bigint age_hours FROM visible WHERE {AGED}),
shared_rows AS (SELECT * FROM aged WHERE NOT $3 OR workspace_kind IN('team','project'))";
const SUMMARY: &str = "jsonb_build_object('count',count(*)::text,'known_floor_microusd',sum(held_microusd::numeric)::numeric(38,0)::text,'unbounded_count',count(*) FILTER(WHERE unbounded_cost)::text,'oldest_age_hours',max(age_hours))";
const ROW: &str = "jsonb_build_object('execution_id',execution_id,'root_request_id',root_request_id,'workspace',jsonb_build_object('id',workspace_id,'name',workspace_name,'kind',workspace_kind),'public_model',public_model,'provider',provider,'workload_kind',workload_kind,'unknown_since',unknown_since,'age_hours',age_hours,'held_microusd',held_microusd::numeric(38,0)::text,'unbounded_cost',unbounded_cost,'unresolved_reason',CASE WHEN price_id IS NULL THEN 'unpriced' WHEN unbounded_cost THEN 'unbounded_cost_or_unknown_rate' WHEN pricing_version=1 THEN 'incomplete_raw_usage' WHEN pricing_version=3 THEN 'incomplete_or_unpriced_meter_usage' WHEN billing_usage IS NULL THEN 'missing_billing_usage' ELSE 'incomplete_usage_or_cache_allocation' END,'can_reconcile',$5 AND workspace_enabled AND price_id IS NOT NULL AND workload_kind<>'realtime' AND execution_state IN('succeeded','failed','cancelled'))";

pub(super) async fn page(
    tx: &mut Transaction<'_, Postgres>,
    scope: &Scope,
    f: Filters,
) -> Result<Value, ApiError> {
    let after = f.cursor.as_deref().map(position).transpose()?;
    let sql = format!(
        "{}, page AS (SELECT *,{ROW} item FROM shared_rows WHERE ($6::timestamptz IS NULL OR (unknown_since,execution_id)>($6,$7::uuid)) ORDER BY unknown_since,execution_id LIMIT $8) SELECT jsonb_build_object('observed_at',(SELECT observed_at FROM clock),'data',(SELECT coalesce(jsonb_agg(item ORDER BY unknown_since,execution_id),'[]'::jsonb) FROM page),'totals',(SELECT {SUMMARY} FROM shared_rows),'personal',CASE WHEN $3 THEN (SELECT {SUMMARY} FROM aged WHERE workspace_kind='personal') END)",
        BASE.replace("{AGED}", AGED)
    );
    let mut value: Value = sqlx::query_scalar(&sql)
        .bind(scope.workspace)
        .bind(scope.own)
        .bind(scope.workspace.is_none())
        .bind(scope.user)
        .bind(scope.admin)
        .bind(after.map(|p| p.0))
        .bind(after.map(|p| p.1))
        .bind((f.limit + 1) as i64)
        .fetch_one(&mut **tx)
        .await?;
    let rows = value["data"].as_array_mut().ok_or_else(invalid)?;
    let has_more = rows.len() > f.limit;
    rows.truncate(f.limit);
    let next = if has_more {
        let last = rows.last().ok_or_else(invalid)?;
        Some(cursor(
            last["unknown_since"]
                .as_str()
                .ok_or_else(invalid)?
                .parse()
                .map_err(|_| invalid())?,
            last["execution_id"]
                .as_str()
                .ok_or_else(invalid)?
                .parse()
                .map_err(|_| invalid())?,
        ))
    } else {
        None
    };
    value["has_more"] = json!(has_more);
    value["next_cursor"] = json!(next);
    Ok(value)
}
pub(super) async fn workspace(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(ws): Path<Uuid>,
    RawQuery(raw): RawQuery,
) -> ApiResult {
    reports::deadline(async {
        if u.token_id().is_some() {
            return Err(denied());
        }
        let f = Filters::parse(raw)?;
        let mut tx = s.snapshot().await?;
        let a = resources::workspace_access_snapshot(&mut tx, &u, ws).await?;
        let scope = Scope {
            user: u.user_id(),
            workspace: Some(ws),
            own: (!a.view_all_activity).then_some(u.user_id()),
            admin: a.platform_writer,
        };
        let mut tx = s.reporting(tx).await?;
        let value = page(&mut tx, &scope, f).await?;
        tx.commit().await?;
        Ok(Json(value))
    })
    .await
}
pub(super) async fn platform(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    RawQuery(raw): RawQuery,
) -> ApiResult {
    reports::deadline(async {
        let f = Filters::parse(raw)?;
        let mut tx = s.snapshot().await?;
        let role = resources::platform_role_snapshot(&mut tx, u.user_id()).await?;
        if !matches!(role.as_str(), "admin" | "auditor") {
            return Err(denied());
        }
        let scope = Scope {
            user: u.user_id(),
            workspace: None,
            own: None,
            admin: role == "admin",
        };
        let mut tx = s.reporting(tx).await?;
        let value = page(&mut tx, &scope, f).await?;
        tx.commit().await?;
        Ok(Json(value))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cursor_position_has_exact_bounded_format() {
        let id = Uuid::new_v4();
        for micros in [-1, 0, 1_700_000_000_123_456] {
            let since = DateTime::from_timestamp_micros(micros).unwrap();
            assert_eq!(position(&cursor(since, id)).unwrap(), (since, id));
        }
        let nil = Uuid::nil();
        for malformed in [
            String::new(),
            "x".repeat(CURSOR_MAX_LEN + 1),
            format!("+1_{nil}"),
            format!("01_{nil}"),
            format!("-0_{nil}"),
            format!(" 1_{nil}"),
            format!("1_{nil} "),
            format!("1_{nil}_extra"),
            format!("9223372036854775808_{nil}"),
            format!("9223372036854775807_{nil}"),
            format!("-9223372036854775808_{nil}"),
            format!("1_{}", nil.simple()),
            "1_AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA".into(),
            "1_bad".into(),
        ] {
            assert!(position(&malformed).is_err(), "{malformed}");
        }
    }
    #[test]
    fn cursor_rejects_timestamps_postgres_cannot_store() {
        use chrono::NaiveDate;
        let id = Uuid::new_v4();
        let at = |year| {
            NaiveDate::from_ymd_opt(year, 11, 24)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
        };
        // timestamptz starts at Julian day 0 (4713 BC); one microsecond
        // earlier is rejected by the server when bound as $6.
        let min = at(-4713);
        assert_eq!(position(&cursor(min, id)).unwrap(), (min, id));
        for early in [
            min - chrono::Duration::microseconds(1),
            at(-4714),
            at(-100_000),
            DateTime::<Utc>::MIN_UTC,
        ] {
            assert!(position(&cursor(early, id)).is_err(), "{early}");
        }
        // chrono ends at year 262142, below PostgreSQL's 294276 AD limit.
        assert!(position(&cursor(DateTime::<Utc>::MAX_UTC, id)).is_ok());
    }
    #[test]
    fn parameters_are_strict() {
        assert_eq!(Filters::parse(None).unwrap().limit, 50);
        assert_eq!(Filters::parse(Some("limit=200".into())).unwrap().limit, 200);
        for raw in [
            "",
            "limit=0",
            "limit=201",
            "limit=-1",
            "limit=1&limit=2",
            "offset=0",
            "cursor=",
            "limit=%zz",
            "limit=+1",
        ] {
            assert!(Filters::parse(Some(raw.into())).is_err(), "{raw}");
        }
    }
}
