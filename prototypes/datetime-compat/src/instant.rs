use crate::{CalendarDate, Error};
use time::{OffsetDateTime, SignedDuration, UtcOffset};

/// UTC instant/observation. A leap second retains `:60` and its fractional part.
/// Ordering keeps `:59` < `:60` < the next minute; Unix encodings overlap there,
/// as they did in the previous carrier. Never compare leap observations by Unix integers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcInstant {
    // Whole-second UTC anchor; for a leap observation this is the preceding :59.
    anchor: OffsetDateTime,
    // 0..1e9 normally; 1e9..2e9 for a retained leap-second observation.
    subsecond: u32,
}

impl UtcInstant {
    pub fn from_unix_parts(seconds: i64, nanoseconds: u32) -> Result<Self, Error> {
        if nanoseconds >= 2_000_000_000
            || (nanoseconds >= 1_000_000_000 && seconds.rem_euclid(60) != 59)
        {
            return Err(Error::Range);
        }
        let anchor = OffsetDateTime::from_unix_timestamp(seconds).map_err(|_| Error::Range)?;
        CalendarDate::try_from_time(anchor.date())?;
        Ok(Self {
            anchor,
            subsecond: nanoseconds,
        })
    }

    pub fn from_unix_micros(value: i64) -> Result<Self, Error> {
        Self::from_unix_parts(
            value.div_euclid(1_000_000),
            (value.rem_euclid(1_000_000) * 1000) as u32,
        )
    }

    pub fn try_from_time(value: OffsetDateTime) -> Result<Self, Error> {
        let utc = value
            .checked_to_offset(UtcOffset::UTC)
            .ok_or(Error::Range)?;
        Self::from_unix_parts(utc.unix_timestamp(), utc.nanosecond())
    }

    pub fn try_into_time(self) -> Result<OffsetDateTime, Error> {
        if self.is_leap_second() {
            return Err(Error::LeapSecond);
        }
        self.anchor
            .replace_nanosecond(self.subsecond)
            .map_err(|_| Error::Range)
    }

    pub fn is_leap_second(self) -> bool {
        self.subsecond >= 1_000_000_000
    }
    pub fn unix_seconds(self) -> i64 {
        self.anchor.unix_timestamp()
    }
    /// Subsecond value can exceed 1e9 for a leap observation; never silently clamp it.
    pub fn subsecond_nanos(self) -> u32 {
        self.subsecond
    }
    /// Integer Unix microseconds, flooring fractions, including before the epoch.
    /// Leap observations overlap the next second in this legacy encoding.
    pub fn unix_micros(self) -> i64 {
        self.unix_seconds() * 1_000_000 + i64::from(self.subsecond / 1000)
    }
    pub fn unix_nanos_i64(self) -> Option<i64> {
        i64::try_from(i128::from(self.unix_seconds()) * 1_000_000_000 + i128::from(self.subsecond))
            .ok()
    }
    pub fn date(self) -> CalendarDate {
        CalendarDate::try_from_time(self.anchor.date()).expect("constructor validates the UTC date")
    }
    pub(crate) fn hms(self) -> (u8, u8, u8) {
        (
            self.anchor.hour(),
            self.anchor.minute(),
            self.anchor.second() + u8::from(self.is_leap_second()),
        )
    }

    /// Duration arithmetic on retained leap observations remains an integration gap.
    /// Refuse rather than silently erase the observation through native conversion.
    pub fn checked_add(self, duration: SignedDuration) -> Result<Self, Error> {
        Self::try_from_time(
            self.try_into_time()?
                .checked_add(duration)
                .ok_or(Error::Range)?,
        )
    }
}

impl TryFrom<OffsetDateTime> for UtcInstant {
    type Error = Error;
    fn try_from(value: OffsetDateTime) -> Result<Self, Error> {
        Self::try_from_time(value)
    }
}
impl TryFrom<UtcInstant> for OffsetDateTime {
    type Error = Error;
    fn try_from(value: UtcInstant) -> Result<Self, Error> {
        value.try_into_time()
    }
}
impl serde::Serialize for UtcInstant {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&crate::format::utc_json(*self))
    }
}
impl<'de> serde::Deserialize<'de> for UtcInstant {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = UtcInstant;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a relaxed UTC timestamp string")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                crate::http::parse_serde_timestamp(v).map_err(E::custom)
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}
