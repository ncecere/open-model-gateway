//! Timestamp component only: UUID/session grammar and ordering remain consumer-owned.
use crate::{Error, UtcInstant};

pub const MIN_MICROS: i64 = -210_866_803_200_000_000;
pub const MAX_MICROS: i64 = 8_210_266_876_799_999_999;

/// Old PostgreSQL/carrier intersection, not `time`'s larger range.
/// Rust integer syntax intentionally preserves `+0`, `-0` and leading zeros.
pub fn parse_micros(text: &str) -> Result<UtcInstant, Error> {
    let micros: i64 = text.parse().map_err(|_| Error::Syntax)?;
    if !(MIN_MICROS..=MAX_MICROS).contains(&micros) {
        return Err(Error::Range);
    }
    UtcInstant::from_unix_micros(micros)
}

/// Holds require the canonical decimal spelling; other cursor endpoints do not.
pub fn parse_canonical_micros(text: &str) -> Result<UtcInstant, Error> {
    let value = parse_micros(text)?;
    if value.unix_micros().to_string() != text {
        return Err(Error::Syntax);
    }
    Ok(value)
}
