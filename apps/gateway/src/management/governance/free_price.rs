//! Explicit zero-cost resolution; usage is never inferred from a free price.
use super::*;
use crate::governance::locks::{self, Scope};
use crate::timestamp::TimestampFormat as _;
use gateway_datetime_compat::UtcInstant;
use sqlx::{Postgres, Transaction};

const MAX_CANDIDATES: usize = 10_000;
type MonthBounds = (UtcInstant, UtcInstant);
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Resolve {
    #[serde(default)]
    apply: bool,
    month: Option<String>,
    workspace_id: Option<Uuid>,
    deployment_id: Option<Uuid>,
    expected_count: Option<String>,
    reason: Option<String>,
    #[serde(default)]
    offset: Option<String>,
}
impl Resolve {
    fn bounds(&self) -> Result<Option<MonthBounds>, ApiError> {
        let Some(month) = &self.month else {
            return Ok(None);
        };
        if month.len() != 7 {
            return Err(invalid());
        }
        let start = gateway_datetime_compat::http::parse_date_filter(&format!("{month}-01"))
            .map_err(|_| invalid())?;
        if start.format("%Y-%m") != *month || month.starts_with("0000") {
            return Err(invalid());
        }
        let end = start.checked_add_months(1).ok().ok_or_else(invalid)?;
        Ok(Some((
            start.midnight().map_err(|_| invalid())?,
            end.midnight().map_err(|_| invalid())?,
        )))
    }
    fn validate(&self) -> Result<(), ApiError> {
        self.bounds()?;
        self.offset()?;
        if self.reason.as_ref().is_some_and(|r| {
            r.trim().is_empty() || r.chars().count() > 150 || r.chars().any(char::is_control)
        }) {
            return Err(invalid());
        }
        if let Some(n) = &self.expected_count
            && (money(n, false)? > MAX_CANDIDATES as i64
                || n.parse::<usize>().map_err(|_| invalid())?.to_string() != *n)
        {
            return Err(invalid());
        }
        if self.apply && (self.reason.is_none() || self.expected_count.is_none()) {
            return Err(invalid());
        }
        Ok(())
    }
    fn offset(&self) -> Result<i64, ApiError> {
        let raw = self.offset.as_deref().unwrap_or("0");
        let offset = money(raw, false)?;
        if offset.to_string() != raw || offset > i64::MAX - MAX_CANDIDATES as i64 {
            return Err(invalid());
        }
        Ok(offset)
    }
}
#[derive(sqlx::FromRow)]
struct Candidate {
    execution_id: Uuid,
    workspace_id: Uuid,
    api_key_id: Uuid,
    deployment_id: Uuid,
    price_id: Uuid,
    price_tier: String,
    admitted_at: UtcInstant,
    workload_kind: String,
}
// No private projection ever leaves this module. A zero hold alone is not proof:
// prices are checked separately against their immutable pinned versions.
const CANDIDATES: &str = "SELECT r.execution_id,r.workspace_id,r.api_key_id,r.deployment_id,r.price_id,r.price_tier,r.admitted_at,e.workload_kind
 FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id AND e.started_at=r.admitted_at
 WHERE r.state='unknown' AND r.held_microusd=0 AND NOT r.unbounded_cost AND r.price_id IS NOT NULL
 AND e.state<>'started' AND e.workload_kind<>'realtime'
 AND (e.workload_kind NOT IN ('videos','batches') OR EXISTS(SELECT 1 FROM async_jobs j WHERE j.execution_id=r.execution_id AND j.state IN ('completed','failed','cancelled','expired')))
 AND ($1::timestamptz IS NULL OR r.admitted_at >= $1) AND ($2::timestamptz IS NULL OR r.admitted_at < $2)
 AND ($3::uuid IS NULL OR r.workspace_id=$3) AND ($4::uuid IS NULL OR r.deployment_id=$4)
 AND ($5::uuid[] IS NULL OR (r.execution_id,r.admitted_at) IN (SELECT * FROM unnest($5::uuid[],$6::timestamptz[])))
 ORDER BY r.admitted_at,r.execution_id LIMIT 10001 OFFSET $7";
async fn candidates(
    tx: &mut Transaction<'_, Postgres>,
    p: &Resolve,
    ids: Option<&[Uuid]>,
    ats: Option<&[UtcInstant]>,
    lock: bool,
) -> Result<Vec<Candidate>, ApiError> {
    let bounds = p.bounds()?;
    let rows = sqlx::query_as::<_, Candidate>(&format!(
        "{CANDIDATES}{}",
        if lock { " FOR UPDATE OF r" } else { "" }
    ))
    .bind(bounds.map(|b| b.0))
    .bind(bounds.map(|b| b.1))
    .bind(p.workspace_id)
    .bind(p.deployment_id)
    .bind(ids)
    .bind(ats)
    .bind(if ids.is_none() { p.offset()? } else { 0 })
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows)
}

async fn eligible(
    tx: &mut Transaction<'_, Postgres>,
    rows: Vec<Candidate>,
) -> Result<Vec<Candidate>, ApiError> {
    let mut selected = Vec::new();
    let mut prices = std::collections::BTreeMap::new();
    for row in rows {
        let key = (
            row.price_id,
            row.deployment_id,
            row.price_tier.clone(),
            row.workload_kind == "videos",
        );
        let free = if let Some(free) = prices.get(&key) {
            *free
        } else {
            let free = crate::governance::entirely_free_price(tx, key.0, key.1, &key.2, key.3)
                .await
                .map_err(|_| {
                    ApiError(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Pinned pricing unavailable",
                    )
                })?;
            prices.insert(key, free);
            free
        };
        if free {
            selected.push(row);
        }
    }
    Ok(selected)
}
pub(super) async fn resolve(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Json(p): Json<Resolve>,
) -> ApiResult {
    p.validate()?;
    let offset = p.offset()?;
    // This operation intentionally has no token delegation, even if a caller
    // bypasses route middleware in an internal harness.
    if u.token_id().is_some() {
        return Err(denied());
    }
    let mut tx = crate::db::begin(&s.pool).await?;
    resources::catalog_lock(&mut tx, false).await?;
    resources::require_platform_admin(
        &resources::platform_role_snapshot(&mut tx, u.user_id()).await?,
    )?;
    let mut initial = candidates(&mut tx, &p, None, None, false).await?;
    let more = initial.len() > MAX_CANDIDATES;
    initial.truncate(MAX_CANDIDATES);
    let scanned = initial.len() as i64;
    locks::shared(
        &mut tx,
        initial
            .iter()
            .map(|r| Scope::Workspace(r.workspace_id))
            .chain([Scope::User(u.user_id())]),
    )
    .await?;
    resources::platform_write(&mut tx, &u).await?;
    // Restrict the second read to scopes already locked, so newly arriving
    // unknowns cannot introduce an earlier authority scope after the actor.
    let ids: Vec<_> = initial.iter().map(|r| r.execution_id).collect();
    let ats: Vec<_> = initial.iter().map(|r| r.admitted_at).collect();
    let rows = candidates(&mut tx, &p, Some(&ids), Some(&ats), p.apply).await?;
    let eligible = eligible(&mut tx, rows).await?;
    let count = eligible.len().to_string();
    if p.apply {
        if p.expected_count.as_deref() != Some(count.as_str()) {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "Eligible count changed; preview again",
            ));
        }
        let touches: Vec<_> = eligible
            .iter()
            .map(|r| locks::Touch {
                workspace: r.workspace_id,
                api_key: r.api_key_id,
                at: r.admitted_at,
            })
            .collect();
        locks::rows(&mut tx, &touches, false).await?;
        let reason = p.reason.as_deref().ok_or_else(invalid)?;
        for row in &eligible {
            settle(&mut tx, row, u.user_id(), reason).await?;
        }
        audit(&mut tx, &eligible, u.user_id(), reason).await?;
    }
    // Applying removes resolved rows from the candidate sequence. Continue
    // after the remaining rows of this page, without skipping the next page.
    let removed = if p.apply { eligible.len() as i64 } else { 0 };
    let next = more.then(|| (offset + scanned - removed).to_string());
    tx.commit().await?;
    Ok(Json(
        json!({"eligible_count":count,"resolved_count":if p.apply {eligible.len().to_string()} else {"0".into()},"next_offset":next}),
    ))
}
async fn settle(
    tx: &mut Transaction<'_, Postgres>,
    r: &Candidate,
    actor: Uuid,
    reason: &str,
) -> Result<(), ApiError> {
    sqlx::query("UPDATE governance_reservations SET state='settled',actual_microusd=0,free_price_resolved=true WHERE execution_id=$1 AND admitted_at=$2 AND state='unknown'")
        .bind(r.execution_id).bind(r.admitted_at).execute(&mut **tx).await?;
    // INSERT SELECT copies observations exactly, including SQL NULLs. Neither
    // history surface is assigned fabricated tokens, meters or cost components.
    sqlx::query("INSERT INTO monetary_ledger(id,execution_id,admitted_at,kind,amount_microusd,input_tokens,output_tokens,billing_usage,meter_usage,output_image_variant,provider_cost_microusd,evidence) SELECT gen_random_uuid(),execution_id,admitted_at,'reconciliation',0,input_tokens,output_tokens,billing_usage,meter_usage,output_image_variant,provider_cost_microusd,$3 FROM governance_reservations WHERE execution_id=$1 AND admitted_at=$2")
        .bind(r.execution_id).bind(r.admitted_at).bind(format!("free-price:{actor}:{reason}")).execute(&mut **tx).await?;
    Ok(())
}
async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    rows: &[Candidate],
    actor: Uuid,
    reason: &str,
) -> Result<(), ApiError> {
    let mut counts = std::collections::BTreeMap::<Uuid, u64>::new();
    for row in rows {
        *counts.entry(row.workspace_id).or_default() += 1;
    }
    // Actor-owned exports include the actor's audit rows. Keep even those
    // rows aggregate-only: no private execution/key identity in any audit field.
    for (workspace, count) in counts {
        sqlx::query("INSERT INTO audit_events(id,workspace_id,actor_user_id,action,resource_type,resource_id,metadata) VALUES(gen_random_uuid(),$1,$2,'usage.free_price_resolved','workspace',$1,$3)")
            .bind(workspace).bind(actor).bind(json!({"reason":reason,"resolved_count":count.to_string()})).execute(&mut **tx).await?;
    }
    Ok(())
}

#[test]
fn management_openapi_free_price_request_vectors() {
    crate::management::openapi::requests::vectors::<Resolve>(
        "POST",
        "/api/v1/platform/reservations/resolve-free-price",
        json!({"apply":true,"month":"2026-01","workspace_id":Uuid::nil(),"deployment_id":Uuid::nil(),"expected_count":"0","reason":"operator reference"}),
    );
}
