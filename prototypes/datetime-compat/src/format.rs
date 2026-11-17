//! Wire-specific formatters. These do not change PostgreSQL-generated JSON.
use crate::UtcInstant;

fn timestamp(at: UtcInstant, fixed_millis: bool) -> String {
    let (h, m, s) = at.hms();
    let mut text = format!("{}T{h:02}:{m:02}:{s:02}", at.date());
    let fraction = at.subsecond_nanos() % 1_000_000_000;
    if fixed_millis {
        text.push_str(&format!(".{:03}", fraction / 1_000_000));
    } else if fraction == 0 {
    } else if fraction.is_multiple_of(1_000_000) {
        text.push_str(&format!(".{:03}", fraction / 1_000_000));
    } else if fraction.is_multiple_of(1000) {
        text.push_str(&format!(".{:06}", fraction / 1000));
    } else {
        text.push_str(&format!(".{fraction:09}"));
    }
    text.push('Z');
    text
}

/// Former UTC serde spelling: 0/3/6/9 fractional digits, `Z`, expanded signed years.
pub fn utc_json(at: UtcInstant) -> String {
    timestamp(at, false)
}
/// SCIM metadata: exactly three fractional digits, truncating submilliseconds.
pub fn scim_millis(at: UtcInstant) -> String {
    timestamp(at, true)
}
/// Syslog timestamp only; framing/JSON escaping belongs to the existing exporter.
pub fn syslog_millis(at: UtcInstant) -> String {
    timestamp(at, true)
}
/// Existing OTLP signed-nanosecond string, including its `0` out-of-range sentinel.
pub fn otlp_unix_nanos(at: UtcInstant) -> String {
    at.unix_nanos_i64().unwrap_or(0).to_string()
}
