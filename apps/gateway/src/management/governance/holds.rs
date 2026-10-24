//! Read-only review of aged unknown holds. A retained floor is never spend or
//! an upper bound. No reservation, ledger or totals writes belong here.
use super::*;
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use axum::extract::RawQuery;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use std::{collections::HashMap, sync::OnceLock};

pub(super) const AGED: &str = "unknown_since <= observed_at - interval '24 hours'";

#[derive(serde::Serialize)]
struct Scope {
    user: Uuid,
    token: Option<Uuid>,
    workspace: Option<Uuid>,
    own: Option<Uuid>,
    admin: bool,
}
impl Scope {
    fn aad(&self) -> Result<Vec<u8>, ApiError> {
        serde_json::to_vec(&("aged-unknown-holds-v1", self)).map_err(|_| invalid())
    }
}

// Ephemeral, replica-local pagination, not a credential. No configuration or
// durable secret is introduced: restarting/switching replicas requires a fresh
// first page. A cursor never replaces the live authorization on each page.
fn cipher() -> Result<Aes256Gcm, ApiError> {
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    if KEY.get().is_none() {
        let key = crate::entropy::bytes().map_err(ApiError::from)?;
        let _ = KEY.set(key);
    }
    Aes256Gcm::new_from_slice(KEY.get().ok_or_else(invalid)?).map_err(|_| invalid())
}
fn cursor(scope: &Scope, since: DateTime<Utc>, id: Uuid) -> Result<String, ApiError> {
    let mut plain = Vec::with_capacity(24);
    plain.extend_from_slice(&since.timestamp_micros().to_be_bytes());
    plain.extend_from_slice(id.as_bytes());
    let nonce: [u8; 12] = crate::entropy::bytes().map_err(ApiError::from)?;
    let encrypted = cipher()?
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: &plain,
                aad: &scope.aad()?,
            },
        )
        .map_err(|_| invalid())?;
    let mut bytes = nonce.to_vec();
    bytes.extend_from_slice(&encrypted);
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn position(scope: &Scope, value: &str) -> Result<(DateTime<Utc>, Uuid), ApiError> {
    let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|_| invalid())?;
    if bytes.len() != 52 || URL_SAFE_NO_PAD.encode(&bytes) != value {
        return Err(invalid());
    }
    let nonce: [u8; 12] = bytes[..12].try_into().map_err(|_| invalid())?;
    let plain = cipher()?
        .decrypt(
            &Nonce::from(nonce),
            Payload {
                msg: &bytes[12..],
                aad: &scope.aad()?,
            },
        )
        .map_err(|_| invalid())?;
    let micros = i64::from_be_bytes(plain[..8].try_into().map_err(|_| invalid())?);
    Ok((
        DateTime::from_timestamp_micros(micros).ok_or_else(invalid)?,
        Uuid::from_slice(&plain[8..]).map_err(|_| invalid())?,
    ))
}
struct Filters {
    limit: usize,
    cursor: Option<String>,
}
impl Filters {
    fn parse(raw: Option<String>) -> Result<Self, ApiError> {
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
            || cursor.as_ref().is_some_and(|s| s.len() != 70)
        {
            return Err(invalid());
        }
        Ok(Self { limit, cursor })
    }
}

// Authorization filters precede BOTH detail selection and aggregation. Project
// and Team are siblings; platform personal exposure is aggregated separately,
// never projected as a detail row. Ledger joins include the admission partition.
const BASE: &str = "WITH clock AS MATERIALIZED (SELECT clock_timestamp() observed_at),
visible AS (
 SELECT r.execution_id,r.workspace_id,w.name workspace_name,w.kind workspace_kind,
 e.root_request_id,e.public_model,e.provider,e.workload_kind,e.state execution_state,
 e.billing_usage,r.price_id,p.pricing_version,r.held_microusd,r.unbounded_cost,
 coalesce(l.created_at,r.admitted_at) unknown_since,c.observed_at
 FROM clock c JOIN governance_reservations r ON r.state='unknown' AND r.admitted_at<=c.observed_at-interval '24 hours'
 JOIN workspaces w ON w.id=r.workspace_id
 JOIN api_keys k ON k.workspace_id=r.workspace_id AND k.id=r.api_key_id
 JOIN inference_executions e ON e.id=r.execution_id AND e.started_at=r.admitted_at AND e.workspace_id=r.workspace_id
 LEFT JOIN monetary_ledger l ON l.execution_id=r.execution_id AND l.admitted_at=r.admitted_at AND l.kind='unknown'
 LEFT JOIN deployment_prices p ON p.id=r.price_id
 WHERE w.disabled_at IS NULL AND ($1::uuid IS NULL OR r.workspace_id=$1)
 AND ($2::uuid IS NULL OR (k.issued_to_user_id=$2 AND k.service_account_id IS NULL))
 AND ($3 OR w.kind<>'personal' OR w.owner_user_id=$4)
), aged AS (SELECT *,floor(extract(epoch FROM observed_at-unknown_since)/3600)::bigint age_hours FROM visible WHERE {AGED}),
shared_rows AS (SELECT * FROM aged WHERE NOT $3 OR workspace_kind IN('team','project'))";
const SUMMARY: &str = "jsonb_build_object('count',count(*)::text,'known_floor_microusd',sum(held_microusd::numeric)::numeric(38,0)::text,'unbounded_count',count(*) FILTER(WHERE unbounded_cost)::text,'oldest_age_hours',max(age_hours))";
const ROW: &str = "jsonb_build_object('execution_id',execution_id,'root_request_id',root_request_id,'workspace',jsonb_build_object('id',workspace_id,'name',workspace_name,'kind',workspace_kind),'public_model',public_model,'provider',provider,'workload_kind',workload_kind,'unknown_since',unknown_since,'age_hours',age_hours,'held_microusd',held_microusd::numeric(38,0)::text,'unbounded_cost',unbounded_cost,'unresolved_reason',CASE WHEN price_id IS NULL THEN 'unpriced' WHEN unbounded_cost THEN 'unbounded_cost_or_unknown_rate' WHEN pricing_version=1 THEN 'incomplete_raw_usage' WHEN pricing_version=3 THEN 'incomplete_or_unpriced_meter_usage' WHEN billing_usage IS NULL THEN 'missing_billing_usage' ELSE 'incomplete_usage_or_cache_allocation' END,'can_reconcile',$5 AND price_id IS NOT NULL AND execution_state IN('succeeded','failed','cancelled'))";

async fn page(
    tx: &mut Transaction<'_, Postgres>,
    scope: &Scope,
    f: Filters,
) -> Result<Value, ApiError> {
    let after = f
        .cursor
        .as_deref()
        .map(|v| position(scope, v))
        .transpose()?;
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
            scope,
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
        )?)
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
            token: None,
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
            token: u.token_id(),
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
    fn cursor_authenticates_every_byte_and_scope() {
        let mut scope = Scope {
            user: Uuid::new_v4(),
            token: None,
            workspace: Some(Uuid::new_v4()),
            own: None,
            admin: false,
        };
        let now = DateTime::from_timestamp_micros(1_700_000_000_123_456).unwrap();
        let id = Uuid::new_v4();
        let c = cursor(&scope, now, id).unwrap();
        assert_eq!(position(&scope, &c).unwrap(), (now, id));
        let bytes = URL_SAFE_NO_PAD.decode(&c).unwrap();
        for i in 0..bytes.len() {
            let mut bad = bytes.clone();
            bad[i] ^= 1;
            assert!(position(&scope, &URL_SAFE_NO_PAD.encode(bad)).is_err());
        }
        scope.user = Uuid::new_v4();
        assert!(position(&scope, &c).is_err());
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
