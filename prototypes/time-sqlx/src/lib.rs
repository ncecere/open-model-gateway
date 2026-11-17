//! Native SQLx/time codec feasibility proof; not a gateway dependency.
//!
//! These guards demonstrate application bounds independently of the native codec.
//! They do not protect reads of arbitrary database values or define an HTTP parser.
use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset, macros::datetime};

/// Existing log/session/hold cursor intersection of PostgreSQL's finite lower
/// bound and Chrono's upper bound, in integer Unix microseconds. Keep this ceiling
/// when changing libraries; native time and PostgreSQL both permit later years.
pub fn legacy_cursor_time(micros: i64) -> Result<OffsetDateTime, &'static str> {
    if !(-210_866_803_200_000_000..=8_210_266_876_799_999_999).contains(&micros) {
        return Err("outside legacy cursor range");
    }
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(micros) * 1_000)
        .map_err(|_| "outside native time range")
}

/// PostgreSQL 17's finite TIMESTAMP range (also TIMESTAMPTZ in UTC), checked
/// before precision reduction. This is deliberately broader than HTTP cursors.
/// Normalize OffsetDateTime to UTC with checked_to_offset before applying it.
pub fn pg_timestamp(value: PrimitiveDateTime) -> Result<PrimitiveDateTime, &'static str> {
    if !(datetime!(-4713-11-24 0:00)..=datetime!(+294276-12-31 23:59:59.999999)).contains(&value) {
        return Err("outside finite PostgreSQL timestamp range");
    }
    Ok(value)
}

/// Check UTC conversion before SQLx's infallible to_offset path can panic.
/// Returns UTC and checks the finite server range without changing precision.
pub fn pg_instant(value: OffsetDateTime) -> Result<OffsetDateTime, &'static str> {
    let utc = value
        .checked_to_offset(UtcOffset::UTC)
        .ok_or("UTC conversion outside native time range")?;
    pg_timestamp(PrimitiveDateTime::new(utc.date(), utc.time()))?;
    Ok(utc)
}
