//! #102: browser-only configuration and live-authorized occurrence reads.
use super::*;
#[path = "scheduled_report_csv.rs"]
mod csv;

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route(
            "/api/v1/platform/report-schedules/{id}/runs",
            get(platform_runs),
        )
        .route(
            "/api/v1/workspaces/{ws}/report-schedules/{id}/runs",
            get(workspace_runs),
        )
        .route(
            "/api/v1/platform/report-schedules/{id}/runs/{run}/csv",
            get(platform_csv),
        )
        .route(
            "/api/v1/workspaces/{ws}/report-schedules/{id}/runs/{run}/csv",
            get(workspace_csv),
        )
        .route(
            "/api/v1/platform/report-schedules",
            get(platform_list).post(platform_create),
        )
        .route(
            "/api/v1/platform/report-schedules/{id}/runs/{run}",
            get(platform_run),
        )
        .route(
            "/api/v1/workspaces/{ws}/report-schedules/{id}/runs/{run}",
            get(workspace_run),
        )
        .route(
            "/api/v1/platform/report-schedules/{id}",
            get(platform_get)
                .patch(platform_update)
                .delete(platform_delete),
        )
        .route(
            "/api/v1/workspaces/{ws}/report-schedules",
            get(workspace_list).post(workspace_create),
        )
        .route(
            "/api/v1/workspaces/{ws}/report-schedules/{id}",
            get(workspace_get)
                .patch(workspace_update)
                .delete(workspace_delete),
        )
}

/// Unlike legacy metadata boundaries, foreign personal scopes are indistinguishable
/// from absent workspaces (#282). Used by both middleware and handlers.
pub(super) async fn workspace_authority(
    tx: &mut Transaction<'_, Postgres>,
    u: &impl Actor,
    ws: Uuid,
) -> Result<String, ApiError> {
    resources::platform_role_snapshot(tx, u.user_id()).await?;
    let (kind, owner): (String, Option<Uuid>) = sqlx::query_as(
        "SELECT kind,owner_user_id FROM workspaces WHERE id=$1 AND disabled_at IS NULL AND (kind<>'personal' OR owner_user_id=$2)",
    ).bind(ws).bind(u.user_id()).fetch_optional(&mut **tx).await?.ok_or_else(missing)?;
    if kind == "personal" && owner == Some(u.user_id()) {
        return Ok(kind);
    }
    let admin: bool = sqlx::query_scalar("SELECT EXISTS(SELECT FROM effective_workspace_memberships WHERE workspace_id=$1 AND user_id=$2 AND role IN ('owner','admin'))")
        .bind(ws).bind(u.user_id()).fetch_one(&mut **tx).await?;
    if !shared(&kind) || !admin {
        return Err(denied());
    }
    Ok("workspace".into())
}
async fn authority(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    ws: Option<Uuid>,
    write: bool,
) -> Result<String, ApiError> {
    if let Some(ws) = ws {
        return workspace_authority(tx, u, ws).await;
    }
    let role = resources::platform_role_snapshot(tx, u.user_id()).await?;
    if role != "admin" && (write || role != "auditor") {
        return Err(denied());
    }
    Ok("installation".into())
}
/// A single canonical acquisition, before any write/row lock. Shared authority
/// locks intersect the exclusive revocation/suspension/disable locks, as #100.
async fn lock_authority(
    tx: &mut Transaction<'_, Postgres>,
    u: &BrowserPrincipal,
    ws: Option<Uuid>,
    recipients: &[Uuid],
) -> Result<(), ApiError> {
    let scopes = ws
        .map(locks::Scope::Workspace)
        .into_iter()
        .chain(std::iter::once(locks::Scope::User(u.user_id)))
        .chain(recipients.iter().copied().map(locks::Scope::User));
    locks::shared(tx, scopes).await?;
    authority(tx, u, ws, true).await?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    cadence: String,
    timezone: String,
    delivery_time: String,
    #[serde(default = "paused")]
    state: String,
    cost_center_id: Option<Uuid>,
    recipient_user_ids: Vec<Uuid>,
}
fn paused() -> String {
    "paused".into()
}
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    #[serde(default, deserialize_with = "present")]
    cadence: Option<String>,
    #[serde(default, deserialize_with = "present")]
    timezone: Option<String>,
    #[serde(default, deserialize_with = "present")]
    delivery_time: Option<String>,
    #[serde(default, deserialize_with = "present")]
    state: Option<String>,
    #[serde(default, deserialize_with = "present")]
    recipient_user_ids: Option<Vec<Uuid>>,
}
#[cfg(test)]
#[test]
fn management_openapi_money_schedule_request_vectors() {
    // The schema states every constraint validate() can express without the
    // time-zone database or live configuration: cadence/state enums, HH:MM,
    // timezone length and 1..=50 unique recipients. Bind exactly those.
    fn expressible(
        cadence: Option<&str>,
        timezone: Option<&str>,
        time: Option<&str>,
        state: Option<&str>,
        recipients: Option<&[Uuid]>,
    ) -> bool {
        cadence.is_none_or(|v| matches!(v, "weekly" | "monthly"))
            && state.is_none_or(|v| matches!(v, "active" | "paused"))
            && timezone.is_none_or(|v| (1..=64).contains(&v.chars().count()))
            && time.is_none_or(|v| crate::jobs::schedule::parse_hhmm(v).is_some())
            && recipients.is_none_or(|r| {
                (1..=50).contains(&r.len())
                    && r.iter().collect::<std::collections::HashSet<_>>().len() == r.len()
            })
    }
    struct DecodedCreate;
    impl<'de> Deserialize<'de> for DecodedCreate {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            let b = Create::deserialize(d)?;
            expressible(
                Some(&b.cadence),
                Some(&b.timezone),
                Some(&b.delivery_time),
                Some(&b.state),
                Some(&b.recipient_user_ids),
            )
            .then_some(Self)
            .ok_or_else(|| serde::de::Error::custom("constraint"))
        }
    }
    struct DecodedUpdate;
    impl<'de> Deserialize<'de> for DecodedUpdate {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            let b = Update::deserialize(d)?;
            expressible(
                b.cadence.as_deref(),
                b.timezone.as_deref(),
                b.delivery_time.as_deref(),
                b.state.as_deref(),
                b.recipient_user_ids.as_deref(),
            )
            .then_some(Self)
            .ok_or_else(|| serde::de::Error::custom("constraint"))
        }
    }
    for scope in ["/api/v1/platform", "/api/v1/workspaces/{ws}"] {
        super::openapi::requests::vectors::<DecodedCreate>(
            "POST",
            &format!("{scope}/report-schedules"),
            json!({"cadence":"weekly","timezone":"UTC","delivery_time":"00:01","state":"paused","cost_center_id":Uuid::nil(),"recipient_user_ids":[Uuid::nil()]}),
        );
        super::openapi::requests::vectors::<DecodedUpdate>(
            "PATCH",
            &format!("{scope}/report-schedules/{{id}}"),
            json!({"cadence":"monthly","timezone":"UTC","delivery_time":"12:01","state":"paused","recipient_user_ids":[Uuid::nil()]}),
        );
    }
}
fn validate(
    cadence: &str,
    timezone: &str,
    time: &str,
    state: &str,
    recipients: &[Uuid],
    config: Option<&settings::SignIn>,
) -> Result<(), ApiError> {
    if !matches!(cadence, "weekly" | "monthly")
        || !matches!(state, "active" | "paused")
        || timezone.is_empty()
        || timezone.len() > 64
        || jiff::tz::TimeZone::get(timezone).is_err()
        || crate::jobs::schedule::parse_hhmm(time).is_none()
        || recipients.is_empty()
        || recipients.len() > 50
        || recipients
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .len()
            != recipients.len()
    {
        return Err(invalid());
    }
    // Reuse startup-validated, operator-controlled OIDC public origin. Its
    // validation requires HTTPS; loopback HTTP is development/test only.
    if state == "active"
        && !config.is_some_and(|c| c.0.sign_in_summary()["public_url"].as_str().is_some())
    {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "A validated operator public URL is required for active report schedules",
        ));
    }
    Ok(())
}
async fn eligible(
    tx: &mut Transaction<'_, Postgres>,
    kind: &str,
    ws: Option<Uuid>,
    recipients: &[Uuid],
) -> Result<(), ApiError> {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM effective_platform_roles p WHERE p.user_id=ANY($1) AND (($2 IN ('installation','cost_center') AND p.role IN ('admin','auditor')) OR ($2='workspace' AND EXISTS(SELECT FROM effective_workspace_memberships m WHERE m.workspace_id=$3 AND m.user_id=p.user_id AND m.role IN ('owner','admin'))) OR ($2='personal' AND EXISTS(SELECT FROM workspaces w WHERE w.id=$3 AND w.kind='personal' AND w.owner_user_id=p.user_id AND w.disabled_at IS NULL)))")
        .bind(recipients).bind(kind).bind(ws).fetch_one(&mut **tx).await?;
    if count != recipients.len() as i64 {
        return Err(invalid());
    }
    Ok(())
}
pub(super) const LIMIT_REACHED: &str = "The report schedule limit for this scope has been reached";
/// The existing installation management row serializes creates/resumes across
/// different actors; caller also holds the live authority locks. Paused rows
/// count, tombstones do not. Keep the count itself bounded on legacy populations.
async fn capacity(
    tx: &mut Transaction<'_, Postgres>,
    kind: &str,
    ws: Option<Uuid>,
    owner: Uuid,
    creating: bool,
) -> Result<(), ApiError> {
    let (predicate, scope, cap) = match kind {
        "personal" => (
            "scope_kind='personal' AND created_by=$1",
            Some(owner),
            10i64,
        ),
        "workspace" => ("workspace_id=$1", ws, 50),
        _ => ("workspace_id IS NULL AND $1::uuid IS NULL", None, 100),
    };
    let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM (SELECT 1 FROM report_schedules WHERE deleted_at IS NULL AND ({predicate}) LIMIT $2) bounded"))
        .bind(scope).bind(cap+1).fetch_one(&mut **tx).await?;
    if count > cap || (creating && count == cap) {
        return Err(ApiError(StatusCode::CONFLICT, LIMIT_REACHED));
    }
    Ok(())
}
const JSON_ROW: &str = "jsonb_build_object('id',s.id,'scope_kind',s.scope_kind,'workspace_id',s.workspace_id,'cost_center_id',s.cost_center_id,'created_by',s.created_by,'cadence',s.cadence,'timezone',s.timezone,'delivery_time',left(s.delivery_time::text,5),'state',s.state,'configuration_revision',s.configuration_revision::text,'created_at',s.created_at,'updated_at',s.updated_at,'next_run_at',s.next_run_at,'last_period_end',s.last_period_end,'recipient_user_ids',coalesce((SELECT jsonb_agg(r.user_id ORDER BY r.slot) FROM report_schedule_recipients r WHERE r.schedule_id=s.id),'[]'::jsonb))";
async fn row(
    tx: &mut Transaction<'_, Postgres>,
    ws: Option<Uuid>,
    id: Uuid,
) -> Result<Value, ApiError> {
    sqlx::query_scalar(&format!("SELECT {JSON_ROW} FROM report_schedules s WHERE s.id=$1 AND s.workspace_id IS NOT DISTINCT FROM $2 AND s.deleted_at IS NULL"))
        .bind(id).bind(ws).fetch_optional(&mut **tx).await?.ok_or_else(missing)
}
async fn recipients(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    users: &[Uuid],
) -> Result<(), ApiError> {
    sqlx::query("DELETE FROM report_schedule_recipients WHERE schedule_id=$1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO report_schedule_recipients(schedule_id,slot,user_id) SELECT $1,ordinality::smallint,user_id FROM unnest($2::uuid[]) WITH ORDINALITY AS r(user_id,ordinality)")
        .bind(id).bind(users).execute(&mut **tx).await?;
    Ok(())
}
async fn create(
    s: Store,
    u: BrowserPrincipal,
    ws: Option<Uuid>,
    b: Create,
    config: Option<settings::SignIn>,
) -> ApiResult {
    let mut tx = resources::installation_tx(&s).await?;
    let mut kind = authority(&mut tx, &u, ws, true).await?;
    validate(
        &b.cadence,
        &b.timezone,
        &b.delivery_time,
        &b.state,
        &b.recipient_user_ids,
        config.as_ref(),
    )?;
    lock_authority(&mut tx, &u, ws, &b.recipient_user_ids).await?;
    if b.cost_center_id.is_some() {
        if kind == "personal" {
            return Err(invalid());
        }
        if ws.is_none() {
            kind = "cost_center".into();
        }
        // Historical archived assignments are intentionally valid. Cost centers
        // are labels, never authority, and need not be today's workspace label.
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT FROM cost_centers WHERE id=$1)")
                .bind(b.cost_center_id)
                .fetch_one(&mut *tx)
                .await?;
        if !exists {
            return Err(invalid());
        }
    }
    eligible(&mut tx, &kind, ws, &b.recipient_user_ids).await?;
    capacity(&mut tx, &kind, ws, u.user_id, true).await?;
    let id:Uuid = sqlx::query_scalar("INSERT INTO report_schedules(scope_kind,workspace_id,cost_center_id,created_by,cadence,timezone,delivery_time,state) VALUES($1,$2,$3,$4,$5,$6,$7::text::time,$8) RETURNING id")
        .bind(&kind).bind(ws).bind(b.cost_center_id).bind(u.user_id).bind(&b.cadence).bind(&b.timezone).bind(&b.delivery_time).bind(&b.state).fetch_one(&mut *tx).await?;
    recipients(&mut tx, id, &b.recipient_user_ids).await?;
    crate::scheduled_reports::initialize(&mut tx, id).await?;
    audit(
        &mut tx,
        &u,
        ws,
        "report_schedule.created",
        "report_schedule",
        Some(id),
        json!({"kind":kind,"count":b.recipient_user_ids.len()}),
    )
    .await?;
    let value = row(&mut tx, ws, id).await?;
    tx.commit().await?;
    Ok(Json(value))
}
async fn update(
    s: Store,
    u: BrowserPrincipal,
    ws: Option<Uuid>,
    id: Uuid,
    b: Update,
    config: Option<settings::SignIn>,
) -> ApiResult {
    let mut tx = resources::installation_tx(&s).await?;
    authority(&mut tx, &u, ws, true).await?;
    let old = row(&mut tx, ws, id).await?;
    if b.cadence.is_none()
        && b.timezone.is_none()
        && b.delivery_time.is_none()
        && b.state.is_none()
        && b.recipient_user_ids.is_none()
    {
        return Err(invalid());
    }
    let cadence = b
        .cadence
        .as_deref()
        .unwrap_or_else(|| old["cadence"].as_str().unwrap());
    let timezone = b
        .timezone
        .as_deref()
        .unwrap_or_else(|| old["timezone"].as_str().unwrap());
    let time = b
        .delivery_time
        .as_deref()
        .unwrap_or_else(|| old["delivery_time"].as_str().unwrap());
    let state = b
        .state
        .as_deref()
        .unwrap_or_else(|| old["state"].as_str().unwrap());
    let old_users: Vec<Uuid> =
        serde_json::from_value(old["recipient_user_ids"].clone()).map_err(|_| invalid())?;
    let recipients_set = b.recipient_user_ids.is_some();
    let users = b.recipient_user_ids.unwrap_or_else(|| old_users.clone());
    validate(cadence, timezone, time, state, &users, config.as_ref())?;
    lock_authority(&mut tx, &u, ws, &users).await?;
    let kind = old["scope_kind"].as_str().ok_or_else(invalid)?;
    // Stale recipients must never prevent an authorized operator from pausing.
    // Explicit recipient writes and a transition back to active reauthorize.
    if recipients_set || (state == "active" && old["state"] != "active") {
        eligible(&mut tx, kind, ws, &users).await?;
    }
    if state == "active" && old["state"] != "active" {
        let creator: Uuid = old["created_by"]
            .as_str()
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?;
        capacity(&mut tx, kind, ws, creator, false).await?;
    }
    let recipients_changed = users.iter().collect::<std::collections::HashSet<_>>()
        != old_users.iter().collect::<std::collections::HashSet<_>>();
    sqlx::query("UPDATE report_schedules SET cadence=$2,timezone=$3,delivery_time=$4::text::time,state=$5,recipient_revision=recipient_revision+CASE WHEN $6 THEN 1 ELSE 0 END WHERE id=$1 AND deleted_at IS NULL")
        .bind(id).bind(cadence).bind(timezone).bind(time).bind(state).bind(recipients_changed).execute(&mut *tx).await?;
    if recipients_changed {
        recipients(&mut tx, id, &users).await?;
    }
    crate::scheduled_reports::initialize(&mut tx, id).await?;
    let action = if old["state"] != state {
        if state == "paused" {
            "report_schedule.paused"
        } else {
            "report_schedule.resumed"
        }
    } else {
        "report_schedule.updated"
    };
    audit(
        &mut tx,
        &u,
        ws,
        action,
        "report_schedule",
        Some(id),
        json!({"kind":kind,"count":users.len()}),
    )
    .await?;
    let value = row(&mut tx, ws, id).await?;
    tx.commit().await?;
    Ok(Json(value))
}
async fn delete(s: Store, u: BrowserPrincipal, ws: Option<Uuid>, id: Uuid) -> ApiResult {
    let mut tx = resources::installation_tx(&s).await?;
    authority(&mut tx, &u, ws, true).await?;
    lock_authority(&mut tx, &u, ws, &[]).await?;
    let old = row(&mut tx, ws, id).await?;
    sqlx::query("UPDATE report_schedules SET state='paused',deleted_at=clock_timestamp() WHERE id=$1 AND deleted_at IS NULL").bind(id).execute(&mut *tx).await?;
    audit(&mut tx,&u,ws,"report_schedule.deleted","report_schedule",Some(id),json!({"kind":old["scope_kind"],"count":old["recipient_user_ids"].as_array().map_or(0,Vec::len)})).await?;
    tx.commit().await?;
    Ok(Json(json!({"deleted":true})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    #[serde(default = "list_limit")]
    limit: i64,
    after: Option<Uuid>,
}
fn list_limit() -> i64 {
    50
}
async fn list(
    s: Store,
    u: BrowserPrincipal,
    ws: Option<Uuid>,
    q: List,
    config: Option<settings::SignIn>,
) -> ApiResult {
    let mut tx = resources::read_snapshot(&s).await?;
    authority(&mut tx, &u, ws, false).await?;
    if !(1..=100).contains(&q.limit) {
        return Err(invalid());
    }
    // Explicit equality/null and continuation predicates keep the scope+id
    // indexes usable without a null-safe comparison or optional-OR scan.
    let scope = if ws.is_some() {
        "s.workspace_id=$1"
    } else {
        "s.workspace_id IS NULL AND $1::uuid IS NULL"
    };
    let continuation = if q.after.is_some() {
        "s.id>$2"
    } else {
        "$2::uuid IS NULL"
    };
    let mut values:Vec<Value> = sqlx::query_scalar(&format!("SELECT {JSON_ROW} FROM report_schedules s WHERE {scope} AND s.deleted_at IS NULL AND {continuation} ORDER BY s.id LIMIT $3"))
        .bind(ws).bind(q.after).bind(q.limit+1).fetch_all(&mut *tx).await?;
    let has_more = values.len() > q.limit as usize;
    values.truncate(q.limit as usize);
    let next = if has_more {
        values.last().map(|v| v["id"].clone())
    } else {
        None
    };
    let smtp: bool = sqlx::query_scalar("SELECT smtp_host IS NOT NULL AND smtp_from_address IS NOT NULL FROM installation_settings WHERE singleton").fetch_one(&mut *tx).await?;
    let public_url = config.is_some_and(|c| c.0.sign_in_summary()["public_url"].as_str().is_some());
    tx.commit().await?;
    Ok(Json(
        json!({"data":values,"has_more":has_more,"next_cursor":next,"configuration":{"smtp_configured":smtp,"public_url_configured":public_url}}),
    ))
}
async fn get_one(s: Store, u: BrowserPrincipal, ws: Option<Uuid>, id: Uuid) -> ApiResult {
    let mut tx = resources::read_snapshot(&s).await?;
    authority(&mut tx, &u, ws, false).await?;
    let value = row(&mut tx, ws, id).await?;
    tx.commit().await?;
    Ok(Json(value))
}
async fn run_value(
    s: Store,
    u: BrowserPrincipal,
    ws: Option<Uuid>,
    id: Uuid,
    run: Uuid,
) -> Result<Value, ApiError> {
    // Always primary, never a reporting replica or authorization cached at
    // enqueue/send time. A link is an identifier, not a capability.
    let mut tx = resources::read_snapshot(&s).await?;
    authority(&mut tx, &u, ws, false).await?;
    let value: Value = sqlx::query_scalar("SELECT jsonb_build_object('id',r.id,'schedule_id',r.schedule_id,'period_start',r.period_start,'period_end',r.period_end,'configuration_revision',r.configuration_revision::text,'observed_at',r.observed_at,'expires_at',r.expires_at,'expired',r.expires_at<=clock_timestamp() OR p.run_id IS NULL OR p.expires_at<=clock_timestamp(),'snapshot',CASE WHEN r.expires_at>clock_timestamp() AND p.expires_at>clock_timestamp() THEN p.body END,'delivery', (SELECT jsonb_build_object('status',d.status,'recipients',d.recipients,'sent',d.sent,'failed',d.failed,'waiting',d.waiting) FROM alert_deliveries d WHERE d.report_run_id=r.id)) FROM report_runs r JOIN report_schedules s ON s.id=r.schedule_id LEFT JOIN report_snapshots p ON p.run_id=r.id WHERE r.id=$1 AND s.id=$2 AND s.workspace_id IS NOT DISTINCT FROM $3 AND s.deleted_at IS NULL")
        .bind(run).bind(id).bind(ws).fetch_optional(&mut *tx).await?.ok_or_else(missing)?;
    tx.commit().await?;
    Ok(value)
}
async fn read_run(
    s: Store,
    u: BrowserPrincipal,
    ws: Option<Uuid>,
    id: Uuid,
    run: Uuid,
) -> Result<Response, ApiError> {
    let value = run_value(s, u, ws, id, run).await?;
    Ok((
        [
            (axum::http::header::CACHE_CONTROL, "no-store"),
            (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        Json(value),
    )
        .into_response())
}
async fn platform_run(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((id, run)): Path<(Uuid, Uuid)>,
) -> Result<Response, ApiError> {
    read_run(s, u, None, id, run).await
}
async fn workspace_run(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id, run)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<Response, ApiError> {
    read_run(s, u, Some(ws), id, run).await
}
async fn platform_list(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(q): Query<List>,
    config: Option<Extension<settings::SignIn>>,
) -> ApiResult {
    list(s, u, None, q, config.map(|Extension(c)| c)).await
}
async fn workspace_list(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(q): Query<List>,
    config: Option<Extension<settings::SignIn>>,
) -> ApiResult {
    list(s, u, Some(ws), q, config.map(|Extension(c)| c)).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recent {
    #[serde(default = "recent_limit")]
    limit: i64,
}
fn recent_limit() -> i64 {
    20
}
async fn recent_runs(
    s: Store,
    u: BrowserPrincipal,
    ws: Option<Uuid>,
    id: Uuid,
    q: Recent,
) -> Result<Response, ApiError> {
    if !(1..=100).contains(&q.limit) {
        return Err(invalid());
    }
    let mut tx = resources::read_snapshot(&s).await?;
    authority(&mut tx, &u, ws, false).await?;
    row(&mut tx, ws, id).await?;
    let data: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('id',r.id,'schedule_id',r.schedule_id,'period_start',r.period_start,'period_end',r.period_end,'observed_at',r.observed_at,'expires_at',r.expires_at,'expired',r.expires_at<=clock_timestamp(),'skipped_intervals',r.skipped_intervals::text,'delivery',(SELECT jsonb_build_object('status',d.status,'recipients',d.recipients,'sent',d.sent,'failed',d.failed,'waiting',d.waiting) FROM alert_deliveries d WHERE d.report_run_id=r.id)) FROM report_runs r WHERE r.schedule_id=$1 ORDER BY r.period_end DESC,r.id LIMIT $2").bind(id).bind(q.limit).fetch_all(&mut *tx).await?;
    // The append-only outcomes include failed generations, which have no run.
    let outcomes: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('category',category,'created_at',created_at,'period_start',period_start,'period_end',period_end) FROM report_generation_outcomes WHERE schedule_id=$1 ORDER BY created_at DESC,id LIMIT $2").bind(id).bind(q.limit).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok((
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(json!({"data":data,"outcomes":outcomes,"limit":q.limit})),
    )
        .into_response())
}
async fn platform_runs(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    Query(q): Query<Recent>,
) -> Result<Response, ApiError> {
    recent_runs(s, u, None, id, q).await
}
async fn workspace_runs(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, Uuid)>,
    Query(q): Query<Recent>,
) -> Result<Response, ApiError> {
    recent_runs(s, u, Some(ws), id, q).await
}
async fn platform_csv(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((id, run)): Path<(Uuid, Uuid)>,
) -> Result<Response, ApiError> {
    Ok(csv::download(run_value(s, u, None, id, run).await?))
}
async fn workspace_csv(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id, run)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<Response, ApiError> {
    Ok(csv::download(run_value(s, u, Some(ws), id, run).await?))
}
async fn platform_get(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    get_one(s, u, None, id).await
}
async fn workspace_get(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    get_one(s, u, Some(ws), id).await
}
async fn platform_create(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    config: Option<Extension<settings::SignIn>>,
    Json(b): Json<Create>,
) -> ApiResult {
    create(s, u, None, b, config.map(|Extension(c)| c)).await
}
async fn workspace_create(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    config: Option<Extension<settings::SignIn>>,
    Json(b): Json<Create>,
) -> ApiResult {
    create(s, u, Some(ws), b, config.map(|Extension(c)| c)).await
}
async fn platform_update(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
    config: Option<Extension<settings::SignIn>>,
    Json(b): Json<Update>,
) -> ApiResult {
    update(s, u, None, id, b, config.map(|Extension(c)| c)).await
}
async fn workspace_update(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, Uuid)>,
    config: Option<Extension<settings::SignIn>>,
    Json(b): Json<Update>,
) -> ApiResult {
    update(s, u, Some(ws), id, b, config.map(|Extension(c)| c)).await
}
async fn platform_delete(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    delete(s, u, None, id).await
}
async fn workspace_delete(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, id)): Path<(Uuid, Uuid)>,
) -> ApiResult {
    delete(s, u, Some(ws), id).await
}
