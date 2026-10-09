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
    s.len() <= 320
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

// ---------------------------------------------------------------------------
// Users

#[derive(sqlx::FromRow)]
struct UserRow {
    id: Uuid,
    email: String,
    display_name: Option<String>,
    enabled: bool,
    user_created: DateTime<Utc>,
    user_name: Option<String>,
    external_id: Option<String>,
    given_name: Option<String>,
    family_name: Option<String>,
    scim_active: Option<bool>,
    scim_created: Option<DateTime<Utc>>,
    scim_updated: Option<DateTime<Utc>>,
}

const USER_SELECT: &str = "SELECT u.id,u.email,u.display_name,u.disabled_at IS NULL AS enabled,u.created_at AS user_created,s.user_name,s.external_id,s.given_name,s.family_name,s.active AS scim_active,s.created_at AS scim_created,s.updated_at AS scim_updated FROM users u LEFT JOIN scim_users s ON s.user_id=u.id WHERE u.cleaned_at IS NULL AND u.email IS NOT NULL";
const USER_FILTER: &str = " AND ($1::text IS NULL OR lower(coalesce(s.user_name,u.email))=lower($1)) AND ($2::text IS NULL OR s.external_id=$2) AND ($3::uuid IS NULL OR u.id=$3) AND ($4::text IS NULL OR lower(u.email)=lower($4))";

impl UserRow {
    /// The identity provider's view: its last `active` value, or the account state for
    /// users it has not written yet.
    fn active(&self) -> bool {
        self.scim_active.unwrap_or(self.enabled)
    }
    fn user_name(&self) -> &str {
        self.user_name.as_deref().unwrap_or(&self.email)
    }
    /// Privacy: directory attributes only. Never roles, workspaces, keys or activity.
    fn to_json(&self, base: &str) -> Value {
        let mut user = json!({
            "schemas": [USER_SCHEMA],
            "id": self.id,
            "userName": self.user_name(),
            "active": self.active(),
            "emails": [{"value": self.email, "type": "work", "primary": true}],
            "meta": {
                "resourceType": "User",
                "created": ts(self.scim_created.unwrap_or(self.user_created)),
                "lastModified": ts(self.scim_updated.unwrap_or(self.user_created)),
                "location": format!("{base}/Users/{}", self.id),
            },
        });
        if let Some(external) = &self.external_id {
            user["externalId"] = json!(external);
        }
        if let Some(name) = &self.display_name {
            user["displayName"] = json!(name);
        }
        if self.given_name.is_some() || self.family_name.is_some() {
            let formatted = [self.given_name.as_deref(), self.family_name.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            let mut name = json!({ "formatted": formatted });
            if let Some(given) = &self.given_name {
                name["givenName"] = json!(given);
            }
            if let Some(family) = &self.family_name {
                name["familyName"] = json!(family);
            }
            user["name"] = name;
        }
        user
    }
    fn draft(&self) -> UserDraft {
        UserDraft {
            user_name: self.user_name().to_owned(),
            external_id: self.external_id.clone(),
            given_name: self.given_name.clone(),
            family_name: self.family_name.clone(),
            display_name: self.display_name.clone(),
            email: self.email.clone(),
            active: self.active(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct UserDraft {
    user_name: String,
    external_id: Option<String>,
    given_name: Option<String>,
    family_name: Option<String>,
    display_name: Option<String>,
    email: String,
    active: bool,
}

/// Full representation (POST/PUT). Absent optional attributes are cleared, except
/// `active` (absent keeps `current`'s value, or true on create) and `displayName`
/// (absent keeps the current one, which sign-in also maintains).
fn user_from_body(body: &Value, current: Option<&UserDraft>) -> Result<UserDraft, ScimError> {
    let user_name = text(body.get("userName").unwrap_or(&Value::Null), 320)?
        .ok_or_else(|| invalid_value("userName is required"))?;
    let name = body.get("name").unwrap_or(&Value::Null);
    if !(name.is_null() || name.is_object()) {
        return Err(invalid_value("name must be an object"));
    }
    let email = match email_from(body.get("emails").unwrap_or(&Value::Null))? {
        Some(email) => email,
        None => email_text(&Value::String(user_name.clone()))
            .map_err(|_| invalid_value("An email address is required (emails or userName)"))?,
    };
    Ok(UserDraft {
        external_id: text(body.get("externalId").unwrap_or(&Value::Null), 512)?,
        given_name: text(name.get("givenName").unwrap_or(&Value::Null), 200)?,
        family_name: text(name.get("familyName").unwrap_or(&Value::Null), 200)?,
        display_name: match body.get("displayName") {
            Some(value) => display(value)?,
            None => current.and_then(|c| c.display_name.clone()),
        },
        active: match body.get("active") {
            Some(value) => boolean(value)?,
            None => current.is_none_or(|c| c.active),
        },
        user_name,
        email,
    })
}

fn set_user_attribute(draft: &mut UserDraft, path: &str, value: &Value) -> Result<(), ScimError> {
    let path = strip_schema(path);
    let lower = path.to_ascii_lowercase();
    match lower.as_str() {
        "active" => draft.active = boolean(value)?,
        "username" => {
            draft.user_name =
                text(value, 320)?.ok_or_else(|| invalid_value("userName is required"))?
        }
        "externalid" => draft.external_id = text(value, 512)?,
        "displayname" => draft.display_name = display(value)?,
        "name" => {
            let name = value
                .as_object()
                .ok_or_else(|| invalid_value("name must be an object"))?;
            for (key, value) in name {
                match key.to_ascii_lowercase().as_str() {
                    "givenname" => draft.given_name = text(value, 200)?,
                    "familyname" => draft.family_name = text(value, 200)?,
                    _ => {}
                }
            }
        }
        "name.givenname" => draft.given_name = text(value, 200)?,
        "name.familyname" => draft.family_name = text(value, 200)?,
        "emails" => {
            draft.email = email_from(value)?
                .ok_or_else(|| invalid_value("A valid email address is required"))?
        }
        // e.g. emails[type eq "work"].value / emails[primary eq true].value
        _ if lower.starts_with("emails[") && lower.ends_with("].value") => {
            draft.email = email_text(value)?
        }
        _ if lower.starts_with("emails[") && lower.ends_with(']') => {
            let entry = value
                .get("value")
                .ok_or_else(|| invalid_value("Email value missing"))?;
            draft.email = email_text(entry)?
        }
        // Outside the published schema subset (title, phoneNumbers, enterprise extension…).
        _ => {}
    }
    Ok(())
}

fn remove_user_attribute(draft: &mut UserDraft, path: &str) -> Result<(), ScimError> {
    let lower = strip_schema(path).to_ascii_lowercase();
    match lower.as_str() {
        "externalid" => draft.external_id = None,
        "displayname" => draft.display_name = None,
        "name" => {
            draft.given_name = None;
            draft.family_name = None;
        }
        "name.givenname" => draft.given_name = None,
        "name.familyname" => draft.family_name = None,
        "active" | "username" | "emails" => {
            return Err(mutability("Required attribute cannot be removed"));
        }
        _ if lower.starts_with("emails") => {
            return Err(mutability("Required attribute cannot be removed"));
        }
        _ => {}
    }
    Ok(())
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

fn apply_user_patch(draft: &mut UserDraft, body: &Value) -> Result<(), ScimError> {
    for op in operations(body)? {
        let (kind, path, value) = op_parts(op)?;
        match kind.as_str() {
            "add" | "replace" => {
                let value = value.ok_or_else(|| invalid_syntax("Operation value is required"))?;
                match path {
                    Some(path) => set_user_attribute(draft, path, value)?,
                    None => {
                        let fields = value
                            .as_object()
                            .ok_or_else(|| invalid_syntax("Operation value must be an object"))?;
                        for (key, value) in fields {
                            set_user_attribute(draft, key, value)?;
                        }
                    }
                }
            }
            "remove" => {
                let path = path.ok_or_else(|| {
                    ScimError::new(
                        StatusCode::BAD_REQUEST,
                        Some("noTarget"),
                        "path is required",
                    )
                })?;
                remove_user_attribute(draft, path)?;
            }
            _ => return Err(invalid_syntax("Unsupported operation")),
        }
    }
    Ok(())
}

async fn write_tx(store: &Store) -> Result<Transaction<'_, Postgres>, ScimError> {
    let mut tx = store.pool.begin().await?;
    lifecycle::lock(&mut tx).await?;
    Ok(tx)
}

async fn finish(mut tx: Transaction<'_, Postgres>) -> Result<(), ScimError> {
    sqlx::query("UPDATE scim_state SET last_write_at=now() WHERE singleton")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
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

async fn load_user(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    lock: bool,
) -> Result<Option<UserRow>, ScimError> {
    let sql = format!(
        "{USER_SELECT} AND u.id=$1{}",
        if lock { " FOR UPDATE OF u" } else { "" }
    );
    Ok(sqlx::query_as::<_, UserRow>(&sql)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?)
}

async fn list_users(
    State(state): State<ScimState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let page = Page::parse(&query);
    let (mut user_name, mut external_id, mut id, mut email) = (None, None, None, None);
    if let Some(filter) = query.get("filter") {
        let (attribute, value) = parse_filter(filter)?;
        match attribute.as_str() {
            "username" => user_name = Some(value),
            "externalid" => external_id = Some(value),
            "emails" | "emails.value" => email = Some(value),
            "id" => match Uuid::parse_str(&value) {
                Ok(value) => id = Some(value),
                Err(_) => return Ok(list(0, page.start, Vec::new())),
            },
            _ => return Err(invalid_filter()),
        }
    }
    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM users u LEFT JOIN scim_users s ON s.user_id=u.id WHERE u.cleaned_at IS NULL AND u.email IS NOT NULL{USER_FILTER}"))
        .bind(&user_name).bind(&external_id).bind(id).bind(&email)
        .fetch_one(&state.store.pool).await?;
    let rows: Vec<UserRow> = sqlx::query_as(&format!(
        "{USER_SELECT}{USER_FILTER} ORDER BY u.created_at,u.id LIMIT $5 OFFSET $6"
    ))
    .bind(&user_name)
    .bind(&external_id)
    .bind(id)
    .bind(&email)
    .bind(page.count)
    .bind(page.offset())
    .fetch_all(&state.store.pool)
    .await?;
    Ok(list(
        total,
        page.start,
        rows.iter().map(|r| r.to_json(&rt.base_url)).collect(),
    ))
}

async fn get_user(
    State(state): State<ScimState>,
    Path(id): Path<String>,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let id = resource_id(&id)?;
    let row: UserRow = sqlx::query_as(&format!("{USER_SELECT} AND u.id=$1"))
        .bind(id)
        .fetch_optional(&state.store.pool)
        .await?
        .ok_or_else(not_found)?;
    Ok(scim_json(StatusCode::OK, row.to_json(&rt.base_url)))
}

/// Email, display name and SCIM attributes, then any active-state transition.
async fn persist_user(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    before: Option<&UserDraft>,
    enabled: bool,
    draft: &UserDraft,
) -> Result<(), ScimError> {
    let taken: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users u LEFT JOIN scim_users s ON s.user_id=u.id WHERE u.id<>$1 AND u.cleaned_at IS NULL AND (lower(u.email)=lower($2) OR lower(coalesce(s.user_name,u.email))=lower($3)))")
        .bind(id).bind(&draft.email).bind(&draft.user_name)
        .fetch_one(&mut **tx).await?;
    if taken {
        return Err(conflict());
    }
    if let Some(before) = before {
        if !before.email.eq_ignore_ascii_case(&draft.email) {
            sqlx::query("UPDATE users SET email=$2 WHERE id=$1")
                .bind(id)
                .bind(&draft.email)
                .execute(&mut **tx)
                .await?;
            // A directory email change is not fresh OIDC proof: require a new sign-in.
            sqlx::query("UPDATE browser_sessions SET revoked_at=coalesce(revoked_at,now()) WHERE user_id=$1 AND revoked_at IS NULL")
                .bind(id)
                .execute(&mut **tx)
                .await?;
        }
        if before.display_name != draft.display_name {
            sqlx::query("UPDATE users SET display_name=$2 WHERE id=$1")
                .bind(id)
                .bind(&draft.display_name)
                .execute(&mut **tx)
                .await?;
        }
    } else if draft.display_name.is_some() {
        sqlx::query("UPDATE users SET display_name=$2 WHERE id=$1")
            .bind(id)
            .bind(&draft.display_name)
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query("INSERT INTO scim_users(user_id,user_name,external_id,given_name,family_name,active) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(user_id) DO UPDATE SET user_name=EXCLUDED.user_name,external_id=EXCLUDED.external_id,given_name=EXCLUDED.given_name,family_name=EXCLUDED.family_name,active=EXCLUDED.active,updated_at=now()")
        .bind(id).bind(&draft.user_name).bind(&draft.external_id).bind(&draft.given_name).bind(&draft.family_name).bind(draft.active)
        .execute(&mut **tx).await?;
    let was_active = before.is_none_or(|b| b.active);
    if !draft.active && (was_active || enabled) {
        deactivate(tx, id).await?;
    } else if draft.active && !was_active {
        reactivate(tx, id).await?;
    }
    Ok(())
}

/// Suspend: sessions and the user's personal/issued keys are revoked (never re-enabled).
/// Administrative suspension keeps its own reason; entitlement loss becomes SCIM's.
async fn deactivate(tx: &mut Transaction<'_, Postgres>, user: Uuid) -> Result<(), ScimError> {
    sqlx::query("UPDATE users SET disabled_at=coalesce(disabled_at,now()),cleanup_due_at=coalesce(cleanup_due_at,now()+interval '30 days'),disable_reason=CASE WHEN disable_reason IS NULL OR disable_reason='entitlement_loss' THEN 'scim_deactivated' ELSE disable_reason END WHERE id=$1 AND cleaned_at IS NULL")
        .bind(user).execute(&mut **tx).await?;
    sqlx::query("UPDATE browser_sessions SET revoked_at=coalesce(revoked_at,now()) WHERE user_id=$1 AND revoked_at IS NULL")
        .bind(user).execute(&mut **tx).await?;
    sqlx::query("UPDATE api_keys SET revoked_at=coalesce(revoked_at,now()) WHERE issued_to_user_id=$1 AND revoked_at IS NULL")
        .bind(user).execute(&mut **tx).await?;
    audit(tx, "scim.user.deactivated", "user", user, json!({})).await
}

/// Lifts only SCIM's own suspension, within the grace period. Grants decide access;
/// revoked keys and sessions stay revoked.
async fn reactivate(tx: &mut Transaction<'_, Postgres>, user: Uuid) -> Result<(), ScimError> {
    sqlx::query("UPDATE users SET disabled_at=NULL,cleanup_due_at=NULL,disable_reason=NULL WHERE id=$1 AND cleaned_at IS NULL AND disable_reason='scim_deactivated' AND cleanup_due_at>now()")
        .bind(user).execute(&mut **tx).await?;
    audit(tx, "scim.user.reactivated", "user", user, json!({})).await
}

async fn respond_user(
    tx: Transaction<'_, Postgres>,
    store: &Store,
    rt: &ScimRuntime,
    id: Uuid,
    status: StatusCode,
) -> Result<Response, ScimError> {
    finish(tx).await?;
    let row: UserRow = sqlx::query_as(&format!("{USER_SELECT} AND u.id=$1"))
        .bind(id)
        .fetch_optional(&store.pool)
        .await?
        .ok_or_else(not_found)?;
    let body = row.to_json(&rt.base_url);
    Ok(if status == StatusCode::CREATED {
        created(format!("{}/Users/{id}", rt.base_url), body)
    } else {
        scim_json(status, body)
    })
}

async fn create_user(State(state): State<ScimState>, body: Bytes) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let draft = user_from_body(&body_json(&body)?, None)?;
    let mut tx = write_tx(&state.store).await?;
    let id = Uuid::new_v4();
    let taken: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users u LEFT JOIN scim_users s ON s.user_id=u.id WHERE u.cleaned_at IS NULL AND (lower(u.email)=lower($1) OR lower(coalesce(s.user_name,u.email))=lower($2)))")
        .bind(&draft.email).bind(&draft.user_name).fetch_one(&mut *tx).await?;
    if taken {
        return Err(conflict());
    }
    // Pre-authorizes one-time OIDC linking by verified email, like manual provisioning.
    sqlx::query("INSERT INTO users(id,email,oidc_link_allowed) VALUES($1,$2,true)")
        .bind(id)
        .bind(&draft.email)
        .execute(&mut *tx)
        .await?;
    persist_user(&mut tx, id, None, true, &draft).await?;
    audit(
        &mut tx,
        "scim.user.created",
        "user",
        id,
        json!({"active": draft.active}),
    )
    .await?;
    respond_user(tx, &state.store, rt, id, StatusCode::CREATED).await
}

async fn replace_user(
    State(state): State<ScimState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let id = resource_id(&id)?;
    let body = body_json(&body)?;
    let mut tx = write_tx(&state.store).await?;
    let row = load_user(&mut tx, id, true).await?.ok_or_else(not_found)?;
    let before = row.draft();
    let draft = user_from_body(&body, Some(&before))?;
    persist_user(&mut tx, id, Some(&before), row.enabled, &draft).await?;
    audit(&mut tx, "scim.user.updated", "user", id, json!({})).await?;
    respond_user(tx, &state.store, rt, id, StatusCode::OK).await
}

async fn patch_user(
    State(state): State<ScimState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let id = resource_id(&id)?;
    let body = body_json(&body)?;
    let mut tx = write_tx(&state.store).await?;
    let row = load_user(&mut tx, id, true).await?.ok_or_else(not_found)?;
    let before = row.draft();
    let mut draft = before.clone();
    apply_user_patch(&mut draft, &body)?;
    persist_user(&mut tx, id, Some(&before), row.enabled, &draft).await?;
    audit(&mut tx, "scim.user.updated", "user", id, json!({})).await?;
    respond_user(tx, &state.store, rt, id, StatusCode::OK).await
}

/// DELETE deactivates; users are never deleted (history and attribution stay).
async fn delete_user(
    State(state): State<ScimState>,
    Path(id): Path<String>,
) -> Result<Response, ScimError> {
    state.rt()?;
    let id = resource_id(&id)?;
    let mut tx = write_tx(&state.store).await?;
    let row = load_user(&mut tx, id, true).await?.ok_or_else(not_found)?;
    let before = row.draft();
    let draft = UserDraft {
        active: false,
        ..before.clone()
    };
    persist_user(&mut tx, id, Some(&before), row.enabled, &draft).await?;
    finish(tx).await?;
    Ok(no_content())
}

// ---------------------------------------------------------------------------
// Groups and group-provenance synchronization

#[derive(sqlx::FromRow)]
struct GroupRow {
    id: Uuid,
    display_name: String,
    external_id: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl GroupRow {
    fn values(&self) -> Vec<String> {
        let mut values = vec![self.display_name.clone()];
        values.extend(self.external_id.clone());
        values
    }
    fn to_json(&self, members: Option<&[Uuid]>, base: &str) -> Value {
        let mut group = json!({
            "schemas": [GROUP_SCHEMA],
            "id": self.id,
            "displayName": self.display_name,
            "meta": {
                "resourceType": "Group",
                "created": ts(self.created_at),
                "lastModified": ts(self.updated_at),
                "location": format!("{base}/Groups/{}", self.id),
            },
        });
        if let Some(external) = &self.external_id {
            group["externalId"] = json!(external);
        }
        if let Some(members) = members {
            group["members"] = members
                .iter()
                .map(
                    |id| json!({"value": id, "$ref": format!("{base}/Users/{id}"), "type": "User"}),
                )
                .collect();
        }
        group
    }
}

const GROUP_SELECT: &str =
    "SELECT id,display_name,external_id,created_at,updated_at FROM scim_groups g WHERE true";
const GROUP_FILTER: &str = " AND ($1::text IS NULL OR lower(g.display_name)=lower($1)) AND ($2::text IS NULL OR g.external_id=$2) AND ($3::uuid IS NULL OR g.id=$3)";

/// Every group value SCIM manages (display names and external ids).
async fn managed_values(tx: &mut Transaction<'_, Postgres>) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT DISTINCT v FROM scim_groups g CROSS JOIN LATERAL (VALUES (g.display_name),(g.external_id)) x(v) WHERE v IS NOT NULL")
        .fetch_all(&mut **tx)
        .await
}

async fn member_values(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT DISTINCT v FROM scim_group_members m JOIN scim_groups g ON g.id=m.group_id CROSS JOIN LATERAL (VALUES (g.display_name),(g.external_id)) x(v) WHERE m.user_id=$1 AND v IS NOT NULL")
        .bind(user)
        .fetch_all(&mut **tx)
        .await
}

/// Sign-in claim merge: SCIM-managed groups come from SCIM membership, everything else
/// from the signed token.
pub(crate) async fn effective_groups(
    tx: &mut Transaction<'_, Postgres>,
    user: Uuid,
    token: &[String],
) -> Result<Vec<String>, sqlx::Error> {
    let managed: BTreeSet<String> = managed_values(tx).await?.into_iter().collect();
    let mut groups: BTreeSet<String> = token
        .iter()
        .filter(|g| !managed.contains(*g))
        .cloned()
        .collect();
    groups.extend(member_values(tx, user).await?);
    Ok(groups.into_iter().collect())
}

/// Users holding active group-provenance grants from mappings matching `values`.
async fn holders(
    tx: &mut Transaction<'_, Postgres>,
    issuer: &str,
    values: &[String],
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT g.user_id FROM platform_role_grants g JOIN oidc_group_mappings m ON m.id=g.mapping_id WHERE g.source='group' AND g.revoked_at IS NULL AND m.issuer=$1 AND m.group_value=ANY($2) UNION SELECT g.user_id FROM workspace_membership_grants g JOIN oidc_group_mappings m ON m.id=g.mapping_id WHERE g.source='group' AND g.revoked_at IS NULL AND m.issuer=$1 AND m.group_value=ANY($2)")
        .bind(issuer)
        .bind(values)
        .fetch_all(&mut **tx)
        .await
}

/// Reconcile SCIM-managed group grants for each affected user. `extra_scope` covers
/// values that stopped being managed in this request (renamed or deleted groups).
async fn sync_users(
    tx: &mut Transaction<'_, Postgres>,
    rt: &ScimRuntime,
    users: BTreeSet<Uuid>,
    extra_scope: &[String],
) -> Result<(), ScimError> {
    if users.is_empty() {
        return Ok(());
    }
    let mut scope: BTreeSet<String> = managed_values(tx).await?.into_iter().collect();
    scope.extend(extra_scope.iter().cloned());
    let scope: Vec<String> = scope.into_iter().collect();
    for user in users {
        let live: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM users WHERE id=$1 AND cleaned_at IS NULL FOR UPDATE",
        )
        .bind(user)
        .fetch_optional(&mut **tx)
        .await?;
        if live.is_none() {
            continue;
        }
        let groups = member_values(tx, user).await?;
        lifecycle::synchronize_scim_groups(tx, user, &rt.issuer, &groups, &scope).await?;
    }
    Ok(())
}

async fn members_of(
    executor: impl sqlx::PgExecutor<'_>,
    groups: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Uuid>>, sqlx::Error> {
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as("SELECT m.group_id,m.user_id FROM scim_group_members m JOIN users u ON u.id=m.user_id WHERE m.group_id=ANY($1) AND u.cleaned_at IS NULL ORDER BY m.group_id,m.user_id")
        .bind(groups)
        .fetch_all(executor)
        .await?;
    let mut map: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for (group, user) in rows {
        map.entry(group).or_default().push(user);
    }
    Ok(map)
}

fn members_from(value: &Value) -> Result<BTreeSet<Uuid>, ScimError> {
    let entries = match value {
        Value::Null => return Ok(BTreeSet::new()),
        Value::Array(entries) => entries.as_slice(),
        Value::Object(_) => std::slice::from_ref(value),
        _ => return Err(invalid_value("members must be an array")),
    };
    entries
        .iter()
        .map(|entry| {
            entry
                .get("value")
                .and_then(Value::as_str)
                .and_then(|v| Uuid::parse_str(v.trim()).ok())
                .ok_or_else(|| invalid_value("Member value must be a user id"))
        })
        .collect()
}

/// Members must be existing, non-tombstoned users (never silently dropped).
async fn check_members(
    tx: &mut Transaction<'_, Postgres>,
    members: &BTreeSet<Uuid>,
) -> Result<(), ScimError> {
    if members.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = members.iter().copied().collect();
    let found: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM users WHERE id=ANY($1) AND cleaned_at IS NULL AND email IS NOT NULL",
    )
    .bind(&ids)
    .fetch_one(&mut **tx)
    .await?;
    if found != ids.len() as i64 {
        return Err(invalid_value("Unknown member"));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
struct GroupDraft {
    display_name: String,
    external_id: Option<String>,
    members: BTreeSet<Uuid>,
}

fn group_name(value: &Value) -> Result<String, ScimError> {
    text(value, 512)?.ok_or_else(|| invalid_value("displayName is required"))
}

fn group_from_body(body: &Value) -> Result<GroupDraft, ScimError> {
    Ok(GroupDraft {
        display_name: group_name(body.get("displayName").unwrap_or(&Value::Null))?,
        external_id: text(body.get("externalId").unwrap_or(&Value::Null), 512)?,
        members: members_from(body.get("members").unwrap_or(&Value::Null))?,
    })
}

/// `members[value eq "id"]`
fn member_path(lower: &str, original: &str) -> Result<Option<Uuid>, ScimError> {
    if !(lower.starts_with("members[") && lower.ends_with(']')) {
        return Ok(None);
    }
    let inner = &original["members[".len()..original.len() - 1];
    let (attribute, value) = parse_filter(inner)?;
    if attribute != "value" {
        return Err(invalid_filter());
    }
    Uuid::parse_str(&value)
        .map(Some)
        .map_err(|_| invalid_value("Member value must be a user id"))
}

fn set_group_attribute(
    draft: &mut GroupDraft,
    path: &str,
    value: &Value,
    add: bool,
) -> Result<(), ScimError> {
    match strip_schema(path).to_ascii_lowercase().as_str() {
        "displayname" => draft.display_name = group_name(value)?,
        "externalid" => draft.external_id = text(value, 512)?,
        "members" if add => draft.members.extend(members_from(value)?),
        "members" => draft.members = members_from(value)?,
        _ => {}
    }
    Ok(())
}

fn apply_group_patch(draft: &mut GroupDraft, body: &Value) -> Result<(), ScimError> {
    for op in operations(body)? {
        let (kind, path, value) = op_parts(op)?;
        match kind.as_str() {
            "add" | "replace" => {
                let add = kind == "add";
                let value = value.ok_or_else(|| invalid_syntax("Operation value is required"))?;
                match path {
                    Some(path) => set_group_attribute(draft, path, value, add)?,
                    None => {
                        let fields = value
                            .as_object()
                            .ok_or_else(|| invalid_syntax("Operation value must be an object"))?;
                        for (key, value) in fields {
                            set_group_attribute(draft, key, value, add)?;
                        }
                    }
                }
            }
            "remove" => {
                let path = path.ok_or_else(|| {
                    ScimError::new(
                        StatusCode::BAD_REQUEST,
                        Some("noTarget"),
                        "path is required",
                    )
                })?;
                let stripped = strip_schema(path);
                let lower = stripped.to_ascii_lowercase();
                if let Some(member) = member_path(&lower, stripped)? {
                    draft.members.remove(&member);
                } else {
                    match lower.as_str() {
                        "members" => match value {
                            Some(value) if !value.is_null() => {
                                for member in members_from(value)? {
                                    draft.members.remove(&member);
                                }
                            }
                            _ => draft.members.clear(),
                        },
                        "externalid" => draft.external_id = None,
                        "displayname" => {
                            return Err(mutability("Required attribute cannot be removed"));
                        }
                        _ => {}
                    }
                }
            }
            _ => return Err(invalid_syntax("Unsupported operation")),
        }
    }
    Ok(())
}

async fn load_group(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<Option<(GroupRow, BTreeSet<Uuid>)>, ScimError> {
    let Some(row) =
        sqlx::query_as::<_, GroupRow>(&format!("{GROUP_SELECT} AND g.id=$1 FOR UPDATE"))
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
    else {
        return Ok(None);
    };
    let members = members_of(&mut **tx, &[id])
        .await?
        .remove(&id)
        .unwrap_or_default()
        .into_iter()
        .collect();
    Ok(Some((row, members)))
}

/// Write `draft` over the group (or create it) and reconcile affected users.
async fn persist_group(
    tx: &mut Transaction<'_, Postgres>,
    rt: &ScimRuntime,
    id: Uuid,
    before: Option<(&GroupRow, &BTreeSet<Uuid>)>,
    draft: &GroupDraft,
) -> Result<(), ScimError> {
    check_members(tx, &draft.members).await?;
    let empty = BTreeSet::new();
    let old_members = before.map_or(&empty, |(_, m)| m);
    let old_values = before.map(|(g, _)| g.values()).unwrap_or_default();
    let mut new_values = vec![draft.display_name.clone()];
    new_values.extend(draft.external_id.clone());
    match before {
        None => {
            sqlx::query("INSERT INTO scim_groups(id,display_name,external_id) VALUES($1,$2,$3)")
                .bind(id)
                .bind(&draft.display_name)
                .bind(&draft.external_id)
                .execute(&mut **tx)
                .await?;
        }
        Some((row, _)) => {
            if row.display_name != draft.display_name || row.external_id != draft.external_id {
                sqlx::query("UPDATE scim_groups SET display_name=$2,external_id=$3,updated_at=now() WHERE id=$1")
                    .bind(id).bind(&draft.display_name).bind(&draft.external_id)
                    .execute(&mut **tx).await?;
            }
        }
    }
    let added: Vec<Uuid> = draft.members.difference(old_members).copied().collect();
    let removed: Vec<Uuid> = old_members.difference(&draft.members).copied().collect();
    if !added.is_empty() {
        sqlx::query("INSERT INTO scim_group_members(group_id,user_id) SELECT $1,unnest($2::uuid[]) ON CONFLICT DO NOTHING")
            .bind(id).bind(&added).execute(&mut **tx).await?;
    }
    if !removed.is_empty() {
        sqlx::query("DELETE FROM scim_group_members WHERE group_id=$1 AND user_id=ANY($2)")
            .bind(id)
            .bind(&removed)
            .execute(&mut **tx)
            .await?;
    }
    if before.is_some() && (!added.is_empty() || !removed.is_empty()) {
        sqlx::query("UPDATE scim_groups SET updated_at=now() WHERE id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
    }
    let mut affected: BTreeSet<Uuid> = added.iter().chain(removed.iter()).copied().collect();
    if old_values != new_values {
        // The group's values changed (or it is new): its members and anyone holding a
        // grant through the old/new values now take that group's membership from SCIM.
        affected.extend(old_members.iter().copied());
        affected.extend(draft.members.iter().copied());
        let all: Vec<String> = old_values
            .iter()
            .chain(new_values.iter())
            .cloned()
            .collect();
        affected.extend(holders(tx, &rt.issuer, &all).await?);
    }
    sync_users(tx, rt, affected, &old_values).await?;
    audit(
        tx,
        if before.is_none() {
            "scim.group.created"
        } else {
            "scim.group.updated"
        },
        "scim_group",
        id,
        json!({"members_added": added.len(), "members_removed": removed.len()}),
    )
    .await
}

async fn list_groups(
    State(state): State<ScimState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let page = Page::parse(&query);
    let (mut name, mut external_id, mut id) = (None, None, None);
    if let Some(filter) = query.get("filter") {
        let (attribute, value) = parse_filter(filter)?;
        match attribute.as_str() {
            "displayname" => name = Some(value),
            "externalid" => external_id = Some(value),
            "id" => match Uuid::parse_str(&value) {
                Ok(value) => id = Some(value),
                Err(_) => return Ok(list(0, page.start, Vec::new())),
            },
            _ => return Err(invalid_filter()),
        }
    }
    let excluded = query.get("excludedAttributes").is_some_and(|v| {
        v.split(',')
            .any(|a| strip_schema(a.trim()).eq_ignore_ascii_case("members"))
    });
    let total: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM scim_groups g WHERE true{GROUP_FILTER}"
    ))
    .bind(&name)
    .bind(&external_id)
    .bind(id)
    .fetch_one(&state.store.pool)
    .await?;
    let rows: Vec<GroupRow> = sqlx::query_as(&format!(
        "{GROUP_SELECT}{GROUP_FILTER} ORDER BY g.created_at,g.id LIMIT $4 OFFSET $5"
    ))
    .bind(&name)
    .bind(&external_id)
    .bind(id)
    .bind(page.count)
    .bind(page.offset())
    .fetch_all(&state.store.pool)
    .await?;
    let members = if excluded {
        HashMap::new()
    } else {
        members_of(
            &state.store.pool,
            &rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        )
        .await?
    };
    let resources = rows
        .iter()
        .map(|row| {
            let ids = members.get(&row.id).map(Vec::as_slice).unwrap_or(&[]);
            row.to_json((!excluded).then_some(ids), &rt.base_url)
        })
        .collect();
    Ok(list(total, page.start, resources))
}

async fn get_group(
    State(state): State<ScimState>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let id = resource_id(&id)?;
    let row: GroupRow = sqlx::query_as(&format!("{GROUP_SELECT} AND g.id=$1"))
        .bind(id)
        .fetch_optional(&state.store.pool)
        .await?
        .ok_or_else(not_found)?;
    let excluded = query.get("excludedAttributes").is_some_and(|v| {
        v.split(',')
            .any(|a| strip_schema(a.trim()).eq_ignore_ascii_case("members"))
    });
    let members = members_of(&state.store.pool, &[id])
        .await?
        .remove(&id)
        .unwrap_or_default();
    Ok(scim_json(
        StatusCode::OK,
        row.to_json((!excluded).then_some(&members), &rt.base_url),
    ))
}

async fn respond_group(
    store: &Store,
    rt: &ScimRuntime,
    id: Uuid,
    status: StatusCode,
) -> Result<Response, ScimError> {
    let row: GroupRow = sqlx::query_as(&format!("{GROUP_SELECT} AND g.id=$1"))
        .bind(id)
        .fetch_optional(&store.pool)
        .await?
        .ok_or_else(not_found)?;
    let members = members_of(&store.pool, &[id])
        .await?
        .remove(&id)
        .unwrap_or_default();
    let body = row.to_json(Some(&members), &rt.base_url);
    Ok(if status == StatusCode::CREATED {
        created(format!("{}/Groups/{id}", rt.base_url), body)
    } else {
        scim_json(status, body)
    })
}

async fn create_group(State(state): State<ScimState>, body: Bytes) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let draft = group_from_body(&body_json(&body)?)?;
    let mut tx = write_tx(&state.store).await?;
    let taken: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM scim_groups WHERE display_name=$1)")
            .bind(&draft.display_name)
            .fetch_one(&mut *tx)
            .await?;
    if taken {
        return Err(conflict());
    }
    let id = Uuid::new_v4();
    persist_group(&mut tx, rt, id, None, &draft).await?;
    finish(tx).await?;
    respond_group(&state.store, rt, id, StatusCode::CREATED).await
}

async fn replace_group(
    State(state): State<ScimState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let id = resource_id(&id)?;
    let draft = group_from_body(&body_json(&body)?)?;
    let mut tx = write_tx(&state.store).await?;
    let (row, members) = load_group(&mut tx, id).await?.ok_or_else(not_found)?;
    persist_group(&mut tx, rt, id, Some((&row, &members)), &draft).await?;
    finish(tx).await?;
    respond_group(&state.store, rt, id, StatusCode::OK).await
}

/// 204: large groups are not echoed back on every membership change.
async fn patch_group(
    State(state): State<ScimState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let id = resource_id(&id)?;
    let body = body_json(&body)?;
    let mut tx = write_tx(&state.store).await?;
    let (row, members) = load_group(&mut tx, id).await?.ok_or_else(not_found)?;
    let mut draft = GroupDraft {
        display_name: row.display_name.clone(),
        external_id: row.external_id.clone(),
        members: members.clone(),
    };
    apply_group_patch(&mut draft, &body)?;
    persist_group(&mut tx, rt, id, Some((&row, &members)), &draft).await?;
    finish(tx).await?;
    Ok(no_content())
}

/// Deleting a group revokes the grants it provided (group provenance only).
async fn delete_group(
    State(state): State<ScimState>,
    Path(id): Path<String>,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let id = resource_id(&id)?;
    let mut tx = write_tx(&state.store).await?;
    let (row, members) = load_group(&mut tx, id).await?.ok_or_else(not_found)?;
    let values = row.values();
    let mut affected = members;
    affected.extend(holders(&mut tx, &rt.issuer, &values).await?);
    sqlx::query("DELETE FROM scim_groups WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sync_users(&mut tx, rt, affected, &values).await?;
    audit(&mut tx, "scim.group.deleted", "scim_group", id, json!({})).await?;
    finish(tx).await?;
    Ok(no_content())
}

// ---------------------------------------------------------------------------
// Discovery

async fn service_provider_config(State(state): State<ScimState>) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    Ok(scim_json(
        StatusCode::OK,
        json!({
            "schemas": [SPC_SCHEMA],
            "patch": {"supported": true},
            "bulk": {"supported": false, "maxOperations": 0, "maxPayloadSize": 0},
            "filter": {"supported": true, "maxResults": MAX_RESULTS},
            "changePassword": {"supported": false},
            "sort": {"supported": false},
            "etag": {"supported": false},
            "authenticationSchemes": [{
                "type": "oauthbearertoken",
                "name": "Bearer token",
                "description": "Static bearer token configured on the gateway",
                "primary": true,
            }],
            "meta": {
                "resourceType": "ServiceProviderConfig",
                "location": format!("{}/ServiceProviderConfig", rt.base_url),
            },
        }),
    ))
}

fn resource_type_json(id: &str, base: &str) -> Option<Value> {
    let (endpoint, schema, description) = match id {
        "User" => ("/Users", USER_SCHEMA, "Gateway user"),
        "Group" => (
            "/Groups",
            GROUP_SCHEMA,
            "Directory group (feeds SSO group mappings)",
        ),
        _ => return None,
    };
    Some(json!({
        "schemas": [RESOURCE_TYPE_SCHEMA],
        "id": id,
        "name": id,
        "endpoint": endpoint,
        "description": description,
        "schema": schema,
        "meta": {"resourceType": "ResourceType", "location": format!("{base}/ResourceTypes/{id}")},
    }))
}

async fn resource_types(State(state): State<ScimState>) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let items: Vec<Value> = ["User", "Group"]
        .iter()
        .filter_map(|id| resource_type_json(id, &rt.base_url))
        .collect();
    Ok(list(items.len() as i64, 1, items))
}

async fn resource_type(
    State(state): State<ScimState>,
    Path(id): Path<String>,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    resource_type_json(&id, &rt.base_url)
        .map(|v| scim_json(StatusCode::OK, v))
        .ok_or_else(not_found)
}

fn attribute(name: &str, kind: &str, required: bool, extra: Value) -> Value {
    let mut attribute = json!({
        "name": name,
        "type": kind,
        "multiValued": false,
        "required": required,
        "caseExact": false,
        "mutability": "readWrite",
        "returned": "default",
        "uniqueness": "none",
    });
    if let (Some(target), Some(extra)) = (attribute.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
    attribute
}

fn schema_json(id: &str, base: &str) -> Option<Value> {
    let (name, attributes) = match id {
        USER_SCHEMA => (
            "User",
            vec![
                attribute("userName", "string", true, json!({"uniqueness": "server"})),
                attribute(
                    "name",
                    "complex",
                    false,
                    json!({"subAttributes": [
                        attribute("givenName", "string", false, json!({})),
                        attribute("familyName", "string", false, json!({})),
                        attribute("formatted", "string", false, json!({"mutability": "readOnly"})),
                    ]}),
                ),
                attribute("displayName", "string", false, json!({})),
                attribute(
                    "emails",
                    "complex",
                    true,
                    json!({"multiValued": true, "subAttributes": [
                        attribute("value", "string", true, json!({})),
                        attribute("type", "string", false, json!({"canonicalValues": ["work"]})),
                        attribute("primary", "boolean", false, json!({})),
                    ]}),
                ),
                attribute("active", "boolean", false, json!({})),
            ],
        ),
        GROUP_SCHEMA => (
            "Group",
            vec![
                attribute(
                    "displayName",
                    "string",
                    true,
                    json!({"uniqueness": "server"}),
                ),
                attribute(
                    "members",
                    "complex",
                    false,
                    json!({"multiValued": true, "subAttributes": [
                        attribute("value", "string", true, json!({"mutability": "immutable"})),
                        attribute("$ref", "reference", false, json!({"mutability": "immutable", "referenceTypes": ["User"]})),
                        attribute("type", "string", false, json!({"mutability": "immutable", "canonicalValues": ["User"]})),
                    ]}),
                ),
            ],
        ),
        _ => return None,
    };
    Some(json!({
        "schemas": [SCHEMA_SCHEMA],
        "id": id,
        "name": name,
        "attributes": attributes,
        "meta": {"resourceType": "Schema", "location": format!("{base}/Schemas/{id}")},
    }))
}

async fn schemas(State(state): State<ScimState>) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    let items: Vec<Value> = [USER_SCHEMA, GROUP_SCHEMA]
        .iter()
        .filter_map(|id| schema_json(id, &rt.base_url))
        .collect();
    Ok(list(items.len() as i64, 1, items))
}

async fn schema(
    State(state): State<ScimState>,
    Path(id): Path<String>,
) -> Result<Response, ScimError> {
    let rt = state.rt()?;
    schema_json(&id, &rt.base_url)
        .map(|v| scim_json(StatusCode::OK, v))
        .ok_or_else(not_found)
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
#[path = "scim/tests.rs"]
mod tests;
