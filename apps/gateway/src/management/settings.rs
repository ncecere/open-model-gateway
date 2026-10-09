//! Admin › Settings: installation-wide settings in one DB row (0010
//! `installation_settings`) plus the installation display name.
//!
//! - General: display name, support/logo URLs (HTTPS), the maximum lifetime of
//!   new human keys (enforced at create and rotate). Times are UTC.
//! - Data & privacy: the OpenRouter data-collection default and request log
//!   retention. A set environment variable overrides and locks each one.
//!   Prompt and response bodies are never stored (a fact, not a setting).
//! - Email: an SMTP relay for invitations. The password is an allowlisted
//!   `env:NAME` reference, never a value. Test sends are rate-limited from the
//!   audit trail and go only to the signed-in admin's verified address.
//! - Sign-in: read-only OIDC configuration from the server environment.
//! - Storage (Data & privacy): the file store, its health and per-purpose
//!   toggles/retention (`settings/storage.rs`).
//!
//! Admin writes, Auditor reads. Writes are audited inside their transaction.
use super::*;
use crate::{
    email::{self, DeliveryError, OutgoingEmail, SmtpSettings, TlsMode},
    providers::openrouter::DataCollection,
};

#[path = "settings/storage.rs"]
pub(super) mod storage;

pub(super) const KEY_LIFETIME: &str = "Key lifetime exceeds the installation maximum";
pub(super) const SETTING_LOCKED: &str = "This setting is set by the server environment";
pub(super) const REFERENCE_NOT_ALLOWED: &str =
    "The password reference is not on the server allowlist";
pub(super) const PLAINTEXT_REMOTE: &str =
    "Unencrypted delivery is only allowed to a relay on this machine";
pub(super) const EMAIL_NOT_CONFIGURED: &str = "Email delivery is not configured";
pub(super) const EMAIL_TEST_LIMIT: &str = "Too many test emails; wait a minute and try again";
const RETENTION: std::ops::RangeInclusive<i32> = 30..=3650;
const KEY_DAYS: std::ops::RangeInclusive<i32> = 1..=365;
/// Per admin per minute, and installation-wide per hour.
const TESTS_PER_MINUTE: i64 = 2;
const TESTS_PER_HOUR: i64 = 20;

/// Live sign-in status: startup OIDC configuration, JWKS cache and SCIM provisioning.
#[derive(Clone)]
pub(crate) struct SignIn(pub(crate) crate::identity::IdentityState);

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route(
            "/api/v1/platform/settings/general",
            get(general).put(update_general),
        )
        .route(
            "/api/v1/platform/settings/privacy",
            get(privacy).put(update_privacy),
        )
        .route(
            "/api/v1/platform/settings/email",
            get(email_settings).put(update_email),
        )
        .route("/api/v1/platform/settings/email/test", post(test_email))
        .route("/api/v1/platform/settings/sign-in", get(sign_in))
        .merge(storage::routes())
}

async fn write_tx<'a>(
    s: &'a Store,
    u: &BrowserPrincipal,
) -> Result<Transaction<'a, Postgres>, ApiError> {
    let mut tx = resources::installation_tx(s).await?;
    resources::platform_write(&mut tx, u.user_id).await?;
    Ok(tx)
}
async fn read_tx<'a>(
    s: &'a Store,
    u: &BrowserPrincipal,
) -> Result<Transaction<'a, Postgres>, ApiError> {
    let mut tx = resources::installation_tx(s).await?;
    resources::platform_read(&mut tx, u.user_id).await?;
    Ok(tx)
}
fn bad(message: &'static str) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message)
}
/// Trim; empty means absent.
fn text(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}
/// Optional absolute HTTPS URL without credentials or fragment.
fn https_url(value: Option<String>) -> Result<Option<String>, ApiError> {
    let Some(value) = text(value) else {
        return Ok(None);
    };
    if value.len() > 2048 || value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(invalid());
    }
    let url = reqwest::Url::parse(&value).map_err(|_| invalid())?;
    if url.scheme() != "https"
        || url.host_str().is_none_or(str::is_empty)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    Ok(Some(url.to_string()))
}

// ---------- General ----------

const GENERAL_SQL: &str = "SELECT jsonb_build_object('display_name',i.name,'support_url',s.support_url,'logo_url',s.logo_url,'human_key_max_lifetime_days',s.human_key_max_lifetime_days,'timezone','UTC','updated_at',s.updated_at,'updated_by',(SELECT u.email FROM users u WHERE u.id=s.updated_by)) FROM installation i JOIN installation_settings s ON s.singleton WHERE i.singleton";

async fn general(State(s): State<Store>, Extension(u): Extension<BrowserPrincipal>) -> ApiResult {
    let mut tx = read_tx(&s, &u).await?;
    let v: Value = sqlx::query_scalar(GENERAL_SQL).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(v))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GeneralInput {
    display_name: String,
    support_url: Option<String>,
    logo_url: Option<String>,
    human_key_max_lifetime_days: i32,
}
async fn update_general(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<GeneralInput>,
) -> ApiResult {
    let mut tx = write_tx(&s, &u).await?;
    let name = b.display_name.trim();
    if !valid_name(name) || !KEY_DAYS.contains(&b.human_key_max_lifetime_days) {
        return Err(invalid());
    }
    let support = https_url(b.support_url)?;
    let logo = https_url(b.logo_url)?;
    sqlx::query("UPDATE installation SET name=$1 WHERE singleton")
        .bind(name)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE installation_settings SET support_url=$1,logo_url=$2,human_key_max_lifetime_days=$3,updated_at=now(),updated_by=$4 WHERE singleton")
        .bind(support)
        .bind(logo)
        .bind(b.human_key_max_lifetime_days)
        .bind(u.user_id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &u,
        None,
        "settings.general_updated",
        "installation_settings",
        None,
        json!({"count": b.human_key_max_lifetime_days}),
    )
    .await?;
    let v: Value = sqlx::query_scalar(GENERAL_SQL).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(v))
}

/// The installation maximum for new or rotated human (not service-account) keys.
pub(super) async fn check_human_key_lifetime(
    tx: &mut Transaction<'_, Postgres>,
    days: i32,
) -> Result<(), ApiError> {
    let max: i32 = sqlx::query_scalar(
        "SELECT human_key_max_lifetime_days FROM installation_settings WHERE singleton",
    )
    .fetch_one(&mut **tx)
    .await?;
    if days > max {
        return Err(bad(KEY_LIFETIME));
    }
    Ok(())
}

// ---------- Data & privacy ----------

const DATA_COLLECTION_VAR: &str = "GATEWAY_OPENROUTER_DATA_COLLECTION";
const RETENTION_VAR: &str = "GATEWAY_EXECUTION_DETAIL_RETENTION_DAYS";

fn env_retention() -> Option<i32> {
    crate::maintenance::retention_from_env().ok().flatten()
}
fn privacy_json(policy: &str, retention: Option<i32>, updated_at: Value) -> Value {
    let env_policy = DataCollection::env_override();
    let env_days = env_retention();
    let source = |locked: bool| {
        if locked {
            "environment"
        } else {
            "installation"
        }
    };
    json!({
        "openrouter_data_collection": {
            "value": env_policy.map_or(policy, |p| p.as_str()),
            "stored": policy,
            "locked": env_policy.is_some(),
            "source": source(env_policy.is_some()),
            "variable": DATA_COLLECTION_VAR,
        },
        "request_log_retention_days": {
            "value": env_days.or(retention),
            "stored": retention,
            "locked": env_days.is_some(),
            "source": source(env_days.is_some()),
            "variable": RETENTION_VAR,
            "minimum": RETENTION.start(),
            "maximum": RETENTION.end(),
        },
        "prompt_response_storage": "never_stored",
        "updated_at": updated_at,
    })
}
async fn load_privacy(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<(String, Option<i32>, Value), ApiError> {
    Ok(sqlx::query_as("SELECT openrouter_data_collection,request_log_retention_days,to_jsonb(updated_at) FROM installation_settings WHERE singleton FOR NO KEY UPDATE")
        .fetch_one(&mut **tx)
        .await?)
}
async fn privacy(State(s): State<Store>, Extension(u): Extension<BrowserPrincipal>) -> ApiResult {
    let mut tx = read_tx(&s, &u).await?;
    let (policy, retention, updated) = load_privacy(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(privacy_json(&policy, retention, updated)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrivacyInput {
    openrouter_data_collection: String,
    /// Absent or null keeps request metadata until removed by an operator.
    request_log_retention_days: Option<i32>,
}
async fn update_privacy(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<PrivacyInput>,
) -> ApiResult {
    let mut tx = write_tx(&s, &u).await?;
    let policy = DataCollection::parse(&b.openrouter_data_collection).ok_or_else(invalid)?;
    if b.request_log_retention_days
        .is_some_and(|d| !RETENTION.contains(&d))
    {
        return Err(invalid());
    }
    let (stored, stored_retention, _) = load_privacy(&mut tx).await?;
    // A locked setting may be resent unchanged, never changed here.
    if (DataCollection::env_override().is_some() && policy.as_str() != stored)
        || (env_retention().is_some() && b.request_log_retention_days != stored_retention)
    {
        return Err(ApiError(StatusCode::CONFLICT, SETTING_LOCKED));
    }
    sqlx::query("UPDATE installation_settings SET openrouter_data_collection=$1,request_log_retention_days=$2,updated_at=now(),updated_by=$3 WHERE singleton")
        .bind(policy.as_str())
        .bind(b.request_log_retention_days)
        .bind(u.user_id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &u,
        None,
        "settings.privacy_updated",
        "installation_settings",
        None,
        json!({"mode": policy.as_str(), "count": b.request_log_retention_days}),
    )
    .await?;
    let (policy_now, retention, updated) = load_privacy(&mut tx).await?;
    tx.commit().await?;
    // This replica applies it now; others follow on their next maintenance tick.
    DataCollection::set_installation_default(policy);
    Ok(Json(privacy_json(&policy_now, retention, updated)))
}

// ---------- Email ----------

type SmtpRow = (
    Option<String>,
    Option<i32>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);
const SMTP_SQL: &str = "SELECT smtp_host,smtp_port,smtp_tls,smtp_username,smtp_password_ref,smtp_from_address,smtp_from_name FROM installation_settings WHERE singleton";

fn smtp_of(row: SmtpRow) -> Option<SmtpSettings> {
    let (host, port, tls, username, password_ref, from_address, from_name) = row;
    Some(SmtpSettings {
        host: host?,
        port: u16::try_from(port?).ok()?,
        tls: TlsMode::parse(tls.as_deref()?)?,
        username,
        password_ref,
        from_address: from_address?,
        from_name,
    })
}
/// The configured relay, if any.
async fn load_smtp(tx: &mut Transaction<'_, Postgres>) -> Result<Option<SmtpSettings>, ApiError> {
    let row: SmtpRow = sqlx::query_as(SMTP_SQL).fetch_one(&mut **tx).await?;
    Ok(smtp_of(row))
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
async fn email_json(tx: &mut Transaction<'_, Postgres>) -> Result<Value, ApiError> {
    let row: SmtpRow = sqlx::query_as(SMTP_SQL).fetch_one(&mut **tx).await?;
    let (last_at, last_ok, last_error, updated_at): (Option<Value>, Option<bool>, Option<String>, Value) = sqlx::query_as("SELECT to_jsonb(smtp_last_test_at),smtp_last_test_ok,smtp_last_test_error,to_jsonb(updated_at) FROM installation_settings WHERE singleton").fetch_one(&mut **tx).await?;
    let settings = smtp_of(row.clone());
    let (host, port, tls, username, password_ref, from_address, from_name) = row;
    let reference_allowed = password_ref.as_deref().map(email::password_ref_allowed);
    let status = match &settings {
        None => "not_configured",
        Some(s) if !s.credential_available() => "credential_unavailable",
        Some(_) => "ready",
    };
    Ok(json!({
        "configured": settings.is_some(),
        "status": status,
        "host": host,
        "port": port,
        "tls": tls,
        "username": username,
        "password_ref": password_ref,
        "password_ref_allowed": reference_allowed,
        "from_address": from_address,
        "from_name": from_name,
        "public_url_configured": public_url().is_some(),
        "last_test": last_at.map(|at| json!({"at": at, "ok": last_ok, "error": last_error})),
        "updated_at": updated_at,
    }))
}
async fn email_settings(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
) -> ApiResult {
    let mut tx = read_tx(&s, &u).await?;
    let v = email_json(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(v))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmailInput {
    host: Option<String>,
    port: Option<i32>,
    tls: Option<String>,
    username: Option<String>,
    password_ref: Option<String>,
    from_address: Option<String>,
    from_name: Option<String>,
}
/// Validated relay, or `None` to turn delivery off (every field absent).
fn email_input(b: EmailInput) -> Result<Option<SmtpSettings>, ApiError> {
    let host = text(b.host).map(|h| h.to_ascii_lowercase());
    let tls = text(b.tls);
    let username = text(b.username);
    let password_ref = text(b.password_ref);
    let from_address = text(b.from_address).map(|a| a.to_lowercase());
    let from_name = text(b.from_name);
    let Some(host) = host else {
        if b.port.is_some()
            || tls.is_some()
            || username.is_some()
            || password_ref.is_some()
            || from_address.is_some()
            || from_name.is_some()
        {
            return Err(invalid());
        }
        return Ok(None);
    };
    let tls = tls
        .as_deref()
        .and_then(TlsMode::parse)
        .ok_or_else(invalid)?;
    let port = b
        .port
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p > 0)
        .ok_or_else(invalid)?;
    if !email::valid_host(&host) {
        return Err(invalid());
    }
    if tls == TlsMode::None && !email::is_loopback_host(&host) {
        return Err(bad(PLAINTEXT_REMOTE));
    }
    if username.is_some() != password_ref.is_some()
        || username
            .as_deref()
            .is_some_and(|u| u.len() > 256 || u.chars().any(char::is_control))
    {
        return Err(invalid());
    }
    if let Some(r) = password_ref.as_deref() {
        if !email::password_ref_syntax(r) {
            return Err(invalid());
        }
        if !email::password_ref_allowed(r) {
            return Err(bad(REFERENCE_NOT_ALLOWED));
        }
    }
    let from_address = from_address
        .filter(|a| directory::email_valid(a) && a.parse::<lettre::Address>().is_ok())
        .ok_or_else(invalid)?;
    if from_name
        .as_deref()
        .is_some_and(|n| n.chars().count() > 120 || n.chars().any(char::is_control))
    {
        return Err(invalid());
    }
    Ok(Some(SmtpSettings {
        host,
        port,
        tls,
        username,
        password_ref,
        from_address,
        from_name,
    }))
}
async fn update_email(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Json(b): Json<EmailInput>,
) -> ApiResult {
    let mut tx = write_tx(&s, &u).await?;
    let settings = email_input(b)?;
    let c = settings.as_ref();
    // Any change invalidates the previous test result.
    sqlx::query("UPDATE installation_settings SET smtp_host=$1,smtp_port=$2,smtp_tls=$3,smtp_username=$4,smtp_password_ref=$5,smtp_from_address=$6,smtp_from_name=$7,smtp_last_test_at=NULL,smtp_last_test_ok=NULL,smtp_last_test_error=NULL,updated_at=now(),updated_by=$8 WHERE singleton")
        .bind(c.map(|c| c.host.clone()))
        .bind(c.map(|c| i32::from(c.port)))
        .bind(c.map(|c| c.tls.as_str()))
        .bind(c.and_then(|c| c.username.clone()))
        .bind(c.and_then(|c| c.password_ref.clone()))
        .bind(c.map(|c| c.from_address.clone()))
        .bind(c.and_then(|c| c.from_name.clone()))
        .bind(u.user_id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &u,
        None,
        "settings.email_updated",
        "installation_settings",
        None,
        json!({"enabled": c.is_some(), "mode": c.map(|c| c.tls.as_str())}),
    )
    .await?;
    let v = email_json(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(v))
}
/// Send a fixed test message to the signed-in admin's verified address.
async fn test_email(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
) -> ApiResult {
    let mut tx = write_tx(&s, &u).await?;
    let settings = load_smtp(&mut tx)
        .await?
        .ok_or(ApiError(StatusCode::CONFLICT, EMAIL_NOT_CONFIGURED))?;
    // The audit trail is the replica-safe counter (serialized by the installation lock).
    let (mine, all): (i64, i64) = sqlx::query_as("SELECT count(*) FILTER(WHERE actor_user_id=$1 AND created_at>now()-interval '1 minute'),count(*) FROM audit_events WHERE action='settings.email_test' AND created_at>now()-interval '1 hour'")
        .bind(u.user_id)
        .fetch_one(&mut *tx)
        .await?;
    if mine >= TESTS_PER_MINUTE || all >= TESTS_PER_HOUR {
        return Err(ApiError(StatusCode::TOO_MANY_REQUESTS, EMAIL_TEST_LIMIT));
    }
    let (name, updated_at): (String, chrono::DateTime<chrono::Utc>) = sqlx::query_as("SELECT i.name,s.updated_at FROM installation i JOIN installation_settings s ON s.singleton WHERE i.singleton")
        .fetch_one(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &u,
        None,
        "settings.email_test",
        "installation_settings",
        None,
        json!({"mode": settings.tls.as_str()}),
    )
    .await?;
    // Never hold the installation lock across network I/O.
    tx.commit().await?;
    let result = settings
        .send(OutgoingEmail {
            to: u.email.clone(),
            subject: format!("Test email from {name}"),
            body: format!(
                "This is a test email from {name}.\r\n\r\nEmail delivery works: invitations will be sent through this relay.\r\n\r\nYou received it because a Platform Admin chose \"Send test email\" in Admin > Settings > Email.\r\n"
            ),
        })
        .await;
    let error = result.err().map(DeliveryError::as_str);
    // Record the outcome only if the configuration did not change meanwhile.
    sqlx::query("UPDATE installation_settings SET smtp_last_test_at=now(),smtp_last_test_ok=$1,smtp_last_test_error=$2 WHERE singleton AND updated_at=$3")
        .bind(error.is_none())
        .bind(error)
        .bind(updated_at)
        .execute(&s.pool)
        .await?;
    Ok(Json(
        json!({"ok": error.is_none(), "error": error, "recipient": u.email}),
    ))
}

/// Invitation email prepared inside the invite transaction and sent after commit.
pub(super) struct InvitationMail {
    settings: SmtpSettings,
    installation: String,
    workspace: String,
}
pub(super) async fn invitation_mail(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
) -> Result<Option<InvitationMail>, ApiError> {
    let Some(settings) = load_smtp(tx).await? else {
        return Ok(None);
    };
    let (installation, workspace): (String, String) = sqlx::query_as(
        "SELECT i.name,w.name FROM installation i JOIN workspaces w ON w.id=$1 WHERE i.singleton",
    )
    .bind(ws)
    .fetch_one(&mut **tx)
    .await?;
    Ok(Some(InvitationMail {
        settings,
        installation,
        workspace,
    }))
}
/// `sent`, `failed` or `not_configured`. The code travels in the body only
/// (never a URL); bodies and outcomes beyond the category are not logged.
pub(super) async fn send_invitation(
    mail: Option<InvitationMail>,
    to: &str,
    role: &str,
    token: &str,
) -> &'static str {
    let Some(mail) = mail else {
        return "not_configured";
    };
    let accept = public_url().map_or_else(
        || "open the gateway".to_owned(),
        |url| format!("go to {url}/invitations/accept"),
    );
    let body = format!(
        "You have been invited to join \"{workspace}\" as {role} in {installation}.\r\n\r\nTo accept, {accept}, sign in with this email address, choose \"Accept invitation\" and enter this invitation code:\r\n\r\n{token}\r\n\r\nThe code expires in 3 days and only works for {to}. Keep it private. If you were not expecting this invitation, you can ignore this email.\r\n",
        workspace = mail.workspace,
        installation = mail.installation,
    );
    match mail
        .settings
        .send(OutgoingEmail {
            to: to.to_owned(),
            subject: format!("Invitation to {} in {}", mail.workspace, mail.installation),
            body,
        })
        .await
    {
        Ok(()) => "sent",
        Err(_) => "failed",
    }
}

// ---------- Sign-in ----------

async fn sign_in(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    config: Option<Extension<SignIn>>,
) -> ApiResult {
    let mut tx = read_tx(&s, &u).await?;
    let mappings: i64 =
        sqlx::query_scalar("SELECT count(*) FROM oidc_group_mappings WHERE enabled")
            .fetch_one(&mut *tx)
            .await?;
    let scim = config.as_ref().and_then(|Extension(c)| c.0.scim());
    let scim = crate::scim::summary(&mut tx, scim.as_deref()).await?;
    tx.commit().await?;
    let mut v = config.map_or_else(
        || json!({"enabled": false}),
        |Extension(c)| c.0.sign_in_summary(),
    );
    v["enabled_group_mappings"] = json!(mappings);
    v["scim"] = scim;
    Ok(Json(v))
}
