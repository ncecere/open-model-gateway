//! Fixed timestamp presentations used by gateway messages and test fixtures.
//! HTTP parsing, UTC wire serialization and SQL codecs live in the shared carrier.
use gateway_datetime_compat::{CalendarDate, UtcInstant};

pub trait TimestampFormat {
    fn format(&self, pattern: &str) -> String;
    fn to_rfc3339(&self) -> String;
}

fn year_month(d: CalendarDate, separator: char) -> String {
    let day = d.to_string();
    let month = &day[..day.len() - 3];
    let split = month.rfind('-').expect("calendar month separator");
    format!("{}{separator}{}", &month[..split], &month[split + 1..])
}

impl TimestampFormat for CalendarDate {
    fn format(&self, pattern: &str) -> String {
        match pattern {
            "%Y-%m" => year_month(*self, '-'),
            "%Y_%m" => year_month(*self, '_'),
            "%Y-%m-%d" => self.to_string(),
            _ => panic!("unsupported fixed calendar presentation"),
        }
    }
    fn to_rfc3339(&self) -> String {
        self.midnight().expect("validated date").to_rfc3339()
    }
}

impl TimestampFormat for UtcInstant {
    fn format(&self, pattern: &str) -> String {
        let wire = gateway_datetime_compat::format::utc_json(*self);
        let (date, clock) = wire.split_once('T').expect("UTC presentation");
        let clock = &clock[..8];
        match pattern {
            "%Y-%m" | "%Y_%m" | "%Y-%m-%d" => self.date().format(pattern),
            "%Y-%m-%d %H:%M" => format!("{date} {}", &clock[..5]),
            "%Y-%m-%d %H:%M:%S" => format!("{date} {clock}"),
            "%Y-%m-%d %H:%M:%SZ" => format!("{date} {clock}Z"),
            "%Y-%m-%dT%H:%M:%SZ" => format!("{date}T{clock}Z"),
            "%Y-%m-%dT%H:%M:%S%.3fZ" => gateway_datetime_compat::format::syslog_millis(*self),
            "%H:%M" => clock[..5].to_owned(),
            "%a, %d %b %Y %H:%M:%S GMT" => {
                let day = self.date().into_time();
                let weekday = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
                    [day.weekday().number_days_from_monday() as usize];
                let month = [
                    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov",
                    "Dec",
                ][usize::from(self.date().month()) - 1];
                format!(
                    "{weekday}, {:02} {month} {:04} {clock} GMT",
                    self.date().day(),
                    self.date().year()
                )
            }
            _ => panic!("unsupported fixed UTC presentation"),
        }
    }
    fn to_rfc3339(&self) -> String {
        let wire = gateway_datetime_compat::format::utc_json(*self);
        format!(
            "{}+00:00",
            wire.strip_suffix('Z').expect("UTC presentation")
        )
    }
}

/// Strict AWS signing timestamp used only by mock-server signature assertions.
#[cfg(test)]
pub(crate) fn parse_sigv4(value: &str) -> Result<UtcInstant, gateway_datetime_compat::Error> {
    if value.len() != 16
        || !value.is_ascii()
        || value.as_bytes()[8] != b'T'
        || !value.ends_with('Z')
    {
        return Err(gateway_datetime_compat::Error::Syntax);
    }
    gateway_datetime_compat::http::parse_rfc3339(&format!(
        "{}-{}-{}T{}:{}:{}Z",
        &value[..4],
        &value[4..6],
        &value[6..8],
        &value[9..11],
        &value[11..13],
        &value[13..15]
    ))
}

/// UTC fixture constructor with checked calendar/time components.
#[cfg(test)]
pub(crate) fn from_ymd_and_hms(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<UtcInstant> {
    let date = CalendarDate::from_ymd_opt(year, month, day)?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let seconds = i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second);
    date.midnight()
        .ok()?
        .checked_add_signed(gateway_datetime_compat::SignedDuration::seconds(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_presentations_retain_precision_leaps_and_expanded_years() {
        let leap =
            gateway_datetime_compat::http::parse_rfc3339("2016-12-31T23:59:60.123456789Z").unwrap();
        assert_eq!(leap.to_rfc3339(), "2016-12-31T23:59:60.123456789+00:00");
        assert_eq!(
            leap.format("%Y-%m-%dT%H:%M:%S%.3fZ"),
            "2016-12-31T23:59:60.123Z"
        );
        assert_eq!(
            leap.format("%a, %d %b %Y %H:%M:%S GMT"),
            "Sat, 31 Dec 2016 23:59:60 GMT"
        );
        assert_eq!(
            CalendarDate::new(-1, 2, 3).unwrap().format("%Y_%m"),
            "-0001_02"
        );
        assert_eq!(
            CalendarDate::new(10_000, 2, 3).unwrap().format("%Y-%m"),
            "+10000-02"
        );
        assert_eq!(
            parse_sigv4("20261117T010203Z").unwrap().to_rfc3339(),
            "2026-11-17T01:02:03+00:00"
        );
        assert!(parse_sigv4("20261117X010203Z").is_err());
    }
}
