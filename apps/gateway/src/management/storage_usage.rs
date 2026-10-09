//! Usage & costs › Storage: GB-days of the file store per workspace and
//! purpose, from the append-only hourly rows (`storage_usage_hours`, 0020).
//!
//! Storage is **not charged**: every response carries `cost_state:
//! "not_charged"`, never an unknown or a $0 estimate. Quantities are exact
//! integer byte-seconds (strings) plus GB-days (1 GB = 2^30 bytes) rounded to
//! six decimals for display.
//!
//! Visibility: a workspace report needs workspace-wide visibility (workspace
//! admins, the personal owner). The platform report (Admins/Auditors) lists
//! per-workspace totals; personal workspaces show totals only (no purpose split).
use super::requests::strict_date;
use super::*;
use crate::filestore::{files::QUOTA_SQL, usage::GB_DAY_BYTE_SECONDS};
use chrono::{NaiveDate, TimeDelta, Utc};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StorageQuery {
    start_date: String,
    end_date: String,
}

/// `[start, end)` in whole UTC days, 1..=366 days, not after tomorrow.
fn period(q: &StorageQuery) -> Result<(NaiveDate, NaiveDate), ApiError> {
    let (start, end) = (strict_date(&q.start_date)?, strict_date(&q.end_date)?);
    let days = (end - start).num_days();
    let tomorrow = Utc::now()
        .date_naive()
        .checked_add_signed(TimeDelta::days(1))
        .ok_or_else(invalid)?;
    if !(1..=366).contains(&days) || end > tomorrow {
        return Err(invalid());
    }
    Ok((start, end))
}

/// GB-days (2^30 bytes for 24 h) from exact byte-seconds, rounded half up to
/// six decimals, without trailing zeros.
pub(crate) fn gb_days(byte_seconds: &str) -> Result<String, ApiError> {
    let bs: i128 = byte_seconds.parse().map_err(|_| invalid())?;
    let unit = i128::from(GB_DAY_BYTE_SECONDS);
    let micro = (bs * 1_000_000 + unit / 2) / unit;
    let (whole, frac) = (micro / 1_000_000, micro % 1_000_000);
    let text = format!("{whole}.{frac:06}");
    Ok(text.trim_end_matches('0').trim_end_matches('.').to_owned())
}

fn amount(byte_seconds: &str) -> Result<Value, ApiError> {
    Ok(json!({"byte_seconds": byte_seconds, "gb_days": gb_days(byte_seconds)?}))
}

fn envelope(start: NaiveDate, end: NaiveDate, through: DateTimeUtc) -> Value {
    json!({
        "period": {"start_date": start.to_string(), "end_date": end.to_string(), "timezone": "UTC"},
        "unit": "gb_day",
        "gb_bytes": "1073741824",
        "cost_state": "not_charged",
        "recorded_through": through,
    })
}
type DateTimeUtc = chrono::DateTime<Utc>;

async fn recorded_through(tx: &mut Transaction<'_, Postgres>) -> Result<DateTimeUtc, ApiError> {
    Ok(
        sqlx::query_scalar("SELECT recorded_through FROM storage_usage_progress WHERE singleton")
            .fetch_one(&mut **tx)
            .await?,
    )
}

const RANGE: &str = "hour_start>=($2::date::timestamp AT TIME ZONE 'UTC') AND hour_start<($3::date::timestamp AT TIME ZONE 'UTC')";

pub(super) async fn workspace_storage_usage(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(q): Query<StorageQuery>,
) -> ApiResult {
    let (start, end) = period(&q)?;
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    if !a.view_all_activity {
        return Err(denied());
    }
    let through = recorded_through(&mut tx).await?;
    let purposes: Vec<(String, String)> = sqlx::query_as(&format!("SELECT purpose,sum(byte_seconds)::text FROM storage_usage_hours WHERE workspace_id=$1 AND {RANGE} GROUP BY purpose ORDER BY purpose"))
        .bind(ws)
        .bind(start)
        .bind(end)
        .fetch_all(&mut *tx)
        .await?;
    let daily: Vec<(String, String)> = sqlx::query_as(&format!("SELECT ((hour_start AT TIME ZONE 'UTC')::date)::text,sum(byte_seconds)::text FROM storage_usage_hours WHERE workspace_id=$1 AND {RANGE} GROUP BY 1 ORDER BY 1"))
        .bind(ws)
        .bind(start)
        .bind(end)
        .fetch_all(&mut *tx)
        .await?;
    let current = crate::filestore::files::workspace_storage(&mut *tx, ws)
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Management storage unavailable",
            )
        })?;
    tx.commit().await?;
    let total: i128 = purposes
        .iter()
        .map(|(_, b)| b.parse::<i128>().unwrap_or_default())
        .sum();
    let mut v = envelope(start, end, through);
    v["scope"] = json!("workspace");
    v["total"] = amount(&total.to_string())?;
    v["by_purpose"] = Value::Array(
        purposes
            .iter()
            .map(|(p, b)| {
                let mut row = amount(b)?;
                row["purpose"] = json!(p);
                Ok(row)
            })
            .collect::<Result<_, ApiError>>()?,
    );
    v["daily"] = Value::Array(
        daily
            .iter()
            .map(|(d, b)| {
                let mut row = amount(b)?;
                row["date"] = json!(d);
                Ok(row)
            })
            .collect::<Result<_, ApiError>>()?,
    );
    v["current"] = json!({"used_bytes": current.used_bytes, "quota_bytes": current.quota_bytes});
    Ok(Json(v))
}

type PlatformRow = (
    Uuid,
    String,
    String,
    String,
    i64,
    Option<i64>,
    Option<Value>,
);

pub(super) async fn platform_storage_usage(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(q): Query<StorageQuery>,
) -> ApiResult {
    let (start, end) = period(&q)?;
    let mut tx = resources::installation_tx(&s).await?;
    resources::platform_read(&mut tx, u.user_id).await?;
    sqlx::query("SET LOCAL statement_timeout='10s'")
        .execute(&mut *tx)
        .await?;
    let through = recorded_through(&mut tx).await?;
    let used = crate::filestore::files::used_by_workspace_sql();
    let quota = QUOTA_SQL.replace("WHERE w.id=$1", "WHERE w.id=x.id");
    // Workspaces with usage in the period or bytes stored now; personal
    // workspaces get totals only.
    let rows: Vec<PlatformRow> = sqlx::query_as(&format!("WITH u AS (SELECT workspace_id,purpose,sum(byte_seconds) bs FROM storage_usage_hours WHERE {RANGE} GROUP BY 1,2), cur AS ({used}), ids AS (SELECT workspace_id id FROM u UNION SELECT workspace_id FROM cur) SELECT w.id,w.name,w.kind,coalesce((SELECT sum(bs) FROM u WHERE u.workspace_id=w.id),0)::text,coalesce(cur.bytes,0)::bigint,({quota_sub}),CASE WHEN w.kind='personal' THEN NULL ELSE (SELECT coalesce(jsonb_agg(jsonb_build_object('purpose',u.purpose,'byte_seconds',u.bs::text) ORDER BY u.purpose),'[]'::jsonb) FROM u WHERE u.workspace_id=w.id) END FROM ids x JOIN workspaces w ON w.id=x.id LEFT JOIN cur ON cur.workspace_id=w.id WHERE $1::boolean ORDER BY coalesce((SELECT sum(bs) FROM u WHERE u.workspace_id=w.id),0) DESC,w.name,w.id LIMIT 500", quota_sub = quota))
        .bind(true)
        .bind(start)
        .bind(end)
        .fetch_all(&mut *tx)
        .await?;
    let totals: Vec<(String, String)> = sqlx::query_as(&format!("SELECT purpose,sum(byte_seconds)::text FROM storage_usage_hours WHERE $1::boolean AND {RANGE} GROUP BY purpose ORDER BY purpose"))
        .bind(true)
        .bind(start)
        .bind(end)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    let total: i128 = totals
        .iter()
        .map(|(_, b)| b.parse::<i128>().unwrap_or_default())
        .sum();
    let mut v = envelope(start, end, through);
    v["scope"] = json!("platform");
    v["total"] = amount(&total.to_string())?;
    v["by_purpose"] = Value::Array(
        totals
            .iter()
            .map(|(p, b)| {
                let mut row = amount(b)?;
                row["purpose"] = json!(p);
                Ok(row)
            })
            .collect::<Result<_, ApiError>>()?,
    );
    v["workspaces"] = Value::Array(
        rows.into_iter()
            .map(|(id, name, kind, bs, bytes, quota, purposes)| {
                let mut row = amount(&bs)?;
                row["workspace_id"] = json!(id);
                row["name"] = json!(name);
                row["kind"] = json!(kind);
                row["current_bytes"] = json!(bytes);
                row["quota_bytes"] = json!(quota);
                row["by_purpose"] = match purposes {
                    Some(Value::Array(list)) => Value::Array(
                        list.into_iter()
                            .map(|mut p| {
                                let b = p["byte_seconds"].as_str().unwrap_or("0").to_owned();
                                p["gb_days"] = json!(gb_days(&b)?);
                                Ok(p)
                            })
                            .collect::<Result<_, ApiError>>()?,
                    ),
                    _ => Value::Null,
                };
                Ok(row)
            })
            .collect::<Result<_, ApiError>>()?,
    );
    Ok(Json(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gb_days_are_exact_and_rounded() {
        let day = GB_DAY_BYTE_SECONDS.to_string();
        assert_eq!(gb_days(&day).unwrap(), "1");
        assert_eq!(gb_days("0").unwrap(), "0");
        assert_eq!(
            gb_days(&(GB_DAY_BYTE_SECONDS / 2).to_string()).unwrap(),
            "0.5"
        );
        // 1 MiB stored for one hour.
        assert_eq!(
            gb_days(&(1_048_576i64 * 3600).to_string()).unwrap(),
            "0.000041"
        );
        assert!(gb_days("x").is_err());
    }
}
