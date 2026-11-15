//! The management API's error types: `ApiError` and its stable reasons, the
//! policy rejection catalogue, the storage-error mapping and `ManagementError`.
use super::*;

pub(super) type ApiResult = Result<Json<Value>, ApiError>;
#[derive(Debug)]
pub(crate) struct ApiError(pub(crate) StatusCode, pub(crate) &'static str);
/// Stable machine-readable reasons for selected errors (additive `error.reason`).
pub(super) const REASONS: &[(&str, &str)] = &[
    (PERSONAL_NAME_FIXED, "personal_workspace_name_fixed"),
    (
        PERSONAL_VISIBILITY_FIXED,
        "personal_workspace_visibility_fixed",
    ),
    (PERSONAL_LIMITS, "personal_limits_platform_controlled"),
    (STACKED_LEGACY, "stacked_budgets_require_budgets_field"),
    (KEY_REVOKED, "key_revoked"),
    (KEY_DISABLED, "key_disabled"),
    (MEMBER_VISIBILITY, "workspace_wide_visibility_required"),
    (settings::KEY_LIFETIME, "key_lifetime_exceeds_maximum"),
    (settings::SETTING_LOCKED, "setting_locked_by_environment"),
    (
        settings::REFERENCE_NOT_ALLOWED,
        "credential_reference_not_allowed",
    ),
    (settings::PLAINTEXT_REMOTE, "plaintext_requires_loopback"),
    (settings::EMAIL_NOT_CONFIGURED, "email_not_configured"),
    (settings::EMAIL_TEST_LIMIT, "email_test_rate_limited"),
    (
        settings::storage::STORAGE_OFF,
        "file_storage_not_configured",
    ),
    (
        settings::storage::STORAGE_UNHEALTHY,
        "file_storage_unhealthy",
    ),
    (
        settings::storage::STORAGE_TEST_LIMIT,
        "storage_test_rate_limited",
    ),
    (branding::LOGO_TYPE, "logo_unsupported_type"),
    (branding::LOGO_TOO_LARGE, "logo_too_large"),
    (branding::LOGO_INVALID, "logo_invalid_image"),
    (branding::LOGO_DIMENSIONS, "logo_dimensions"),
    (alerts::PERSONAL_BUILTIN_ONLY, "personal_alerts_built_in"),
    (alerts::RULE_LIMIT, "alert_rule_limit"),
    (
        scheduled_reports::LIMIT_REACHED,
        "report_schedule_limit_reached",
    ),
    (alerts::KIND_FIXED, "alert_rule_kind_fixed"),
    (INSTALLATION_LIMITS_REMOVED, "installation_limits_removed"),
    (PRECONDITION_FAILED, "precondition_failed"),
];
pub(super) const PRECONDITION_FAILED: &str =
    "The mapping was changed since this proposal was reviewed; re-check it and try again";
/// Installation-wide limits were removed (0026): the former installation
/// policy endpoint answers `410` and an alert rule watching the
/// `installation` budget layer is `400`, both with this reason.
pub(crate) const INSTALLATION_LIMITS_REMOVED: &str = "Installation-wide limits were removed; set limits on workspace-type defaults, workspaces or API keys, and use an installation spend alert to watch total spend";
pub(crate) fn installation_limits_removed(status: StatusCode) -> ApiError {
    ApiError(status, INSTALLATION_LIMITS_REMOVED)
}
const PERSONAL_NAME_FIXED: &str = "Personal workspace name is fixed";
const PERSONAL_VISIBILITY_FIXED: &str =
    "Personal workspaces are private to their owner; object visibility is fixed";
const PERSONAL_LIMITS: &str = "Personal workspace limits are set by the platform";
const STACKED_LEGACY: &str = "This layer has several budgets; update them with the budgets field";
const KEY_REVOKED: &str = "Revoked keys can never be re-enabled";
const KEY_DISABLED: &str = "Disabled keys must be enabled first";
const MEMBER_VISIBILITY: &str = "Grouping by member requires workspace-wide visibility";
pub(super) fn personal_name_fixed() -> ApiError {
    ApiError(StatusCode::CONFLICT, PERSONAL_NAME_FIXED)
}
pub(super) fn personal_visibility_fixed() -> ApiError {
    ApiError(StatusCode::CONFLICT, PERSONAL_VISIBILITY_FIXED)
}
pub(super) fn personal_limits() -> ApiError {
    ApiError(StatusCode::FORBIDDEN, PERSONAL_LIMITS)
}
pub(super) fn stacked_legacy() -> ApiError {
    ApiError(StatusCode::CONFLICT, STACKED_LEGACY)
}
pub(super) fn key_revoked() -> ApiError {
    ApiError(StatusCode::CONFLICT, KEY_REVOKED)
}
pub(super) fn key_disabled() -> ApiError {
    ApiError(StatusCode::CONFLICT, KEY_DISABLED)
}
pub(super) fn member_visibility() -> ApiError {
    ApiError(StatusCode::FORBIDDEN, MEMBER_VISIBILITY)
}
/// Policy rejections with a stable reason plus one non-sensitive detail
/// (`period` or `limit` name). Never amounts: the caller learns which rule
/// failed, not another scope's value. `(message, reason, detail key, detail value)`.
pub(super) const POLICY_REASONS: &[(&str, &str, &str, &str)] = &[
    (
        "The day budget exceeds a parent day budget",
        "exceeds_parent_budget",
        "period",
        "day",
    ),
    (
        "The week budget exceeds a parent week budget",
        "exceeds_parent_budget",
        "period",
        "week",
    ),
    (
        "The month budget exceeds a parent month budget",
        "exceeds_parent_budget",
        "period",
        "month",
    ),
    (
        "The lifetime budget exceeds a parent lifetime budget",
        "exceeds_parent_budget",
        "period",
        "lifetime",
    ),
    (
        "requests_per_minute exceeds a parent limit",
        "exceeds_parent_rate",
        "limit",
        "requests_per_minute",
    ),
    (
        "tokens_per_minute exceeds a parent limit",
        "exceeds_parent_rate",
        "limit",
        "tokens_per_minute",
    ),
    (
        "concurrent_requests exceeds a parent limit",
        "exceeds_parent_rate",
        "limit",
        "concurrent_requests",
    ),
    (
        "concurrent_jobs exceeds a parent limit",
        "exceeds_parent_rate",
        "limit",
        "concurrent_jobs",
    ),
    (
        "storage_bytes exceeds a parent limit",
        "exceeds_parent_rate",
        "limit",
        "storage_bytes",
    ),
    (
        "key_issuances_per_holder exceeds a parent limit",
        "exceeds_parent_rate",
        "limit",
        "key_issuances_per_holder",
    ),
    (
        "key_issuances_per_workspace exceeds a parent limit",
        "exceeds_parent_rate",
        "limit",
        "key_issuances_per_workspace",
    ),
    (
        "A stored day budget cannot be raised",
        "stored_budget_raise_not_allowed",
        "period",
        "day",
    ),
    (
        "A stored week budget cannot be raised",
        "stored_budget_raise_not_allowed",
        "period",
        "week",
    ),
    (
        "A stored month budget cannot be raised",
        "stored_budget_raise_not_allowed",
        "period",
        "month",
    ),
    (
        "A stored lifetime budget cannot be raised",
        "stored_budget_raise_not_allowed",
        "period",
        "lifetime",
    ),
    (
        "A stored day budget cannot be removed or moved to another period",
        "period_change_not_allowed",
        "period",
        "day",
    ),
    (
        "A stored week budget cannot be removed or moved to another period",
        "period_change_not_allowed",
        "period",
        "week",
    ),
    (
        "A stored month budget cannot be removed or moved to another period",
        "period_change_not_allowed",
        "period",
        "month",
    ),
    (
        "A stored lifetime budget cannot be removed or moved to another period",
        "period_change_not_allowed",
        "period",
        "lifetime",
    ),
    (
        "A stored requests_per_minute cap cannot be raised or removed",
        "stored_rate_loosen_not_allowed",
        "limit",
        "requests_per_minute",
    ),
    (
        "A stored tokens_per_minute cap cannot be raised or removed",
        "stored_rate_loosen_not_allowed",
        "limit",
        "tokens_per_minute",
    ),
    (
        "A stored concurrent_requests cap cannot be raised or removed",
        "stored_rate_loosen_not_allowed",
        "limit",
        "concurrent_requests",
    ),
    (
        "A stored concurrent_jobs cap cannot be raised or removed",
        "stored_rate_loosen_not_allowed",
        "limit",
        "concurrent_jobs",
    ),
    (
        "A stored storage_bytes cap cannot be raised or removed",
        "stored_rate_loosen_not_allowed",
        "limit",
        "storage_bytes",
    ),
    (
        "A stored key_issuances_per_holder cap cannot be raised or removed",
        "stored_rate_loosen_not_allowed",
        "limit",
        "key_issuances_per_holder",
    ),
    (
        "A stored key_issuances_per_workspace cap cannot be raised or removed",
        "stored_rate_loosen_not_allowed",
        "limit",
        "key_issuances_per_workspace",
    ),
];
/// Policy rejection with `reason` and detail value (a period or limit name).
pub(crate) fn policy_rejection(status: StatusCode, reason: &str, detail: &str) -> ApiError {
    POLICY_REASONS
        .iter()
        .find(|(_, r, _, v)| *r == reason && *v == detail)
        .map_or_else(
            || ApiError(status, "Invalid request"),
            |(m, ..)| ApiError(status, m),
        )
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut error = json!({"code":self.0.as_u16().to_string(),"message":self.1});
        if let Some((_, reason)) = REASONS.iter().find(|(m, _)| *m == self.1) {
            error["reason"] = json!(reason);
        }
        if let Some((_, reason, key, value)) = POLICY_REASONS.iter().find(|(m, ..)| *m == self.1) {
            error["reason"] = json!(reason);
            error[*key] = json!(value);
        }
        (self.0, Json(json!({ "error": error }))).into_response()
    }
}
/// The OS random number generator failed while creating a secret (key,
/// invitation): nothing was created; the transaction rolls back (fail closed).
pub(crate) const ENTROPY_UNAVAILABLE: &str = "Secure random number generation unavailable";
impl From<crate::entropy::EntropyUnavailable> for ApiError {
    fn from(_: crate::entropy::EntropyUnavailable) -> Self {
        Self(StatusCode::SERVICE_UNAVAILABLE, ENTROPY_UNAVAILABLE)
    }
}
impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        if e.as_database_error()
            .is_some_and(|e| e.code().as_deref() == Some("57014"))
        {
            // The management read deadline (or an operator) cancelled the statement.
            log_storage_error(&e);
            Self(
                StatusCode::SERVICE_UNAVAILABLE,
                "The request took too long; narrow it and try again",
            )
        } else if e
            .as_database_error()
            .is_some_and(|e| matches!(e.code().as_deref(), Some("23505" | "23503" | "23514")))
        {
            Self(
                StatusCode::CONFLICT,
                "Resource conflicts with existing configuration",
            )
        } else {
            log_storage_error(&e);
            Self(
                StatusCode::SERVICE_UNAVAILABLE,
                "Management storage unavailable",
            )
        }
    }
}
/// SQLSTATE classes whose PostgreSQL primary messages name only objects or
/// conditions (transaction state, serialization, resources, operator action,
/// connection, syntax/privilege, prerequisite state, unsupported feature,
/// internal), never row or parameter values. Other classes (for example 22
/// data exceptions, 23 integrity, P0 trigger raises) may interpolate values,
/// so only their SQLSTATE and object names are logged.
const MESSAGE_SAFE_CLASSES: &[&str] = &[
    "08", "0A", "25", "40", "42", "53", "54", "55", "57", "58", "XX",
];
/// Sanitized diagnostics for a database error mapped to 503 (no row data,
/// bound values or connection strings). Emitted inside the `http.request`
/// span, so the line carries the request id.
pub(crate) fn storage_error_fields(e: &sqlx::Error) -> StorageErrorFields {
    let mut f = StorageErrorFields {
        kind: match e {
            sqlx::Error::Database(_) => "database",
            sqlx::Error::PoolTimedOut => "pool_timed_out",
            sqlx::Error::PoolClosed => "pool_closed",
            sqlx::Error::Io(_) => "io",
            sqlx::Error::Tls(_) => "tls",
            sqlx::Error::Protocol(_) => "protocol",
            sqlx::Error::RowNotFound => "row_not_found",
            sqlx::Error::ColumnNotFound(_) => "column_not_found",
            sqlx::Error::ColumnIndexOutOfBounds { .. } => "column_index_out_of_bounds",
            sqlx::Error::ColumnDecode { .. } => "column_decode",
            sqlx::Error::Decode(_) => "decode",
            sqlx::Error::TypeNotFound { .. } => "type_not_found",
            sqlx::Error::WorkerCrashed => "worker_crashed",
            _ => "other",
        },
        ..StorageErrorFields::default()
    };
    match e {
        sqlx::Error::Database(db) => {
            f.sqlstate = db.code().map(|c| c.into_owned());
            f.constraint = db.constraint().map(str::to_owned);
            if let Some(pg) = db.try_downcast_ref::<sqlx::postgres::PgDatabaseError>() {
                f.table = pg.table().map(str::to_owned);
                f.column = pg.column().map(str::to_owned);
                f.routine = pg.routine().map(str::to_owned);
            }
            if f.sqlstate
                .as_deref()
                .is_some_and(|c| MESSAGE_SAFE_CLASSES.iter().any(|k| c.starts_with(k)))
            {
                f.message = Some(db.message().chars().take(200).collect());
            }
        }
        sqlx::Error::ColumnNotFound(c) => f.column = Some(c.clone()),
        sqlx::Error::ColumnDecode { index, .. } => f.column = Some(index.clone()),
        _ => {}
    }
    f
}
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct StorageErrorFields {
    pub(crate) kind: &'static str,
    pub(crate) sqlstate: Option<String>,
    pub(crate) constraint: Option<String>,
    pub(crate) table: Option<String>,
    pub(crate) column: Option<String>,
    pub(crate) routine: Option<String>,
    pub(crate) message: Option<String>,
}
pub(crate) fn log_storage_error(e: &sqlx::Error) {
    let f = storage_error_fields(e);
    tracing::warn!(
        error_kind = f.kind,
        sqlstate = f.sqlstate.as_deref(),
        constraint = f.constraint.as_deref(),
        table = f.table.as_deref(),
        column = f.column.as_deref(),
        routine = f.routine.as_deref(),
        db_message = f.message.as_deref(),
        "management storage error"
    );
}
/// A management error that may carry a dynamic message, a stable machine code
/// and detail fields (for example `missing_meters`). It uses the same envelope
/// as every management error: `error.code` is the HTTP status and
/// `error.reason` is the machine code. Plain [`ApiError`]s convert into it
/// unchanged, so `?` keeps working.
#[derive(Debug)]
pub(crate) enum ManagementError {
    Api(ApiError),
    Detailed {
        status: StatusCode,
        code: &'static str,
        message: String,
        details: serde_json::Map<String, Value>,
    },
}
pub(super) type DetailedResult = Result<Json<Value>, ManagementError>;
impl ManagementError {
    pub(crate) fn detailed(
        status: StatusCode,
        code: &'static str,
        message: String,
        details: Value,
    ) -> Self {
        Self::Detailed {
            status,
            code,
            message,
            details: details.as_object().cloned().unwrap_or_default(),
        }
    }
    /// 400 `route_unsupported_capability`: the route's connection profile
    /// serves none of its model's protocols (`providers::capabilities`).
    pub(crate) fn route_unsupported(message: String) -> Self {
        Self::detailed(
            StatusCode::BAD_REQUEST,
            "route_unsupported_capability",
            message,
            json!({}),
        )
    }
}
impl From<ApiError> for ManagementError {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}
impl From<sqlx::Error> for ManagementError {
    fn from(e: sqlx::Error) -> Self {
        Self::Api(e.into())
    }
}
impl IntoResponse for ManagementError {
    fn into_response(self) -> Response {
        match self {
            Self::Api(e) => e.into_response(),
            Self::Detailed {
                status,
                code,
                message,
                details,
            } => {
                // Same envelope as `ApiError`: `code` is the HTTP status,
                // `reason` the machine code.
                let retry_after = (status == StatusCode::TOO_MANY_REQUESTS)
                    .then(|| details.get("retry_after_seconds").and_then(Value::as_u64))
                    .flatten();
                let mut error =
                    json!({"code":status.as_u16().to_string(),"message":message,"reason":code});
                if let Some(object) = error.as_object_mut() {
                    object.extend(details);
                }
                let mut response = (status, Json(json!({ "error": error }))).into_response();
                // A 429 that names its wait also sends it as the standard header.
                if let Some(seconds) = retry_after {
                    response
                        .headers_mut()
                        .insert(axum::http::header::RETRY_AFTER, seconds.into());
                }
                response
            }
        }
    }
}
pub(super) fn denied() -> ApiError {
    ApiError(StatusCode::FORBIDDEN, "Access denied")
}
pub(super) fn missing() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "Resource not found")
}
pub(super) fn invalid() -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, "Invalid request")
}
