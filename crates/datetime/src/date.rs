use crate::{Error, UtcInstant};
use time::{Date, Month, SignedDuration};

/// Proleptic Gregorian date in the former date carrier's range, including year 0.
/// HTTP endpoints may impose additional, different constraints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CalendarDate(Date);

impl CalendarDate {
    pub const MIN_YEAR: i32 = -262_143;
    pub const MAX_YEAR: i32 = 262_142;

    pub fn new(year: i32, month: u8, day: u8) -> Result<Self, Error> {
        Self::try_from_time(
            Date::from_calendar_date(year, Month::try_from(month).map_err(|_| Error::Range)?, day)
                .map_err(|_| Error::Range)?,
        )
    }

    pub fn try_from_time(date: Date) -> Result<Self, Error> {
        if !(Self::MIN_YEAR..=Self::MAX_YEAR).contains(&date.year()) {
            return Err(Error::Range);
        }
        Ok(Self(date))
    }

    /// Infallible after the checked construction of this date.
    pub fn into_time(self) -> Date {
        self.0
    }
    pub fn year(self) -> i32 {
        self.0.year()
    }
    pub fn month(self) -> u8 {
        self.0.month() as u8
    }
    pub fn day(self) -> u8 {
        self.0.day()
    }
    pub fn days_since(self, earlier: Self) -> i64 {
        (self.0 - earlier.0).whole_days()
    }
    pub fn midnight(self) -> Result<UtcInstant, Error> {
        UtcInstant::try_from_time(self.0.midnight().assume_utc())
    }

    pub fn checked_add_days(self, days: i64) -> Result<Self, Error> {
        let duration = SignedDuration::seconds(days.checked_mul(86_400).ok_or(Error::Range)?);
        Self::try_from_time(self.0.checked_add(duration).ok_or(Error::Range)?)
    }

    /// Calendar-month shift, clamping to the last valid day (not a fixed duration).
    pub fn checked_add_months(self, months: i32) -> Result<Self, Error> {
        let index = i64::from(self.year()) * 12 + i64::from(self.month()) - 1 + i64::from(months);
        let year = i32::try_from(index.div_euclid(12)).map_err(|_| Error::Range)?;
        let month = (index.rem_euclid(12) + 1) as u8;
        let first = Self::new(year, month, 1)?;
        Self::new(year, month, self.day().min(first.0.month().length(year)))
    }
}

impl TryFrom<Date> for CalendarDate {
    type Error = Error;
    fn try_from(value: Date) -> Result<Self, Error> {
        Self::try_from_time(value)
    }
}
impl From<CalendarDate> for Date {
    fn from(value: CalendarDate) -> Self {
        value.into_time()
    }
}
impl std::fmt::Display for CalendarDate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let y = self.year();
        if y < 0 {
            write!(f, "-{abs:04}", abs = -y)?;
        } else if y > 9999 {
            write!(f, "+{y}")?;
        } else {
            write!(f, "{y:04}")?;
        }
        write!(f, "-{:02}-{:02}", self.month(), self.day())
    }
}

impl serde::Serialize for CalendarDate {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
impl<'de> serde::Deserialize<'de> for CalendarDate {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = CalendarDate;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a calendar date string")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                crate::http::parse_serde_date(v).map_err(E::custom)
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}
