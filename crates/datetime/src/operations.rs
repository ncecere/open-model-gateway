//! Explicit carrier operations shared by the gateway and the SDK date/clock APIs.
use crate::{CalendarDate, Error, SignedDuration, UtcInstant};

impl UtcInstant {
    pub const MIN_UTC: Self = Self {
        anchor: match time::Date::from_calendar_date(-262143, time::Month::January, 1) {
            Ok(date) => date.midnight().assume_utc(),
            Err(_) => panic!("valid minimum date"),
        },
        subsecond: 0,
    };
    pub const MAX_UTC: Self = Self {
        anchor: match time::Date::from_calendar_date(262142, time::Month::December, 31) {
            Ok(date) => match date.with_hms(23, 59, 59) {
                Ok(at) => at.assume_utc(),
                Err(_) => panic!("valid maximum clock"),
            },
            Err(_) => panic!("valid maximum date"),
        },
        subsecond: 999_999_999,
    };
    pub const UNIX_EPOCH: Self = Self {
        anchor: time::OffsetDateTime::UNIX_EPOCH,
        subsecond: 0,
    };
    pub fn now() -> Self {
        Self::try_from_time(time::OffsetDateTime::now_utc())
            .expect("system clock within supported UTC range")
    }
    pub fn from_timestamp(seconds: i64, nanos: u32) -> Option<Self> {
        Self::from_unix_parts(seconds, nanos).ok()
    }
    pub fn from_timestamp_micros(micros: i64) -> Option<Self> {
        Self::from_unix_micros(micros).ok()
    }
    pub fn checked_add_months(self, months: i32) -> Result<Self, Error> {
        let day = self.date().checked_add_months(months)?;
        let base = day.into_time().with_time(self.anchor.time()).assume_utc();
        Self::from_unix_parts(base.unix_timestamp(), self.subsecond)
    }
    pub fn timestamp_subsec_nanos(self) -> u32 {
        self.subsecond_nanos()
    }
    pub fn timestamp(self) -> i64 {
        self.unix_seconds()
    }
    pub fn timestamp_micros(self) -> i64 {
        self.unix_micros()
    }
    pub fn timestamp_nanos_opt(self) -> Option<i64> {
        self.unix_nanos_i64()
    }
    pub fn date_naive(self) -> CalendarDate {
        self.date()
    }
    pub fn year(self) -> i32 {
        self.date().year()
    }
    pub fn month(self) -> u32 {
        u32::from(self.date().month())
    }
    pub fn day(self) -> u32 {
        u32::from(self.date().day())
    }
    pub fn hour(self) -> u32 {
        u32::from(self.hms().0)
    }
    pub fn minute(self) -> u32 {
        u32::from(self.hms().1)
    }
    /// Whole-second anchor; a leap observation is retained in the subsecond.
    pub fn second(self) -> u32 {
        u32::from(self.anchor.second())
    }
    pub fn checked_add_signed(self, d: SignedDuration) -> Option<Self> {
        self.checked_add(d).ok()
    }
    pub fn checked_sub_signed(self, d: SignedDuration) -> Option<Self> {
        self.checked_add(d.checked_neg()?).ok()
    }
    pub fn signed_duration_since(self, earlier: Self) -> SignedDuration {
        // The old carrier counts only the leap observations at the two endpoints,
        // using their time-of-day ordering even when the calendar dates differ.
        let left = self.unix_seconds().rem_euclid(86400);
        let right = earlier.unix_seconds().rem_euclid(86400);
        let adjustment = if left > right && earlier.is_leap_second() {
            1
        } else if left < right && self.is_leap_second() {
            -1
        } else {
            0
        };
        SignedDuration::seconds(self.unix_seconds() - earlier.unix_seconds() + adjustment)
            + SignedDuration::nanoseconds(
                i64::from(self.subsecond_nanos()) - i64::from(earlier.subsecond_nanos()),
            )
    }
    pub fn duration_trunc(self, duration: SignedDuration) -> Result<Self, Error> {
        let nanos = self.unix_nanos_i64().ok_or(Error::Range)?;
        let width = i64::try_from(duration.whole_nanoseconds()).map_err(|_| Error::Range)?;
        if width <= 0 {
            return Err(Error::Range);
        }
        self.checked_add(SignedDuration::nanoseconds(-nanos.rem_euclid(width)))
    }
}
impl CalendarDate {
    pub fn from_ymd_opt(year: i32, month: u32, day: u32) -> Option<Self> {
        Self::new(year, month.try_into().ok()?, day.try_into().ok()?).ok()
    }
    pub fn with_day(self, day: u32) -> Option<Self> {
        Self::from_ymd_opt(self.year(), self.month().into(), day)
    }
    pub fn succ_opt(self) -> Option<Self> {
        self.checked_add_days(1).ok()
    }
    pub fn pred_opt(self) -> Option<Self> {
        self.checked_add_days(-1).ok()
    }
    pub fn weekday(self) -> time::Weekday {
        self.into_time().weekday()
    }
    pub fn checked_add_signed(self, d: SignedDuration) -> Option<Self> {
        self.checked_add_days(d.whole_days()).ok()
    }
    pub fn checked_sub_signed(self, d: SignedDuration) -> Option<Self> {
        self.checked_add_days(-d.whole_days()).ok()
    }
    pub fn signed_duration_since(self, earlier: Self) -> SignedDuration {
        SignedDuration::days(self.days_since(earlier))
    }
}
impl std::str::FromStr for UtcInstant {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        crate::http::parse_serde_timestamp(s)
    }
}
impl std::str::FromStr for CalendarDate {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        crate::http::parse_serde_date(s)
    }
}
impl std::fmt::Display for UtcInstant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let wire = crate::format::utc_json(*self);
        write!(
            f,
            "{} {} UTC",
            &wire[..wire.find('T').expect("wire date separator")],
            wire.split_once('T')
                .expect("wire date separator")
                .1
                .trim_end_matches('Z')
        )
    }
}
macro_rules! arithmetic {
    ($ty:ty) => {
        impl std::ops::Add<SignedDuration> for $ty {
            type Output = Self;
            fn add(self, d: SignedDuration) -> Self {
                self.checked_add_signed(d)
                    .expect("date/time addition within supported range")
            }
        }
        impl std::ops::Sub<SignedDuration> for $ty {
            type Output = Self;
            fn sub(self, d: SignedDuration) -> Self {
                self.checked_sub_signed(d)
                    .expect("date/time subtraction within supported range")
            }
        }
        impl std::ops::Sub for $ty {
            type Output = SignedDuration;
            fn sub(self, rhs: Self) -> SignedDuration {
                self.signed_duration_since(rhs)
            }
        }
    };
}
arithmetic!(UtcInstant);
arithmetic!(CalendarDate);

impl Default for CalendarDate {
    fn default() -> Self {
        Self::new(1970, 1, 1).expect("valid default date")
    }
}
