//! Saved platform receivers only. No network I/O or secret resolution here.
use super::*;
use chrono::{DateTime, Utc};

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route("/api/v1/platform/alerts/webhooks", get(list).post(create))
        .route(
            "/api/v1/platform/alerts/webhooks/{id}",
            get(item).patch(update).delete(remove),
        )
        .route("/api/v1/platform/alerts/webhooks/{id}/test", post(test))
        .route(
            "/api/v1/platform/alerts/webhooks/{id}/deliveries",
            get(deliveries),
        )
}
const STATUS: &str = "jsonb_build_object('id',d.id,'label',d.label,'enabled',d.enabled,'created_at',d.created_at,'updated_at',d.updated_at,'signing_configured',true,'overlap_active',coalesce(d.previous_secret_until>clock_timestamp(),false),'previous_secret_until',d.previous_secret_until,'waiting',(SELECT count(*) FROM alert_webhook_outbox q WHERE q.destination_id=d.id AND q.state='waiting' AND q.destination_generation=d.enable_generation),'exhausted',(SELECT count(*) FROM alert_webhook_outbox q WHERE q.destination_id=d.id AND q.state='exhausted'),'last_success_at',(SELECT completed_at FROM alert_webhook_outbox q WHERE q.destination_id=d.id AND q.state='sent' ORDER BY completed_at DESC LIMIT 1),'last_error',(SELECT last_error FROM alert_webhook_outbox q WHERE q.destination_id=d.id AND q.last_error IS NOT NULL ORDER BY created_at DESC,id DESC LIMIT 1)) || CASE WHEN $1 THEN jsonb_build_object('url',d.url,'current_secret_ref',d.current_secret_ref,'previous_secret_ref',d.previous_secret_ref) ELSE '{}'::jsonb END";
async fn read_tx(
    s: &Store,
    u: &ManagementPrincipal,
) -> Result<(Transaction<'static, Postgres>, bool), ApiError> {
    // Token revalidation must be inside a lock-capable transaction, catalog first.
    let mut tx = crate::db::begin(&s.pool).await?;
    resources::catalog_lock(&mut tx, false).await?;
    resources::platform_read(&mut tx, u).await?;
    let admin = resources::platform_role(&mut tx, u.user_id()).await? == "admin"
        && u.as_admin_token()
            .is_none_or(|token| token.scopes.iter().any(|scope| scope == "settings:write"));
    Ok((tx, admin))
}
async fn status(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    admin: bool,
) -> Result<Value, ApiError> {
    sqlx::query_scalar(&format!(
        "SELECT {STATUS} FROM alert_webhook_destinations d WHERE d.id=$2 AND d.deleted_at IS NULL"
    ))
    .bind(admin)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(missing)
}
async fn list(State(s): State<Store>, Extension(u): Extension<ManagementPrincipal>) -> ApiResult {
    let (mut tx, admin) = read_tx(&s, &u).await?;
    let data: Vec<Value> = sqlx::query_scalar(&format!("SELECT {STATUS} FROM alert_webhook_destinations d WHERE d.deleted_at IS NULL ORDER BY d.created_at,d.id"))
        .bind(admin).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"data":data})))
}
async fn item(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let (mut tx, admin) = read_tx(&s, &u).await?;
    let value = status(&mut tx, id, admin).await?;
    tx.commit().await?;
    Ok(Json(value))
}
/// Saved receivers only. The installation management lock in write_tx
/// serializes the audit-log allowance across replicas, including token callers.
async fn test(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(id): Path<Uuid>,
    body: axum::body::Bytes,
) -> ApiResult {
    let mut tx = super::super::settings::write_tx(&s, &u).await?;
    super::super::settings::no_configuration(&body)?;
    let saved: Option<(i64,)> = sqlx::query_as("SELECT enable_generation FROM alert_webhook_destinations d WHERE id=$1 AND enabled AND deleted_at IS NULL AND url IS NOT NULL FOR SHARE OF d")
        .bind(id).fetch_optional(&mut *tx).await?;
    let (generation,) = saved.ok_or(ApiError(
        StatusCode::CONFLICT,
        "An enabled saved destination is required",
    ))?;
    let (mine,destination,all): (i64,i64,i64) = sqlx::query_as("SELECT count(*) FILTER(WHERE actor_user_id=$1 AND created_at>clock_timestamp()-interval '1 minute'),count(*) FILTER(WHERE resource_id=$2 AND created_at>clock_timestamp()-interval '1 minute'),count(*) FROM audit_events WHERE action='alert_webhook.tested' AND created_at>clock_timestamp()-interval '1 hour'")
        .bind(u.user_id()).bind(id).fetch_one(&mut *tx).await?;
    if mine >= 2 || destination >= 2 || all >= 20 {
        return Err(ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many webhook tests; try again later",
        ));
    }
    let delivery = Uuid::new_v4();
    sqlx::query("INSERT INTO alert_webhook_outbox(id,destination_id,destination_generation,test,payload) VALUES($1,$2,$3,true,convert_to(alert_webhook_payload($1,NULL,NULL),'UTF8'))")
        .bind(delivery).bind(id).bind(generation).execute(&mut *tx).await?;
    audit(
        &mut tx,
        &u,
        None,
        "alert_webhook.tested",
        "alert_webhook",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"id":delivery,"state":"waiting"})))
}

/// Bounded metadata only, never payloads, incident ids, URLs or references.
async fn deliveries(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let (mut tx, admin) = read_tx(&s, &u).await?;
    let destination = status(&mut tx, id, admin).await?;
    let counts: Value = sqlx::query_scalar("SELECT jsonb_object_agg(state,n) FROM (SELECT states.state,count(q.id) n FROM unnest(ARRAY['waiting','leased','sent','failed','exhausted','ineligible']) states(state) LEFT JOIN (alert_webhook_outbox q JOIN alert_webhook_destinations d ON d.id=q.destination_id) ON q.destination_id=$1 AND q.state=states.state AND (q.state<>'waiting' OR q.destination_generation=d.enable_generation) GROUP BY states.state) totals")
        .bind(id).fetch_one(&mut *tx).await?;
    let rows: Vec<Value> = sqlx::query_scalar("SELECT jsonb_build_object('id',q.id,'state',q.state,'test',q.test,'attempt_count',q.attempt_count,'created_at',q.created_at,'completed_at',q.completed_at,'next_attempt_at',CASE WHEN q.state='waiting' THEN q.next_attempt_at END,'last_error',q.last_error) FROM alert_webhook_outbox q JOIN alert_webhook_destinations d ON d.id=q.destination_id WHERE q.destination_id=$1 AND (q.state<>'waiting' OR q.destination_generation=d.enable_generation) ORDER BY q.created_at DESC,q.id DESC LIMIT 50")
        .bind(id).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(
        json!({"destination":destination,"counts":counts,"data":rows}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    label: String,
    url: String,
    current_secret_ref: String,
}
fn reference(s: &Store, value: &str) -> Result<(), ApiError> {
    let name = value.strip_prefix("env:").ok_or_else(invalid)?;
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .enumerate()
            .all(|(i, c)| c == b'_' || c.is_ascii_uppercase() || (i > 0 && c.is_ascii_digit()))
        || !s.alert_webhooks.secrets.allows(value)
    {
        return Err(invalid());
    }
    Ok(())
}
fn label(value: &str) -> Result<String, ApiError> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 120 || value.chars().any(char::is_control) {
        return Err(invalid());
    }
    Ok(value.to_owned())
}
async fn record(
    tx: &mut Transaction<'_, Postgres>,
    u: &ManagementPrincipal,
    id: Uuid,
    action: &'static str,
    fields: &[&str],
) -> Result<(), ApiError> {
    audit(
        tx,
        u,
        None,
        action,
        "alert_webhook",
        Some(id),
        json!({"changed_fields":fields}),
    )
    .await
}
async fn create(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    body: axum::body::Bytes,
) -> ApiResult {
    let mut tx = super::super::settings::write_tx(&s, &u).await?;
    let b: Create = serde_json::from_slice(&body).map_err(|_| invalid())?;
    let label = label(&b.label)?;
    let url = s.alert_webhooks.approved_url(&b.url).ok_or_else(invalid)?;
    reference(&s, &b.current_secret_ref)?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM alert_webhook_destinations WHERE deleted_at IS NULL",
    )
    .fetch_one(&mut *tx)
    .await?;
    if count >= 10 {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Maximum ten webhook destinations",
        ));
    }
    let id: Uuid=sqlx::query_scalar("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref) VALUES($1,$2,$3) RETURNING id")
        .bind(label).bind(url).bind(b.current_secret_ref).fetch_one(&mut *tx).await?;
    record(
        &mut tx,
        &u,
        id,
        "alert_webhook.created",
        &["label", "url", "current_secret_ref"],
    )
    .await?;
    let value = status(&mut tx, id, true).await?;
    tx.commit().await?;
    Ok(Json(value))
}
// Preserve absence vs explicit null. Nonnullable fields deserialize T directly
// (so explicit null is refused); nullable rotation fields use T=Option<_>.
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Patch {
    #[serde(default, deserialize_with = "present")]
    label: Option<String>,
    #[serde(default, deserialize_with = "present")]
    enabled: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    current_secret_ref: Option<String>,
    #[serde(default, deserialize_with = "present")]
    previous_secret_ref: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    previous_secret_until: Option<Option<DateTime<Utc>>>,
}
#[test]
fn management_openapi_manual_webhook_request_vectors() {
    use crate::management::openapi::requests::vectors;
    vectors::<Create>(
        "POST",
        "/api/v1/platform/alerts/webhooks",
        json!({"label":"Contract", "url":"https://receiver.example.test/ingest", "current_secret_ref":"env:HOOK_CURRENT"}),
    );
    vectors::<Patch>(
        "PATCH",
        "/api/v1/platform/alerts/webhooks/{id}",
        json!({"label":"Renamed", "enabled":true, "current_secret_ref":"env:HOOK_CURRENT", "previous_secret_ref":"env:HOOK_PREVIOUS", "previous_secret_until":chrono::Utc::now()+chrono::Duration::hours(1)}),
    );
}

#[derive(sqlx::FromRow)]
struct Saved {
    label: String,
    url: String,
    enabled: bool,
    current_secret_ref: String,
    previous_secret_ref: Option<String>,
    previous_secret_until: Option<DateTime<Utc>>,
}
async fn update(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(id): Path<Uuid>,
    body: axum::body::Bytes,
) -> ApiResult {
    let mut tx = super::super::settings::write_tx(&s, &u).await?;
    let b: Patch = serde_json::from_slice(&body).map_err(|_| invalid())?;
    // FOR NO KEY UPDATE, never FOR UPDATE: alert enqueue's outbox foreign key takes
    // FOR KEY SHARE on this row, and a blocked enqueue would hit the
    // evaluator's statement_timeout (57014), which its handler cannot catch.
    let old: Saved=sqlx::query_as("SELECT label,url,enabled,current_secret_ref,previous_secret_ref,previous_secret_until FROM alert_webhook_destinations WHERE id=$1 AND deleted_at IS NULL FOR NO KEY UPDATE")
        .bind(id).fetch_optional(&mut *tx).await?.ok_or_else(missing)?;
    let label = b
        .label
        .as_deref()
        .map(label)
        .transpose()?
        .unwrap_or(old.label.clone());
    let enabled = b.enabled.unwrap_or(old.enabled);
    let current = b
        .current_secret_ref
        .as_ref()
        .unwrap_or(&old.current_secret_ref);
    let previous = b
        .previous_secret_ref
        .as_ref()
        .unwrap_or(&old.previous_secret_ref);
    let until = b.previous_secret_until.unwrap_or(old.previous_secret_until);
    let mut fields = Vec::new();
    if label != old.label {
        fields.push("label");
    }
    if enabled != old.enabled {
        fields.push("enabled");
    }
    if current != &old.current_secret_ref {
        fields.push("current_secret_ref");
    }
    if previous != &old.previous_secret_ref {
        fields.push("previous_secret_ref");
    }
    if until != old.previous_secret_until {
        fields.push("previous_secret_until");
    }
    if previous.is_some() != until.is_some() || previous.as_ref() == Some(current) {
        return Err(invalid());
    }
    let rotating = b.previous_secret_ref.is_some() || b.previous_secret_until.is_some();
    if rotating && let Some(until) = until {
        let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        if until <= now || until > now + chrono::Duration::hours(24) {
            return Err(invalid());
        }
    }
    if b.current_secret_ref.is_some() || enabled {
        reference(&s, current)?;
    }
    if (rotating || enabled)
        && let Some(previous) = previous
    {
        reference(&s, previous)?;
    }
    if enabled && s.alert_webhooks.approved_url(&old.url).is_none() {
        return Err(invalid());
    }
    if !fields.is_empty() {
        sqlx::query("UPDATE alert_webhook_destinations SET label=$2,enabled=$3,current_secret_ref=$4,previous_secret_ref=$5,previous_secret_until=$6 WHERE id=$1")
            .bind(id).bind(label).bind(enabled).bind(current).bind(previous).bind(until).execute(&mut *tx).await?;
        record(
            &mut tx,
            &u,
            id,
            if old.enabled && !enabled {
                "alert_webhook.disabled"
            } else {
                "alert_webhook.updated"
            },
            &fields,
        )
        .await?;
    }
    let value = status(&mut tx, id, true).await?;
    tx.commit().await?;
    Ok(Json(value))
}
async fn remove(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    Path(id): Path<Uuid>,
    body: axum::body::Bytes,
) -> ApiResult {
    let mut tx = super::super::settings::write_tx(&s, &u).await?;
    super::super::settings::no_configuration(&body)?;
    let changed=sqlx::query("UPDATE alert_webhook_destinations SET enabled=false,deleted_at=clock_timestamp() WHERE id=$1 AND deleted_at IS NULL")
        .bind(id).execute(&mut *tx).await?.rows_affected();
    if changed == 0 {
        return Err(missing());
    }
    record(
        &mut tx,
        &u,
        id,
        "alert_webhook.deleted",
        &["enabled", "deleted_at"],
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({"deleted":true})))
}
