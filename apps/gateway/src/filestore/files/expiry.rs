//! Effective file expiry and bounded SQL interval arguments.
use super::{FileError, RETENTION_SQL};

pub(super) fn expiry_millis(
    duration: Option<gateway_datetime_compat::SignedDuration>,
) -> Result<Option<i64>, FileError> {
    duration
        .map(|d| i64::try_from(d.whole_milliseconds()).map_err(|_| FileError::Invalid))
        .transpose()
}

pub(super) fn expiry_sql() -> String {
    format!("least(f.expires_at, f.created_at + make_interval(days => {RETENTION_SQL}))")
}
