use gateway_datetime_compat::{CalendarDate, SignedDuration, UtcInstant, http};
#[test]
fn endpoint_leap_difference_and_postgres_epoch_encoding_are_distinct_from_unix_order() {
    let leap = http::parse_rfc3339("2016-12-31T23:59:60.250Z").unwrap();
    let before = http::parse_rfc3339("2016-12-31T23:59:59Z").unwrap();
    let after = http::parse_rfc3339("2017-01-01T00:00:00Z").unwrap();
    assert!(before < leap && leap < after);
    assert_eq!((leap - before).whole_nanoseconds(), 1_250_000_000);
    assert_eq!((after - leap).whole_nanoseconds(), -250_000_000);
    let same_day_after = http::parse_rfc3339("2016-12-31T23:59:59Z").unwrap();
    assert_eq!((leap - same_day_after).whole_nanoseconds(), 1_250_000_000);
    // Difference uses time-of-day endpoint leap accounting, independent of date.
    let earlier_leap = http::parse_rfc3339("2016-12-30T23:58:60Z").unwrap();
    assert_eq!((before - earlier_leap).whole_seconds(), 86_460);
}
#[test]
fn truncation_is_euclidean_at_negative_epoch_and_checked_at_nanos_bounds() {
    let before = http::parse_rfc3339("1969-12-31T23:59:59.999999999Z").unwrap();
    assert_eq!(
        before.duration_trunc(SignedDuration::minutes(1)).unwrap(),
        http::parse_rfc3339("1969-12-31T23:59:00Z").unwrap()
    );
    assert_eq!(
        UtcInstant::UNIX_EPOCH
            .duration_trunc(SignedDuration::minutes(1))
            .unwrap(),
        UtcInstant::UNIX_EPOCH
    );
    assert!(
        UtcInstant::MAX_UTC
            .duration_trunc(SignedDuration::minutes(1))
            .is_err()
    );
    assert!(before.duration_trunc(SignedDuration::ZERO).is_err());
}
#[test]
fn month_shifts_preserve_observed_clock_and_carrier_boundaries() {
    let leap = http::parse_rfc3339("2024-01-31T23:59:60.125Z").unwrap();
    assert_eq!(
        gateway_datetime_compat::format::utc_json(leap.checked_add_months(1).unwrap()),
        "2024-02-29T23:59:60.125Z"
    );
    assert_eq!(CalendarDate::default().to_string(), "1970-01-01");
    assert!(
        UtcInstant::MAX_UTC
            .checked_add_signed(SignedDuration::nanoseconds(1))
            .is_none()
    );
    assert!(
        UtcInstant::MIN_UTC
            .checked_sub_signed(SignedDuration::nanoseconds(1))
            .is_none()
    );
}
