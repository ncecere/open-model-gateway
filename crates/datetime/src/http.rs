//! Compatibility grammars, not a universal ISO 8601 parser or an HTTP handler.
//! Callers still enforce their existing byte limits (e.g. provider imports: 40).
use crate::{CalendarDate, Error, UtcInstant};
use time::{Time, UtcOffset};

struct Input<'a>(&'a str);
impl Input<'_> {
    fn space(&mut self) {
        self.0 = self.0.trim_start();
    }
    fn literal(&mut self, text: &str) -> Result<(), Error> {
        self.0 = self.0.strip_prefix(text).ok_or(Error::Syntax)?;
        Ok(())
    }
    fn number(&mut self, width: usize, exact: bool) -> Result<i32, Error> {
        let n = self
            .0
            .bytes()
            .take(width)
            .take_while(u8::is_ascii_digit)
            .count();
        if n == 0 || (exact && n != width) {
            return Err(Error::Syntax);
        }
        let value = self.0[..n].parse().map_err(|_| Error::Range)?;
        self.0 = &self.0[n..];
        Ok(value)
    }
    fn component(&mut self, relaxed: bool) -> Result<u8, Error> {
        if relaxed {
            self.space();
        }
        u8::try_from(self.number(2, !relaxed)?).map_err(|_| Error::Range)
    }
    fn date(&mut self, relaxed: bool, spaces_before_dash: bool) -> Result<CalendarDate, Error> {
        if relaxed {
            self.space();
        }
        let sign = if relaxed && self.0.starts_with(['+', '-']) {
            let negative = self.0.starts_with('-');
            self.0 = &self.0[1..];
            Some(if negative { -1 } else { 1 })
        } else {
            None
        };
        let year =
            self.number(if sign.is_some() { usize::MAX } else { 4 }, !relaxed)? * sign.unwrap_or(1);
        if spaces_before_dash {
            self.space();
        }
        self.literal("-")?;
        let month = self.component(relaxed)?;
        if spaces_before_dash {
            self.space();
        }
        self.literal("-")?;
        let day = self.component(relaxed)?;
        CalendarDate::new(year, month, day)
    }
    fn fraction(&mut self) -> Result<u32, Error> {
        if !self.0.starts_with('.') {
            return Ok(0);
        }
        self.0 = &self.0[1..];
        let length = self.0.bytes().take_while(u8::is_ascii_digit).count();
        if length == 0 {
            return Err(Error::Syntax);
        }
        // Truncate after nine digits without parsing a huge integer or allocating.
        let digits = length.min(9);
        let nanos = self.0[..digits].parse::<u32>().map_err(|_| Error::Range)?
            * 10_u32.pow((9 - digits) as u32);
        self.0 = &self.0[length..];
        Ok(nanos)
    }
    fn offset(&mut self, relaxed: bool) -> Result<i32, Error> {
        if relaxed {
            self.space();
        }
        if relaxed
            && self
                .0
                .get(..3)
                .is_some_and(|s| s.eq_ignore_ascii_case("UTC"))
        {
            self.0 = &self.0[3..];
            return Ok(0);
        }
        if self.0.starts_with(['Z', 'z']) {
            self.0 = &self.0[1..];
            return Ok(0);
        }
        let sign = if self.0.starts_with('+') {
            self.0 = &self.0[1..];
            1
        } else if let Some(rest) = self
            .0
            .strip_prefix('-')
            .or_else(|| self.0.strip_prefix('−'))
        {
            self.0 = rest;
            -1
        } else {
            return Err(Error::Syntax);
        };
        let hour = self.number(2, true)?;
        if relaxed {
            self.0 = self
                .0
                .trim_start_matches(|c: char| c == ':' || c.is_whitespace());
        } else {
            self.literal(":")?;
        }
        let minute = self.number(2, true)?;
        if hour > 23 || minute > 59 {
            return Err(Error::Range);
        }
        Ok(sign * (hour * 3600 + minute * 60))
    }
    fn end(&mut self, relaxed: bool) -> Result<(), Error> {
        if relaxed {
            self.space();
        }
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(Error::Syntax)
        }
    }
}

fn timestamp(text: &str, relaxed: bool) -> Result<UtcInstant, Error> {
    let mut input = Input(text);
    let date = input.date(relaxed, relaxed)?;
    if !input.0.starts_with(['T', 't', ' ']) {
        return Err(Error::Syntax);
    }
    input.0 = &input.0[1..];
    let hour = input.component(relaxed)?;
    if relaxed {
        input.space();
    }
    input.literal(":")?;
    let minute = input.component(relaxed)?;
    if relaxed {
        input.space();
    }
    input.literal(":")?;
    let second = input.component(relaxed)?;
    if second > 60 {
        return Err(Error::Range);
    }
    let nanos = input.fraction()?;
    let offset = input.offset(relaxed)?;
    input.end(relaxed)?;
    let clock = Time::from_hms(hour, minute, second.min(59)).map_err(|_| Error::Range)?;
    let local = date
        .into_time()
        .with_time(clock)
        .assume_offset(UtcOffset::from_whole_seconds(offset).map_err(|_| Error::Range)?);
    let utc = UtcInstant::try_from_time(local)?;
    UtcInstant::from_unix_parts(
        utc.unix_seconds(),
        nanos + if second == 60 { 1_000_000_000 } else { 0 },
    )
}

/// Existing `parse_from_rfc3339`: four-digit years, padded fields, `T`/`t`/space,
/// `Z`/`z` or colon offset (including U+2212 minus), arbitrary fractional digits.
pub fn parse_rfc3339(text: &str) -> Result<UtcInstant, Error> {
    timestamp(text, false)
}
/// Existing timestamp serde: signed expanded years, unpadded fields, component
/// whitespace, UTC, and offsets with optional colons/spaces. Normalizes to UTC.
pub fn parse_serde_timestamp(text: &str) -> Result<UtcInstant, Error> {
    timestamp(text, true)
}

/// Existing date serde/from-str grammar, separate from timestamp parsing.
pub fn parse_serde_date(text: &str) -> Result<CalendarDate, Error> {
    let mut input = Input(text);
    let date = input.date(true, true)?;
    input.end(true)?;
    Ok(date)
}
/// Logs/usage/storage's existing length-ten `%Y-%m-%d` filter. Year zero is legal.
/// Padding/whitespace acceptance follows numeric date parsing, not a new regex.
pub fn parse_date_filter(text: &str) -> Result<CalendarDate, Error> {
    if text.len() != 10 {
        return Err(Error::Syntax);
    }
    let mut input = Input(text);
    let date = input.date(true, false)?;
    input.end(false)?;
    Ok(date)
}
/// Financial report filters add their own 1..=9999 year constraint.
pub fn parse_financial_report_date(text: &str) -> Result<CalendarDate, Error> {
    let date = parse_date_filter(text)?;
    if !(1..=9999).contains(&date.year()) {
        return Err(Error::Range);
    }
    Ok(date)
}

/// Logs and usage: exclusive end, 1..=93 days, ending no later than tomorrow.
pub fn logs_usage_period(
    start: CalendarDate,
    end: CalendarDate,
    today: CalendarDate,
) -> Result<(), Error> {
    if !(1..=93).contains(&end.days_since(start)) || end > today.checked_add_days(1)? {
        return Err(Error::Range);
    }
    Ok(())
}
/// Financial reports use this after their narrower date parsing.
pub fn financial_report_period(
    start: CalendarDate,
    end: CalendarDate,
    today: CalendarDate,
) -> Result<(), Error> {
    if !(1..=9999).contains(&start.year()) || !(1..=9999).contains(&end.year()) {
        return Err(Error::Range);
    }
    logs_usage_period(start, end, today)
}
/// Storage: exclusive end, 1..=366 days, ending no later than tomorrow.
pub fn storage_period(
    start: CalendarDate,
    end: CalendarDate,
    today: CalendarDate,
) -> Result<(), Error> {
    if !(1..=366).contains(&end.days_since(start)) || end > today.checked_add_days(1)? {
        return Err(Error::Range);
    }
    Ok(())
}
/// Subject export: inclusive end, up to 93 days, ending no later than today.
pub fn subject_export_period(
    from: CalendarDate,
    to: CalendarDate,
    today: CalendarDate,
) -> Result<(), Error> {
    if !(0..93).contains(&to.days_since(from)) || to > today {
        return Err(Error::Range);
    }
    Ok(())
}
