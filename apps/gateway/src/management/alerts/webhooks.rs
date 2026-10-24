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
}
const STATUS: &str = "jsonb_build_object('id',d.id,'label',d.label,'enabled',d.enabled,'created_at',d.created_at,'updated_at',d.updated_at,'signing_configured',true,'overlap_active',coalesce(d.previous_secret_until>clock_timestamp(),false),'previous_secret_until',d.previous_secret_until,'waiting',(SELECT count(*) FROM alert_webhook_outbox q WHERE q.destination_id=d.id AND q.state='waiting'),'exhausted',(SELECT count(*) FROM alert_webhook_outbox q WHERE q.destination_id=d.id AND q.state='exhausted'),'last_success_at',(SELECT completed_at FROM alert_webhook_outbox q WHERE q.destination_id=d.id AND q.state='sent' ORDER BY completed_at DESC LIMIT 1),'last_error',(SELECT last_error FROM alert_webhook_outbox q WHERE q.destination_id=d.id AND q.last_error IS NOT NULL ORDER BY created_at DESC,id DESC LIMIT 1)) || CASE WHEN $1 THEN jsonb_build_object('url',d.url,'current_secret_ref',d.current_secret_ref,'previous_secret_ref',d.previous_secret_ref) ELSE '{}'::jsonb END";
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
    let old: Saved=sqlx::query_as("SELECT label,url,enabled,current_secret_ref,previous_secret_ref,previous_secret_until FROM alert_webhook_destinations WHERE id=$1 AND deleted_at IS NULL FOR UPDATE")
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
