//! PostgreSQL delegates: primitive codecs own wire parsing; checked arithmetic
//! precedes date construction, including inside SQLx's Option/array decoders.
use crate::{CalendarDate, Error, SignedDuration, UtcInstant};
use sqlx::{
    Decode, Encode, Postgres, Type,
    encode::IsNull,
    error::BoxDynError,
    postgres::{PgArgumentBuffer, PgHasArrayType, PgTypeInfo, PgValueFormat, PgValueRef},
};
use time::{Date, Month, OffsetDateTime};

fn epoch_date() -> Date {
    Date::from_calendar_date(2000, Month::January, 1).expect("valid epoch")
}
const PG_EPOCH_SECONDS: i64 = 946_684_800;

fn text_instant(text: &str) -> Result<UtcInstant, Error> {
    // PostgreSQL ISO text omits minutes in whole-hour offsets (+00 or -08).
    // Complete that SQL spelling before the bounded timestamp parser normalizes UTC.
    let (_, clock) = text.split_once(' ').ok_or(Error::Syntax)?;
    let offset_start = clock.find(['+', '-']).ok_or(Error::Syntax)?;
    if clock[offset_start..].len() == 3 {
        crate::http::parse_rfc3339(&format!("{text}:00"))
    } else {
        crate::http::parse_rfc3339(text)
    }
}

impl Type<Postgres> for CalendarDate {
    fn type_info() -> PgTypeInfo {
        <Date as Type<Postgres>>::type_info()
    }
}
impl PgHasArrayType for CalendarDate {
    fn array_type_info() -> PgTypeInfo {
        <Date as PgHasArrayType>::array_type_info()
    }
}
impl Encode<'_, Postgres> for CalendarDate {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        <Date as Encode<Postgres>>::encode(self.into_time(), buf)
    }
    fn size_hint(&self) -> usize {
        4
    }
}
impl<'r> Decode<'r, Postgres> for CalendarDate {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        let date = match value.format() {
            PgValueFormat::Binary => {
                // DATE infinity and dates beyond native range must never reach
                // SQLx's unchecked native DATE addition. i32 decoding only parses
                // the primitive integer, independently of the SQL type identity.
                let days = <i32 as Decode<Postgres>>::decode(value)?;
                epoch_date()
                    .checked_add(SignedDuration::days(i64::from(days)))
                    .ok_or(Error::Range)?
            }
            PgValueFormat::Text => <Date as Decode<Postgres>>::decode(value)?,
        };
        Ok(CalendarDate::try_from_time(date)?)
    }
}
impl Type<Postgres> for UtcInstant {
    fn type_info() -> PgTypeInfo {
        <OffsetDateTime as Type<Postgres>>::type_info()
    }
}
impl PgHasArrayType for UtcInstant {
    fn array_type_info() -> PgTypeInfo {
        <OffsetDateTime as PgHasArrayType>::array_type_info()
    }
}
impl Encode<'_, Postgres> for UtcInstant {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        // Old SQLx/Chrono encoded a signed duration from 2000, including the
        // retained leap subsecond. Whole microseconds truncate toward that epoch.
        // Delegating the resulting integer preserves that behavior without
        // forcing a leap observation through native OffsetDateTime.
        let nanos = i128::from(self.unix_seconds() - PG_EPOCH_SECONDS) * 1_000_000_000
            + i128::from(self.subsecond_nanos());
        let micros = i64::try_from(nanos / 1000).map_err(|_| Error::Range)?;
        <i64 as Encode<Postgres>>::encode(micros, buf)
    }
    fn size_hint(&self) -> usize {
        8
    }
}
impl<'r> Decode<'r, Postgres> for UtcInstant {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.format() {
            PgValueFormat::Binary => {
                let micros = <i64 as Decode<Postgres>>::decode(value)?;
                // A checked primitive decode rejects infinity before the native
                // timestamp decoder could turn it into an unrelated finite date.
                let base = epoch_date().midnight().assume_utc();
                let at = base
                    .checked_add(SignedDuration::microseconds(micros))
                    .ok_or(Error::Range)?;
                Ok(UtcInstant::try_from_time(at)?)
            }
            PgValueFormat::Text => Ok(text_instant(value.as_str()?)?),
        }
    }
}
