//! UTC accounting periods; IANA civil time affects only the due instant.
use chrono::{DateTime, Datelike, Months, NaiveDate, TimeDelta, Utc};
use jiff::tz::TimeZone;

#[derive(Clone, Debug)]
pub(crate) struct Calendar {
    monthly: bool,
    zone: TimeZone,
    hour: i8,
    minute: i8,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Occurrence {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub due: DateTime<Utc>,
}
fn invalid() -> sqlx::Error {
    sqlx::Error::Protocol("invalid report calendar".into())
}
impl Calendar {
    pub(crate) fn new(cadence: &str, zone: &str, time: &str) -> Result<Self, sqlx::Error> {
        if !matches!(cadence, "weekly" | "monthly") {
            return Err(invalid());
        }
        let minutes = crate::jobs::schedule::parse_hhmm(time).ok_or_else(invalid)?;
        Ok(Self {
            monthly: cadence == "monthly",
            zone: TimeZone::get(zone).map_err(|_| invalid())?,
            hour: (minutes / 60) as i8,
            minute: (minutes % 60) as i8,
        })
    }
    fn boundary(&self, now: DateTime<Utc>) -> Result<DateTime<Utc>, sqlx::Error> {
        let d = now.date_naive();
        let d = if self.monthly {
            d.with_day(1)
        } else {
            d.checked_sub_signed(TimeDelta::days(i64::from(
                d.weekday().num_days_from_monday(),
            )))
        }
        .ok_or_else(invalid)?;
        midnight(d)
    }
    pub(crate) fn shift(
        &self,
        end: DateTime<Utc>,
        forward: bool,
    ) -> Result<DateTime<Utc>, sqlx::Error> {
        let d = end.date_naive();
        let d = if self.monthly {
            if forward {
                d.checked_add_months(Months::new(1))
            } else {
                d.checked_sub_months(Months::new(1))
            }
        } else {
            d.checked_add_signed(TimeDelta::days(if forward { 7 } else { -7 }))
        }
        .ok_or_else(invalid)?;
        midnight(d)
    }
    /// First daily preference strictly after closure. Compatible disambiguation
    /// shifts gaps forward by their size and chooses the earlier fold only.
    pub(crate) fn delivery_after(&self, end: DateTime<Utc>) -> Result<DateTime<Utc>, sqlx::Error> {
        let ts = jiff::Timestamp::from_second(end.timestamp()).map_err(|_| invalid())?;
        let local = ts.to_zoned(self.zone.clone());
        let mut d = NaiveDate::from_ymd_opt(
            i32::from(local.year()),
            local.month() as u32,
            local.day() as u32,
        )
        .ok_or_else(invalid)?;
        // Includes skipped civil dates (e.g. Pacific/Apia); bounded, not a
        // minute-by-minute search and never the later repetition of a fold.
        for _ in 0..4 {
            let civil = jiff::civil::DateTime::new(
                d.year().try_into().map_err(|_| invalid())?,
                d.month() as i8,
                d.day() as i8,
                self.hour,
                self.minute,
                0,
                0,
            )
            .map_err(|_| invalid())?;
            let zoned = self.zone.to_zoned(civil).map_err(|_| invalid())?;
            let due =
                DateTime::from_timestamp(zoned.timestamp().as_second(), 0).ok_or_else(invalid)?;
            if due > end {
                return Ok(due);
            }
            d = d.succ_opt().ok_or_else(invalid)?;
        }
        Err(invalid())
    }
    fn occurrence(&self, end: DateTime<Utc>) -> Result<Occurrence, sqlx::Error> {
        Ok(Occurrence {
            start: self.shift(end, false)?,
            end,
            due: self.delivery_after(end)?,
        })
    }
    /// Creation/resumption/edit: the next future occurrence, never replaying a
    /// completed period, even when changing cadence.
    pub(crate) fn next_after(
        &self,
        now: DateTime<Utc>,
        floor: Option<DateTime<Utc>>,
    ) -> Result<Occurrence, sqlx::Error> {
        let mut end = self.boundary(now.max(floor.unwrap_or(now)))?;
        loop {
            let o = self.occurrence(end)?;
            if o.due > now && floor.is_none_or(|floor| end > floor) {
                return Ok(o);
            }
            end = self.shift(end, true)?;
        }
    }
    pub(crate) fn latest_complete(&self, now: DateTime<Utc>) -> Result<Occurrence, sqlx::Error> {
        self.occurrence(self.boundary(now)?)
    }
    pub(crate) fn skipped(&self, first: DateTime<Utc>, last: DateTime<Utc>) -> i64 {
        if self.monthly {
            i64::from(
                (last.year() - first.year()) * 12 + last.month() as i32 - first.month() as i32,
            )
            .max(0)
        } else {
            (last - first).num_days().div_euclid(7).max(0)
        }
    }
}
fn midnight(d: NaiveDate) -> Result<DateTime<Utc>, sqlx::Error> {
    Ok(d.and_hms_opt(0, 0, 0).ok_or_else(invalid)?.and_utc())
}
