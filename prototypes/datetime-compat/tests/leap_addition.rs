use gateway_datetime_compat::{Error, UtcInstant, format, http};
use time::SignedDuration;

fn at(text: &str) -> UtcInstant {
    http::parse_serde_timestamp(text).unwrap()
}

#[test]
fn retained_leap_addition_has_literal_duration_results() {
    // Independent literal results for the former carrier's documented 60.300 example,
    // plus whole-second, signed mixed-fraction and day boundaries.
    let leap = at("2016-07-08T03:05:60.300Z");
    for (duration, expected) in [
        (SignedDuration::ZERO, "2016-07-08T03:05:60.300Z"),
        (
            SignedDuration::milliseconds(-500),
            "2016-07-08T03:05:59.800Z",
        ),
        (
            SignedDuration::milliseconds(500),
            "2016-07-08T03:05:60.800Z",
        ),
        (
            SignedDuration::milliseconds(800),
            "2016-07-08T03:06:00.100Z",
        ),
        (SignedDuration::seconds(1), "2016-07-08T03:06:00.300Z"),
        (SignedDuration::seconds(-1), "2016-07-08T03:05:59.300Z"),
        (SignedDuration::seconds(10), "2016-07-08T03:06:09.300Z"),
        (SignedDuration::seconds(-10), "2016-07-08T03:05:50.300Z"),
        (
            SignedDuration::milliseconds(1500),
            "2016-07-08T03:06:00.800Z",
        ),
        (
            SignedDuration::milliseconds(-1500),
            "2016-07-08T03:05:58.800Z",
        ),
        (SignedDuration::days(1), "2016-07-09T03:05:59.300Z"),
        (SignedDuration::days(-1), "2016-07-07T03:06:00.300Z"),
    ] {
        assert_eq!(
            format::utc_json(leap.checked_add(duration).unwrap()),
            expected,
            "{duration:?}"
        );
    }
    assert_eq!(leap.try_into_time(), Err(Error::LeapSecond));
}

#[test]
fn fractional_carry_borrow_and_leap_retention_are_exact() {
    for (input, nanos, expected, retained) in [
        (
            "2016-12-31T23:59:60.300Z",
            699_999_999,
            "2016-12-31T23:59:60.999999999Z",
            true,
        ),
        (
            "2016-12-31T23:59:60.300Z",
            700_000_000,
            "2017-01-01T00:00:00Z",
            false,
        ),
        (
            "2016-12-31T23:59:60Z",
            -1,
            "2016-12-31T23:59:59.999999999Z",
            false,
        ),
        (
            "2016-12-31T23:59:60.999999999Z",
            1,
            "2017-01-01T00:00:00Z",
            false,
        ),
        (
            "2016-12-31T23:59:60.900Z",
            -900_000_000,
            "2016-12-31T23:59:60Z",
            true,
        ),
        (
            "2016-12-31T23:59:60.900Z",
            -900_000_001,
            "2016-12-31T23:59:59.999999999Z",
            false,
        ),
        (
            "2016-12-31T23:59:60.999999999Z",
            -999_999_999,
            "2016-12-31T23:59:60Z",
            true,
        ),
        (
            "2016-12-31T23:59:60.999999999Z",
            -1_000_000_000,
            "2016-12-31T23:59:59.999999999Z",
            false,
        ),
        (
            "2016-12-31T23:59:60.999999999Z",
            1_000_000_001,
            "2017-01-01T00:00:01Z",
            false,
        ),
        (
            "2016-12-31T23:59:60Z",
            -1_000_000_001,
            "2016-12-31T23:59:58.999999999Z",
            false,
        ),
        (
            "1969-12-31T23:59:60.300Z",
            -1_500_000_000,
            "1969-12-31T23:59:58.800Z",
            false,
        ),
        (
            "1969-12-31T23:59:60.300Z",
            800_000_000,
            "1970-01-01T00:00:00.100Z",
            false,
        ),
        (
            "2024-02-29T23:59:60.300Z",
            86_400_000_000_000,
            "2024-03-01T23:59:59.300Z",
            false,
        ),
        (
            "2024-03-01T00:00:60.300Z",
            -86_400_000_000_000,
            "2024-02-29T00:01:00.300Z",
            false,
        ),
    ] {
        let result = at(input)
            .checked_add(SignedDuration::nanoseconds(nanos))
            .unwrap();
        assert_eq!(format::utc_json(result), expected, "{input} + {nanos}ns");
        assert_eq!(result.is_leap_second(), retained);
        if retained {
            assert_eq!(result.try_into_time(), Err(Error::LeapSecond));
        } else {
            assert!(result.try_into_time().is_ok());
        }
    }
}

#[test]
fn leap_addition_enforces_old_date_bounds_without_intermediate_normalization() {
    let last = at("+262142-12-31T23:59:60.900Z");
    assert_eq!(last.checked_add(SignedDuration::ZERO), Ok(last));
    assert_eq!(
        format::utc_json(
            last.checked_add(SignedDuration::nanoseconds(99_999_999))
                .unwrap()
        ),
        "+262142-12-31T23:59:60.999999999Z"
    );
    assert_eq!(
        format::utc_json(last.checked_add(SignedDuration::seconds(-1)).unwrap()),
        "+262142-12-31T23:59:59.900Z"
    );
    assert_eq!(
        last.checked_add(SignedDuration::milliseconds(100)),
        Err(Error::Range)
    );
    assert_eq!(last.checked_add(SignedDuration::days(1)), Err(Error::Range));

    let first = at("-262143-01-01T00:00:60.300Z");
    assert_eq!(
        format::utc_json(
            first
                .checked_add(SignedDuration::milliseconds(-60_300))
                .unwrap()
        ),
        "-262143-01-01T00:00:00Z"
    );
    assert_eq!(
        first.checked_add(SignedDuration::nanoseconds(-60_300_000_001)),
        Err(Error::Range)
    );
    for duration in [
        SignedDuration::MIN,
        SignedDuration::MAX,
        SignedDuration::days(1_000_000_000),
        SignedDuration::days(-1_000_000_000),
    ] {
        assert_eq!(first.checked_add(duration), Err(Error::Range));
        assert_eq!(last.checked_add(duration), Err(Error::Range));
    }
}

#[test]
fn ordinary_instants_do_not_gain_leaps_and_addition_is_not_associative() {
    let ordinary = at("2016-12-31T23:59:59.300Z");
    assert_eq!(
        format::utc_json(ordinary.checked_add(SignedDuration::seconds(1)).unwrap()),
        "2017-01-01T00:00:00.300Z"
    );
    let leap = at("2016-12-31T23:59:60.300Z");
    let borrowed = leap
        .checked_add(SignedDuration::milliseconds(-500))
        .unwrap();
    assert_eq!(
        format::utc_json(
            borrowed
                .checked_add(SignedDuration::milliseconds(500))
                .unwrap()
        ),
        "2017-01-01T00:00:00.300Z"
    );
    assert_eq!(
        format::utc_json(leap.checked_add(SignedDuration::ZERO).unwrap()),
        "2016-12-31T23:59:60.300Z"
    );
    assert!(
        !borrowed
            .checked_add(SignedDuration::milliseconds(500))
            .unwrap()
            .is_leap_second()
    );
}

#[test]
fn generated_leap_interval_boundaries_and_whole_seconds_stay_exact() {
    let mut seed = 0x0067_4add_2026_u64;
    for index in 0..20_000 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let fraction = match index % 4 {
            0 => 0,
            1 => 999_999_999,
            _ => (seed % 1_000_000_000) as u32,
        };
        // Minute-aligned anchors span negative/pre-epoch and positive dates.
        let seconds = ((seed.rotate_left(17) % 10_000_000) as i64 - 5_000_000) * 60 + 59;
        let leap = UtcInstant::from_unix_parts(seconds, 1_000_000_000 + fraction).unwrap();
        assert_eq!(leap.checked_add(SignedDuration::ZERO), Ok(leap));
        let f = i64::from(fraction);
        for (delta, expected_subsecond) in [(-f, 1_000_000_000), (999_999_999 - f, 1_999_999_999)] {
            let shifted = leap
                .checked_add(SignedDuration::nanoseconds(delta))
                .unwrap();
            assert_eq!(shifted.unix_seconds(), seconds);
            assert_eq!(shifted.subsecond_nanos(), expected_subsecond);
            assert_eq!(shifted.try_into_time(), Err(Error::LeapSecond));
            assert_eq!(
                shifted.checked_add(SignedDuration::nanoseconds(-delta)),
                Ok(leap)
            );
        }
        let before = leap
            .checked_add(SignedDuration::nanoseconds(-f - 1))
            .unwrap();
        assert_eq!(before.unix_seconds(), seconds);
        assert_eq!(before.subsecond_nanos(), 999_999_999);
        let after = leap
            .checked_add(SignedDuration::nanoseconds(1_000_000_000 - f))
            .unwrap();
        assert_eq!(after.unix_seconds(), seconds + 1);
        assert_eq!(after.subsecond_nanos(), 0);
        for duration in [1, 10, 86_400, -1, -10, -86_400] {
            let shifted = leap.checked_add(SignedDuration::seconds(duration)).unwrap();
            assert_eq!(
                shifted.unix_seconds(),
                seconds + duration + i64::from(duration < 0)
            );
            assert_eq!(shifted.subsecond_nanos(), fraction);
            assert!(!shifted.is_leap_second());
            assert!(shifted.try_into_time().is_ok());
        }
    }
}
