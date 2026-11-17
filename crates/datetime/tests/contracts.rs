use gateway_datetime_compat::{CalendarDate, Error, UtcInstant, cursor, format, http};
use time::{
    Date, Month, OffsetDateTime, SignedDuration, UtcOffset, format_description::well_known::Rfc3339,
};

#[test]
fn utc_json_preserves_literal_precision_groups() {
    for (input, expected) in [
        ("1970-01-01T01:00:00+01:00", "1970-01-01T00:00:00Z"),
        ("1970-01-01T00:00:00.1Z", "1970-01-01T00:00:00.100Z"),
        ("1970-01-01T00:00:00.1234Z", "1970-01-01T00:00:00.123400Z"),
        (
            "1969-12-31T23:59:59.999999999Z",
            "1969-12-31T23:59:59.999999999Z",
        ),
        ("+10000-01-01T00:00:00Z", "+10000-01-01T00:00:00Z"),
        ("-0001-01-01T00:00:00Z", "-0001-01-01T00:00:00Z"),
    ] {
        let at = http::parse_serde_timestamp(input).unwrap();
        assert_eq!(format::utc_json(at), expected);
        assert_eq!(
            serde_json::to_string(&at).unwrap(),
            format!("\"{expected}\"")
        );
        assert_eq!(
            serde_json::from_str::<UtcInstant>(&format!("\"{input}\"")).unwrap(),
            at
        );
    }
}

#[test]
fn leap_second_is_an_observation_not_a_normalization() {
    let at = http::parse_rfc3339("2016-12-31T23:59:60.123456789Z").unwrap();
    assert_eq!(format::utc_json(at), "2016-12-31T23:59:60.123456789Z");
    assert!(at.is_leap_second());
    assert!(at.try_into_time().is_err());
    assert_eq!(at.unix_seconds(), 1_483_228_799);
    assert_eq!(at.unix_micros(), 1_483_228_800_123_456);
}

#[test]
fn cursor_bounds_are_the_old_intersection() {
    for value in [-210_866_803_200_000_000, 8_210_266_876_799_999_999] {
        assert_eq!(
            cursor::parse_micros(&value.to_string())
                .unwrap()
                .unix_micros(),
            value
        );
    }
    for value in [
        "-210866803200000001",
        "8210266876800000000",
        "9223372036854775807",
    ] {
        assert!(cursor::parse_micros(value).is_err(), "{value}");
    }
    assert_eq!(
        format::utc_json(cursor::parse_micros("-210866803200000000").unwrap()),
        "-4713-11-24T00:00:00Z"
    );
    assert_eq!(
        format::utc_json(cursor::parse_micros("8210266876799999999").unwrap()),
        "+262142-12-31T23:59:59.999999Z"
    );
}

#[test]
fn ordinary_duration_addition_is_checked_at_the_old_frontiers() {
    let at = http::parse_rfc3339("1969-12-31T23:59:59.999999999Z").unwrap();
    assert_eq!(
        format::utc_json(at.checked_add(SignedDuration::nanoseconds(1)).unwrap()),
        "1970-01-01T00:00:00Z"
    );
    let leap_day = http::parse_rfc3339("2024-02-28T00:00:00Z").unwrap();
    assert_eq!(
        format::utc_json(leap_day.checked_add(SignedDuration::days(1)).unwrap()),
        "2024-02-29T00:00:00Z"
    );
    let min = http::parse_serde_timestamp("-262143-01-01T00:00:00Z").unwrap();
    let max = http::parse_serde_timestamp("+262142-12-31T23:59:59.999999999Z").unwrap();
    assert_eq!(
        min.checked_add(SignedDuration::nanoseconds(-1)),
        Err(Error::Range)
    );
    assert_eq!(
        max.checked_add(SignedDuration::nanoseconds(1)),
        Err(Error::Range)
    );
    assert_eq!(at.checked_add(SignedDuration::MAX), Err(Error::Range));
}

#[test]
fn rfc3339_acceptance_has_literal_utc_results() {
    for (input, expected) in [
        ("2024-02-29t01:02:03z", "2024-02-29T01:02:03Z"),
        ("2024-02-29 01:02:03-00:00", "2024-02-29T01:02:03Z"),
        (
            "1970-01-01T00:00:00.1234567899876543210Z",
            "1970-01-01T00:00:00.123456789Z",
        ),
        ("1970-01-01T00:00:00.0000000009Z", "1970-01-01T00:00:00Z"),
        ("1970-01-01T00:00:00+23:59", "1969-12-31T00:01:00Z"),
        ("1970-01-01T00:00:00−01:30", "1970-01-01T01:30:00Z"),
        ("0000-01-01T00:00:00+01:00", "-0001-12-31T23:00:00Z"),
        ("9999-12-31T23:59:59-01:00", "+10000-01-01T00:59:59Z"),
    ] {
        assert_eq!(
            format::utc_json(http::parse_rfc3339(input).unwrap()),
            expected,
            "{input}"
        );
    }
    for input in [
        "+10000-01-01T00:00:00Z",
        "2024-2-29T01:02:03Z",
        "2024-02-29T1:02:03Z",
        "2024-02-29T01:02:03UTC",
        "2024-02-29T01:02:03+0100",
        "2024-02-29T01:02:03+01",
        "2024-02-29X01:02:03Z",
        "2024-02-29T01:02:03.Z",
        "2023-02-29T01:02:03Z",
        "2024-02-29T24:00:00Z",
        "2024-02-29T00:00:61Z",
        "2024-02-29T00:60:00Z",
        "2024-02-29T01:02:03+24:00",
        "2024-02-29T01:02:03+00:60",
        "2024-02-29T01:02:03Z ",
        " 2024-02-29T01:02:03Z",
        "2024-02-29T01:02:03",
        "2024-02-29T01:02:03+01:00:00",
    ] {
        assert!(http::parse_rfc3339(input).is_err(), "{input}");
    }
}

#[test]
fn serde_is_a_separate_relaxed_grammar() {
    for (input, expected) in [
        ("2012-  12-12T12:  12:12Z", "2012-12-12T12:12:12Z"),
        (" 2024 - 2 - 9 1 : 2 : 3 uTc \t", "2024-02-09T01:02:03Z"),
        ("2024-2-9t1:2:3 +0130", "2024-02-08T23:32:03Z"),
        ("2024-2-9T1:2:3−01::\u{2002}30", "2024-02-09T02:32:03Z"),
        ("+10000-1-1T0:0:0Z", "+10000-01-01T00:00:00Z"),
        ("-0001-1-1T0:0:0Z", "-0001-01-01T00:00:00Z"),
        (
            "2024-02-29T00:00:00.1234567890123456789Z",
            "2024-02-29T00:00:00.123456789Z",
        ),
    ] {
        assert_eq!(
            format::utc_json(http::parse_serde_timestamp(input).unwrap()),
            expected,
            "{input}"
        );
    }
    for input in [
        "10000-01-01T00:00:00Z",
        "2024-02-29\t01:02:03Z",
        "2024-02-29T01:02:03 +1:00",
        "2024-02-29T01:02:03 GMT",
        "2024-02-29T01:02:03 +01",
        "2024-02-29T01:02:03 .1Z",
        "2024-02-29T01:02:03,1Z",
        "2024-02-29T01:02:03 UTCx",
        "2024-02-29T01:02:03Z garbage",
        "+262143-01-01T00:00:00Z",
        "-262144-12-31T00:00:00Z",
        "💣",
        "2024-02-29T01:02:03💣",
    ] {
        assert!(http::parse_serde_timestamp(input).is_err(), "{input}");
    }
    for json in ["0", "null", "[]", "{}"] {
        assert!(serde_json::from_str::<UtcInstant>(json).is_err());
    }
}

#[test]
fn leap_seconds_keep_fraction_offset_and_order() {
    for (input, expected) in [
        ("2017-01-01T00:59:60.25+01:00", "2016-12-31T23:59:60.250Z"),
        // The former parser did not consult a leap-second table or require month end.
        ("2024-02-15T12:34:60Z", "2024-02-15T12:34:60Z"),
        ("1969-12-31T23:59:60.5Z", "1969-12-31T23:59:60.500Z"),
    ] {
        let at = http::parse_rfc3339(input).unwrap();
        assert_eq!(format::utc_json(at), expected);
        assert_eq!(at.try_into_time(), Err(Error::LeapSecond));
        assert_eq!(at.checked_add(SignedDuration::ZERO), Ok(at));
    }
    let regular = http::parse_rfc3339("2016-12-31T23:59:59.999999999Z").unwrap();
    let leap = http::parse_rfc3339("2016-12-31T23:59:60.999999999Z").unwrap();
    let next = http::parse_rfc3339("2017-01-01T00:00:00Z").unwrap();
    assert!(regular < leap && leap < next);
    assert!(leap.unix_micros() > next.unix_micros());
    assert_eq!(format::scim_millis(leap), "2016-12-31T23:59:60.999Z");
}

#[test]
fn naive_time_defaults_would_break_frontier_contracts() {
    let at = http::parse_rfc3339("1970-01-01T00:00:00.123400Z").unwrap();
    let native = at.try_into_time().unwrap();
    assert_ne!(
        serde_json::to_string(&native).unwrap(),
        "\"1970-01-01T00:00:00.123400Z\""
    );
    assert_eq!(
        native.format(&Rfc3339).unwrap(),
        "1970-01-01T00:00:00.1234Z"
    );
    let leap = OffsetDateTime::parse("2016-12-31T23:59:60.123456789Z", &Rfc3339).unwrap();
    assert_eq!(
        leap.format(&Rfc3339).unwrap(),
        "2016-12-31T23:59:59.999999999Z"
    );
    assert!(OffsetDateTime::parse("2024-02-15T12:34:60Z", &Rfc3339).is_err());
    assert!(OffsetDateTime::parse("2024-02-29X01:02:03Z", &Rfc3339).is_ok());
    let expanded = http::parse_serde_timestamp("+10000-01-01T00:00:00Z").unwrap();
    assert!(expanded.try_into_time().unwrap().format(&Rfc3339).is_err());
    // Both are valid native time values but outside the old cursor contract.
    for micros in [cursor::MIN_MICROS - 1, cursor::MAX_MICROS + 1] {
        assert!(OffsetDateTime::from_unix_timestamp_nanos(i128::from(micros) * 1000).is_ok());
        assert!(cursor::parse_micros(&micros.to_string()).is_err());
    }
}

#[test]
fn integers_native_interop_and_limits_are_checked() {
    for (text, seconds, micros) in [
        ("1969-12-31T23:59:59.999999999Z", -1, -1),
        ("1969-12-31T23:59:59.000000001Z", -1, -1_000_000),
        ("1970-01-01T00:00:00.000000999Z", 0, 0),
        ("1970-01-01T00:00:00.000001Z", 0, 1),
    ] {
        let at = http::parse_rfc3339(text).unwrap();
        assert_eq!(at.unix_seconds(), seconds);
        assert_eq!(at.unix_micros(), micros);
        assert_eq!(
            UtcInstant::try_from_time(at.try_into_time().unwrap()).unwrap(),
            at
        );
    }
    let east = OffsetDateTime::UNIX_EPOCH.to_offset(UtcOffset::from_hms(5, 30, 0).unwrap());
    assert_eq!(
        format::utc_json(UtcInstant::try_from_time(east).unwrap()),
        "1970-01-01T00:00:00Z"
    );
    for (text, nanos) in [
        ("1677-09-21T00:12:43.145224192Z", Some(i64::MIN)),
        ("1677-09-21T00:12:43.145224191Z", None),
        ("2262-04-11T23:47:16.854775807Z", Some(i64::MAX)),
        ("2262-04-11T23:47:16.854775808Z", None),
    ] {
        assert_eq!(http::parse_rfc3339(text).unwrap().unix_nanos_i64(), nanos);
    }
    assert!(UtcInstant::from_unix_parts(0, 1_000_000_000).is_err());
    assert!(UtcInstant::from_unix_parts(59, 2_000_000_000).is_err());
    for seconds in [i64::MIN, i64::MAX] {
        assert!(UtcInstant::from_unix_parts(seconds, 0).is_err());
    }
    for year in [-262_144, 262_143, -999_999, 999_999] {
        assert!(
            UtcInstant::try_from_time(
                Date::from_calendar_date(year, Month::January, 1)
                    .unwrap()
                    .midnight()
                    .assume_utc()
            )
            .is_err()
        );
    }
    for (text, expected) in [
        ("-262143-01-01T00:00:00Z", "-262143-01-01T00:00:00Z"),
        (
            "+262142-12-31T23:59:59.999999999Z",
            "+262142-12-31T23:59:59.999999999Z",
        ),
    ] {
        assert_eq!(
            format::utc_json(http::parse_serde_timestamp(text).unwrap()),
            expected
        );
    }
}

#[test]
fn exporters_have_separate_literal_contracts() {
    let at = http::parse_rfc3339("1969-12-31T23:59:59.123456789Z").unwrap();
    assert_eq!(format::scim_millis(at), "1969-12-31T23:59:59.123Z");
    assert_eq!(format::syslog_millis(at), "1969-12-31T23:59:59.123Z");
    assert_eq!(format::otlp_unix_nanos(at), "-876543211");
    assert_eq!(
        format::otlp_unix_nanos(http::parse_rfc3339("9999-01-01T00:00:00Z").unwrap()),
        "0"
    );
    let zero = UtcInstant::from_unix_parts(0, 0).unwrap();
    assert_eq!(format::utc_json(zero), "1970-01-01T00:00:00Z");
    assert_eq!(format::scim_millis(zero), "1970-01-01T00:00:00.000Z");
    assert_eq!(format::syslog_millis(zero), "1970-01-01T00:00:00.000Z");
}

#[test]
fn cursor_canonicalization_is_consumer_specific() {
    for s in ["+0", "-0", "000", "+000"] {
        assert_eq!(cursor::parse_micros(s).unwrap().unix_micros(), 0);
        assert!(cursor::parse_canonical_micros(s).is_err());
    }
    for s in ["", " 0", "0 ", "1.0", "--1", "0💣", "9223372036854775808"] {
        assert!(cursor::parse_micros(s).is_err(), "{s}");
    }
    for s in ["-1", "0", "1"] {
        assert!(cursor::parse_canonical_micros(s).is_ok());
    }
}

#[test]
fn dates_and_calendar_arithmetic_keep_old_bounds_and_clamping() {
    for (text, expected) in [
        ("0000-01-01", "0000-01-01"),
        ("+10000-1-1", "+10000-01-01"),
        (" -1 - 1 - 1 ", "-0001-01-01"),
    ] {
        let day = http::parse_serde_date(text).unwrap();
        assert_eq!(day.to_string(), expected);
        assert_eq!(CalendarDate::try_from_time(day.into_time()).unwrap(), day);
        assert_eq!(
            serde_json::to_string(&day).unwrap(),
            format!("\"{expected}\"")
        );
    }
    let date = |y, m, d| CalendarDate::new(y, m, d).unwrap();
    assert_eq!(
        date(2024, 1, 31).checked_add_months(1).unwrap().to_string(),
        "2024-02-29"
    );
    assert_eq!(
        date(2023, 3, 31)
            .checked_add_months(-1)
            .unwrap()
            .to_string(),
        "2023-02-28"
    );
    assert_eq!(
        date(0, 1, 31).checked_add_months(-1).unwrap().to_string(),
        "-0001-12-31"
    );
    assert_eq!(
        date(2024, 2, 28).checked_add_days(2).unwrap().to_string(),
        "2024-03-01"
    );
    for (date, delta) in [(date(-262143, 1, 1), -1), (date(262142, 12, 31), 1)] {
        assert_eq!(date.checked_add_days(delta), Err(Error::Range));
        assert_eq!(date.checked_add_months(delta as i32), Err(Error::Range));
    }
    assert!(date(2024, 1, 1).checked_add_days(i64::MAX).is_err());
    assert!(date(2024, 1, 1).checked_add_months(i32::MAX).is_err());
}

#[test]
fn date_filters_do_not_gain_uniform_range_rules() {
    assert!(http::parse_date_filter("0000-01-01").is_ok());
    assert!(http::parse_financial_report_date("0000-01-01").is_err());
    assert!(http::parse_date_filter(" 2024-1-01").is_ok());
    assert!(http::parse_date_filter("2024-1-01 ").is_err());
    assert!(http::parse_date_filter("2024 -1-01").is_err());
    assert!(http::parse_serde_date("2024 -1-01").is_ok());
    for text in ["2024-1-1", "+10000-01-01", "2023-02-29", "2024-00-01"] {
        assert!(http::parse_date_filter(text).is_err(), "{text}");
    }
    let today = CalendarDate::new(2024, 4, 1).unwrap();
    let tomorrow = today.checked_add_days(1).unwrap();
    assert!(
        http::logs_usage_period(tomorrow.checked_add_days(-93).unwrap(), tomorrow, today).is_ok()
    );
    assert!(
        http::logs_usage_period(tomorrow.checked_add_days(-94).unwrap(), tomorrow, today).is_err()
    );
    assert!(
        http::storage_period(tomorrow.checked_add_days(-366).unwrap(), tomorrow, today).is_ok()
    );
    assert!(
        http::storage_period(tomorrow.checked_add_days(-367).unwrap(), tomorrow, today).is_err()
    );
    assert!(
        http::subject_export_period(today.checked_add_days(-92).unwrap(), today, today).is_ok()
    );
    assert!(
        http::subject_export_period(today.checked_add_days(-93).unwrap(), today, today).is_err()
    );
    assert!(http::subject_export_period(today, today, today).is_ok());
    assert!(http::subject_export_period(today, tomorrow, today).is_err());
    for check in [
        http::logs_usage_period,
        http::storage_period,
        http::financial_report_period,
    ] {
        assert!(check(today, today, today).is_err());
        assert!(check(tomorrow, today, today).is_err());
        assert!(check(today, tomorrow.checked_add_days(1).unwrap(), today).is_err());
    }
}
