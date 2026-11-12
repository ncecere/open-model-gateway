//! SCIM 2.0 provisioning: the RFC 7643/7644 subset used by Okta and Microsoft Entra ID.
//!
//! - Disabled unless `GATEWAY_SCIM_TOKEN_ENV` names an environment variable holding the
//!   bearer token; then every `/scim` path is 404. The token is read once at startup and
//!   only its SHA-256 hash is kept (compared in constant time); it is never stored or logged.
//! - A SCIM User is a gateway user (`id` = user UUID). Provisioning grants nothing by itself.
//!   Deactivation (`active:false` or DELETE) suspends the user: sessions and the user's
//!   personal/issued keys are revoked. Reactivation never restores revoked keys.
//! - A pushed Group becomes "SCIM-managed": its membership feeds the configured issuer's
//!   group mappings with group provenance (manual grants are never touched), and sign-in
//!   no longer takes that group's membership from the token.
//! - Attributes outside the published schema subset are accepted but not stored.
//! - SCIM never removes platform access from the last active Platform Admin: a write
//!   that would leave none is rolled back whole, answered with 409, audited, and raises
//!   the built-in installation alert (see [`finish`]).
use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, get},
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{identity::ScimRuntime, lifecycle, store::Store};

mod discovery;
mod groups;
mod users;

use discovery::{resource_type, resource_types, schema, schemas, service_provider_config};
pub(crate) use groups::effective_groups;
use groups::{create_group, delete_group, get_group, list_groups, patch_group, replace_group};
use users::{create_user, delete_user, get_user, list_users, patch_user, replace_user};

const USER_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
const GROUP_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
const LIST_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
const ERROR_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
const SPC_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig";
const RESOURCE_TYPE_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:ResourceType";
const SCHEMA_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Schema";
const MAX_RESULTS: i64 = 200;
const MAX_OPERATIONS: usize = 1000;
pub(crate) const TOKEN_ENV: &str = "GATEWAY_SCIM_TOKEN_ENV";

/// Startup configuration. Intentionally not Debug/Clone: holds the token hash.
pub struct ScimConfig {
    token_hash: [u8; 32],
}

impl ScimConfig {
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// `GATEWAY_SCIM_TOKEN_ENV=NAME`; the token is the value of `NAME`.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Option<Self>> {
        let Some(name) = get(TOKEN_ENV) else {
            return Ok(None);
        };
        let name = name.trim();
        anyhow::ensure!(!name.is_empty(), "{TOKEN_ENV} must not be empty");
        anyhow::ensure!(
            name.len() <= 128
                && name != TOKEN_ENV
                && name
                    .bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
                && name
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
            "{TOKEN_ENV} must name an environment variable (A-Z, 0-9, _)"
        );
        let token =
            get(name).ok_or_else(|| anyhow::anyhow!("{TOKEN_ENV} names an unset variable"))?;
        anyhow::ensure!(
            (32..=1024).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_graphic()),
            "The SCIM token must be 32-1024 printable characters without spaces"
        );
        Ok(Some(Self {
            token_hash: Sha256::digest(token.as_bytes()).into(),
        }))
    }

    pub(crate) fn token_hash(&self) -> [u8; 32] {
        self.token_hash
    }
}

#[derive(Clone)]
pub(crate) struct ScimState {
    store: Store,
    runtime: Option<Arc<ScimRuntime>>,
}

impl ScimState {
    pub(crate) fn new(store: Store, runtime: Option<Arc<ScimRuntime>>) -> Self {
        Self { store, runtime }
    }
    fn rt(&self) -> Result<&ScimRuntime, ScimError> {
        self.runtime.as_deref().ok_or_else(not_found)
    }
}

/// SCIM routes alone, for runtime-privilege tests. The server mounts them through
/// `identity::router`, bound to the configured OIDC issuer.
#[doc(hidden)]
pub fn standalone_router(store: Store, config: ScimConfig, issuer: &str, base_url: &str) -> Router {
    let runtime = Arc::new(ScimRuntime {
        token_hash: config.token_hash,
        issuer: issuer.into(),
        base_url: base_url.into(),
    });
    router(ScimState::new(store.clone(), Some(runtime))).with_state(store)
}

pub(crate) fn router(state: ScimState) -> Router<Store> {
    Router::new()
        .route(
            "/scim/v2/ServiceProviderConfig",
            get(service_provider_config),
        )
        .route("/scim/v2/ResourceTypes", get(resource_types))
        .route("/scim/v2/ResourceTypes/{id}", get(resource_type))
        .route("/scim/v2/Schemas", get(schemas))
        .route("/scim/v2/Schemas/{id}", get(schema))
        .route("/scim/v2/Users", get(list_users).post(create_user))
        .route(
            "/scim/v2/Users/{id}",
            get(get_user)
                .put(replace_user)
                .patch(patch_user)
                .delete(delete_user),
        )
        .route("/scim/v2/Groups", get(list_groups).post(create_group))
        .route(
            "/scim/v2/Groups/{id}",
            get(get_group)
                .put(replace_group)
                .patch(patch_group)
                .delete(delete_group),
        )
        .route("/scim", any(unknown))
        .route("/scim/{*rest}", any(unknown))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Authentication, errors and response framing

fn authorized(headers: &HeaderMap, expected: &[u8; 32]) -> bool {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some((scheme, token)) = value.split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() || token.len() > 1024 {
        return false;
    }
    bool::from(Sha256::digest(token.as_bytes()).as_slice().ct_eq(expected))
}

async fn authenticate(State(state): State<ScimState>, request: Request, next: Next) -> Response {
    let Some(runtime) = state.runtime.as_ref() else {
        // Disabled: indistinguishable from an unknown route.
        return crate::http::not_found();
    };
    if !authorized(request.headers(), &runtime.token_hash) {
        let mut response =
            ScimError::new(StatusCode::UNAUTHORIZED, None, "Authentication required")
                .into_response();
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"scim\""),
        );
        return response;
    }
    next.run(request).await
}

#[derive(Debug)]
pub(crate) struct ScimError {
    status: StatusCode,
    scim_type: Option<&'static str>,
    detail: &'static str,
}

impl ScimError {
    fn new(status: StatusCode, scim_type: Option<&'static str>, detail: &'static str) -> Self {
        Self {
            status,
            scim_type,
            detail,
        }
    }
}

fn not_found() -> ScimError {
    ScimError::new(StatusCode::NOT_FOUND, None, "Resource not found")
}
fn invalid_value(detail: &'static str) -> ScimError {
    ScimError::new(StatusCode::BAD_REQUEST, Some("invalidValue"), detail)
}
fn invalid_syntax(detail: &'static str) -> ScimError {
    ScimError::new(StatusCode::BAD_REQUEST, Some("invalidSyntax"), detail)
}
fn invalid_filter() -> ScimError {
    ScimError::new(
        StatusCode::BAD_REQUEST,
        Some("invalidFilter"),
        "Only `attribute eq \"value\"` filters are supported",
    )
}
fn conflict() -> ScimError {
    ScimError::new(
        StatusCode::CONFLICT,
        Some("uniqueness"),
        "A resource with this identifier already exists",
    )
}
fn mutability(detail: &'static str) -> ScimError {
    ScimError::new(StatusCode::BAD_REQUEST, Some("mutability"), detail)
}
pub(crate) const LAST_ADMIN: &str =
    "Can't deactivate the last active Platform Admin. Grant Admin to someone else first.";
/// The write conflicts with the installation's state (RFC 7644 409; the
/// attribute change itself is not permitted for this resource now).
fn last_admin() -> ScimError {
    ScimError::new(StatusCode::CONFLICT, Some("mutability"), LAST_ADMIN)
}

impl IntoResponse for ScimError {
    fn into_response(self) -> Response {
        let mut body = json!({
            "schemas": [ERROR_SCHEMA],
            "status": self.status.as_u16().to_string(),
            "detail": self.detail,
        });
        if let Some(kind) = self.scim_type {
            body["scimType"] = json!(kind);
        }
        scim_json(self.status, body)
    }
}

impl From<sqlx::Error> for ScimError {
    fn from(error: sqlx::Error) -> Self {
        let code = error
            .as_database_error()
            .and_then(|e| e.code().map(|c| c.into_owned()));
        match code.as_deref() {
            Some("23505") => conflict(),
            Some("23514") => invalid_value("Attribute value out of range"),
            _ => {
                // Codes only: database messages can carry submitted values.
                tracing::error!(code = ?code, "SCIM storage error");
                ScimError::new(StatusCode::INTERNAL_SERVER_ERROR, None, "Internal error")
            }
        }
    }
}

fn scim_json(status: StatusCode, body: Value) -> Response {
    let mut response = (status, Json(body)).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/scim+json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn created(location: String, body: Value) -> Response {
    let mut response = scim_json(StatusCode::CREATED, body);
    if let Ok(value) = HeaderValue::from_str(&location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    response
}

fn no_content() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn list(total: i64, start: i64, resources: Vec<Value>) -> Response {
    scim_json(
        StatusCode::OK,
        json!({
            "schemas": [LIST_SCHEMA],
            "totalResults": total,
            "startIndex": start,
            "itemsPerPage": resources.len(),
            "Resources": resources,
        }),
    )
}

async fn unknown() -> ScimError {
    not_found()
}

fn ts(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn body_json(bytes: &Bytes) -> Result<Value, ScimError> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| invalid_syntax("Malformed JSON body"))?;
    if !value.is_object() {
        return Err(invalid_syntax("Expected a JSON object"));
    }
    Ok(value)
}

fn resource_id(raw: &str) -> Result<Uuid, ScimError> {
    Uuid::parse_str(raw).map_err(|_| not_found())
}

struct Page {
    start: i64,
    count: i64,
}
impl Page {
    fn parse(query: &HashMap<String, String>) -> Self {
        let start = query
            .get("startIndex")
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(1)
            .max(1);
        let count = query
            .get("count")
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(100)
            .clamp(0, MAX_RESULTS);
        Self { start, count }
    }
    fn offset(&self) -> i64 {
        self.start - 1
    }
}

/// `attribute eq "value"` (RFC 7644 3.4.2.2 subset). Returns the attribute (core schema
/// URN prefix removed, lowercased) and the decoded string value.
fn parse_filter(filter: &str) -> Result<(String, String), ScimError> {
    let filter = filter.trim();
    if filter.len() > 2048 {
        return Err(invalid_filter());
    }
    let (attribute, rest) = filter
        .split_once(char::is_whitespace)
        .ok_or_else(invalid_filter)?;
    let (operator, value) = rest
        .trim_start()
        .split_once(char::is_whitespace)
        .ok_or_else(invalid_filter)?;
    if !operator.eq_ignore_ascii_case("eq") {
        return Err(invalid_filter());
    }
    let value: String = serde_json::from_str(value.trim()).map_err(|_| invalid_filter())?;
    // A JSON escape (`\u0000`) decodes to a real control character; NUL would reach
    // PostgreSQL and fail as 500 (#414). No stored attribute we filter on can hold
    // one: every one is written through `text()`, which refuses them.
    if value.chars().any(char::is_control) {
        return Err(invalid_filter());
    }
    Ok((strip_schema(attribute).to_ascii_lowercase(), value))
}

fn strip_schema(path: &str) -> &str {
    for schema in [USER_SCHEMA, GROUP_SCHEMA] {
        if let Some(rest) = path.strip_prefix(schema).and_then(|r| r.strip_prefix(':')) {
            return rest;
        }
    }
    path
}

// ---------------------------------------------------------------------------
// Attribute validation

fn text(value: &Value, max: usize) -> Result<Option<String>, ScimError> {
    match value {
        Value::Null => Ok(None),
        Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else if trimmed.chars().count() > max || trimmed.chars().any(char::is_control) {
                Err(invalid_value(
                    "Attribute value is too long or has control characters",
                ))
            } else {
                Ok(Some(trimmed.to_owned()))
            }
        }
        _ => Err(invalid_value("Expected a string")),
    }
}

/// Entra ID historically sends booleans as "True"/"False" strings.
fn boolean(value: &Value) -> Result<bool, ScimError> {
    match value {
        Value::Bool(b) => Ok(*b),
        Value::String(s) if s.eq_ignore_ascii_case("true") => Ok(true),
        Value::String(s) if s.eq_ignore_ascii_case("false") => Ok(false),
        _ => Err(invalid_value("Expected a boolean")),
    }
}

fn email_valid(s: &str) -> bool {
    crate::text::chars_at_most(s, 320)
        && s.split('@').count() == 2
        && s.split('@').all(|part| !part.is_empty())
        && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn email_text(value: &Value) -> Result<String, ScimError> {
    text(value, 320)?
        .map(|s| s.to_lowercase())
        .filter(|s| email_valid(s))
        .ok_or_else(|| invalid_value("A valid email address is required"))
}

/// Primary email, else the first one.
fn email_from(value: &Value) -> Result<Option<String>, ScimError> {
    let emails = match value {
        Value::Null => return Ok(None),
        Value::Array(emails) => emails,
        _ => return Err(invalid_value("emails must be an array")),
    };
    let primary = emails
        .iter()
        .find(|e| {
            e.get("primary")
                .is_some_and(|p| boolean(p).unwrap_or(false))
        })
        .or_else(|| emails.first());
    match primary {
        None => Ok(None),
        Some(entry) => Ok(Some(email_text(
            entry
                .get("value")
                .ok_or_else(|| invalid_value("Email value missing"))?,
        )?)),
    }
}

fn display(value: &Value) -> Result<Option<String>, ScimError> {
    match text(value, 200)? {
        None => Ok(None),
        Some(name) => crate::identity::display_name(&name)
            .map(Some)
            .ok_or_else(|| invalid_value("Invalid displayName")),
    }
}

fn operations(body: &Value) -> Result<&Vec<Value>, ScimError> {
    let ops = body
        .get("Operations")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_syntax("Operations is required"))?;
    if ops.is_empty() || ops.len() > MAX_OPERATIONS {
        return Err(invalid_syntax("Operations must contain 1-1000 operations"));
    }
    Ok(ops)
}

fn op_parts(op: &Value) -> Result<(String, Option<&str>, Option<&Value>), ScimError> {
    let kind = op
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_syntax("Operation op is required"))?
        .to_ascii_lowercase();
    let path = match op.get("path") {
        None | Some(Value::Null) => None,
        Some(Value::String(path)) if !path.trim().is_empty() => Some(path.trim()),
        Some(_) => {
            return Err(ScimError::new(
                StatusCode::BAD_REQUEST,
                Some("invalidPath"),
                "Invalid path",
            ));
        }
    };
    Ok((kind, path, op.get("value")))
}

/// Whether the installation had an active Platform Admin when a SCIM write
/// started (read under the installation lock).
#[derive(Clone, Copy)]
struct AdminGuard {
    had_admin: bool,
}

/// Active Platform Admins: a live admin grant (any provenance) on an account that is
/// neither suspended nor cleaned up (`effective_platform_roles`).
async fn active_admins(tx: &mut Transaction<'_, Postgres>) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM effective_platform_roles WHERE role='admin'")
        .fetch_one(&mut **tx)
        .await
}

/// Every SCIM write runs in one transaction under the installation lock, the
/// same lock manual grant/suspend changes and sign-in take, so concurrent
/// writes cannot both pass the last-admin check.
async fn write_tx(store: &Store) -> Result<(Transaction<'_, Postgres>, AdminGuard), ScimError> {
    let mut tx = crate::db::begin(&store.pool).await?;
    lifecycle::lock(&mut tx).await?;
    let had_admin = active_admins(&mut tx).await? > 0;
    Ok((tx, AdminGuard { had_admin }))
}

/// Commit a SCIM write, unless it would leave the installation without an active
/// Platform Admin (deactivation, DELETE, or group changes that drop the last
/// effective Admin grant). Then nothing is applied: the transaction rolls back,
/// the refusal is audited and the built-in installation alert is raised, and the
/// provider gets 409.
async fn finish(
    store: &Store,
    mut tx: Transaction<'_, Postgres>,
    guard: AdminGuard,
    resource: &'static str,
    id: Uuid,
) -> Result<(), ScimError> {
    if guard.had_admin && active_admins(&mut tx).await? == 0 {
        tx.rollback().await?;
        if let Err(error) = record_last_admin_refusal(store, resource, id).await {
            let code = error
                .as_database_error()
                .and_then(|e| e.code().map(|c| c.into_owned()));
            tracing::error!(code = ?code, "SCIM last-admin refusal could not be recorded");
        }
        return Err(last_admin());
    }
    sqlx::query("UPDATE scim_state SET last_write_at=now() WHERE singleton")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Audit (no names or emails) and raise the installation alert in a separate
/// transaction: the refused change itself was rolled back.
async fn record_last_admin_refusal(
    store: &Store,
    resource: &'static str,
    id: Uuid,
) -> Result<(), sqlx::Error> {
    let mut tx = crate::db::begin(&store.pool).await?;
    lifecycle::lock(&mut tx).await?;
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,action,resource_type,resource_id,metadata) VALUES($1,NULL,'scim.last_admin_protected',$2,$3,'{}')")
        .bind(Uuid::new_v4())
        .bind(if resource == "group" { "scim_group" } else { "user" })
        .bind(id)
        .execute(&mut *tx)
        .await?;
    crate::alerts::fire_scim_last_admin(&mut tx, resource).await?;
    tx.commit().await
}

async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    action: &str,
    resource_type: &str,
    id: Uuid,
    metadata: Value,
) -> Result<(), ScimError> {
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,action,resource_type,resource_id,metadata) VALUES($1,NULL,$2,$3,$4,$5)")
        .bind(Uuid::new_v4())
        .bind(action)
        .bind(resource_type)
        .bind(id)
        .bind(metadata)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Admin › Settings › Sign-in

/// Read-only status: never the token or a token hash.
pub(crate) async fn summary(
    tx: &mut Transaction<'_, Postgres>,
    runtime: Option<&ScimRuntime>,
) -> Result<Value, sqlx::Error> {
    let Some(rt) = runtime else {
        return Ok(json!({ "enabled": false }));
    };
    let (users, active, groups, memberships, last): (i64, i64, i64, i64, Option<DateTime<Utc>>) =
        sqlx::query_as("SELECT (SELECT count(*) FROM scim_users s JOIN users u ON u.id=s.user_id WHERE u.cleaned_at IS NULL),(SELECT count(*) FROM scim_users s JOIN users u ON u.id=s.user_id WHERE u.cleaned_at IS NULL AND s.active),(SELECT count(*) FROM scim_groups),(SELECT count(*) FROM scim_group_members m JOIN users u ON u.id=m.user_id WHERE u.cleaned_at IS NULL),(SELECT last_write_at FROM scim_state WHERE singleton)")
            .fetch_one(&mut **tx)
            .await?;
    Ok(json!({
        "enabled": true,
        "base_url": rt.base_url,
        "users": users,
        "active_users": active,
        "groups": groups,
        "memberships": memberships,
        "last_sync_at": last,
    }))
}

#[cfg(test)]
use groups::{GroupDraft, apply_group_patch};
#[cfg(test)]
use users::{UserDraft, apply_user_patch};
#[cfg(test)]
#[path = "scim/tests.rs"]
mod tests;
