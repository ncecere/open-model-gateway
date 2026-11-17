use time::{Date, Duration, Month, PrimitiveDateTime, macros::datetime};
use time_sqlx_proof::{legacy_cursor_time, pg_instant, pg_timestamp};

#[test]
fn native_range_must_not_widen_legacy_cursor_acceptance() {
    for (micros, accepted) in [
        (-210_866_803_200_000_001, false),
        (-210_866_803_200_000_000, true),
        (-1, true),
        (0, true),
        (8_210_266_876_799_999_999, true),
        (8_210_266_876_800_000_000, false),
        (i64::MAX, false),
    ] {
        assert_eq!(legacy_cursor_time(micros).is_ok(), accepted, "{micros}");
    }
}

#[test]
fn postgres_timestamp_bounds_require_explicit_checks() {
    let min = datetime!(-4713-11-24 0:00);
    let max = Date::from_calendar_date(294276, Month::December, 31)
        .unwrap()
        .with_hms_micro(23, 59, 59, 999_999)
        .unwrap();
    assert!(pg_timestamp(min).is_ok());
    assert!(pg_timestamp(max).is_ok());
    for bad in [
        min - Duration::microseconds(1),
        max + Duration::microseconds(1),
        PrimitiveDateTime::MIN,
        PrimitiveDateTime::MAX,
    ] {
        assert!(pg_timestamp(bad).is_err(), "{bad}");
    }
}

#[test]
fn cursor_microseconds_use_floor_based_unix_parts_without_floats() {
    let negative = legacy_cursor_time(-1).unwrap();
    assert_eq!(negative.unix_timestamp(), -1);
    assert_eq!(negative.nanosecond(), 999_999_000);
    assert_eq!(
        legacy_cursor_time(-210_866_803_200_000_000).unwrap(),
        datetime!(-4713-11-24 0:00 UTC)
    );
    assert_eq!(
        legacy_cursor_time(8_210_266_876_799_999_999).unwrap(),
        datetime!(+262142-12-31 23:59:59.999999 UTC)
    );
}

#[test]
fn offset_normalization_is_checked_before_encoding() {
    assert_eq!(
        pg_instant(datetime!(2000-01-01 5:45 +5:45)).unwrap(),
        datetime!(2000-01-01 0:00 UTC)
    );
    // Local calendar values are valid but their UTC instants cross server limits.
    assert!(pg_instant(datetime!(-4713-11-24 0:00 +1)).is_err());
    assert!(pg_instant(datetime!(+294276-12-31 23:59:59.999999 -1)).is_err());
    // time permits an OffsetDateTime whose local date is valid but UTC is not.
    let beyond_native_utc = datetime!(-999999-01-01 0:00 +1);
    assert!(
        beyond_native_utc
            .checked_to_offset(time::UtcOffset::UTC)
            .is_none()
    );
    assert!(pg_instant(beyond_native_utc).is_err());
    for nanos in [i128::MIN, i128::MAX] {
        assert!(time::OffsetDateTime::from_unix_timestamp_nanos(nanos).is_err());
    }
}
