//! Reproducible generated properties supplement, rather than replace, literal fixtures.
use gateway_datetime_compat::{CalendarDate, UtcInstant, cursor, format, http};

#[test]
fn generated_cursor_roundtrips_and_orders_across_the_old_range() {
    let span = (cursor::MAX_MICROS as i128 - cursor::MIN_MICROS as i128 + 1) as u64;
    let mut seed = 0x674_dace_2026_u64;
    for _ in 0..20_000 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let micros = (i128::from(cursor::MIN_MICROS) + i128::from(seed % span)) as i64;
        let at = cursor::parse_canonical_micros(&micros.to_string()).unwrap();
        assert_eq!(at.unix_micros(), micros);
        assert_eq!(at.unix_seconds(), micros.div_euclid(1_000_000));
        assert_eq!(
            at.subsecond_nanos(),
            (micros.rem_euclid(1_000_000) * 1000) as u32
        );
        assert_eq!(
            http::parse_serde_timestamp(&format::utc_json(at)).unwrap(),
            at
        );
        if micros < cursor::MAX_MICROS {
            let next = cursor::parse_micros(&(micros + 1).to_string()).unwrap();
            assert!(at < next);
        }
    }
}

#[test]
fn generated_nanoseconds_survive_wire_and_native_boundaries() {
    let mut seed = 0x674_ba5e_2026_u64;
    for _ in 0..20_000 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let seconds = (seed % 16_000_000_000_000) as i64 - 8_000_000_000_000;
        let nanos = (seed.rotate_left(23) % 1_000_000_000) as u32;
        let at = UtcInstant::from_unix_parts(seconds, nanos).unwrap();
        let native = at.try_into_time().unwrap();
        assert_eq!(
            native.unix_timestamp_nanos(),
            i128::from(seconds) * 1_000_000_000 + i128::from(nanos)
        );
        assert_eq!(UtcInstant::try_from_time(native).unwrap(), at);
        assert_eq!(
            http::parse_serde_timestamp(&format::utc_json(at)).unwrap(),
            at
        );
    }
}

#[test]
fn calendar_properties_cover_a_gregorian_cycle_and_cross_year_zero() {
    for year in -200..200 {
        for month in 1..=12 {
            let first = CalendarDate::new(year, month, 1).unwrap();
            let next = first.checked_add_months(1).unwrap();
            assert_eq!(next.checked_add_months(-1).unwrap(), first);
            assert!((28..=31).contains(&next.days_since(first)));
            let last = next.checked_add_days(-1).unwrap();
            assert_eq!(last.month(), month);
            assert_eq!(last.year(), year);
            assert_eq!(last.checked_add_days(1).unwrap(), next);
            assert_eq!(first.midnight().unwrap().date(), first);
        }
    }
}

#[test]
fn short_arbitrary_utf8_inputs_never_panic() {
    // Includes multibyte offset signs/whitespace and deliberately broken prefixes.
    let alphabet = [
        "0", "1", "9", "+", "-", ":", "T", "Z", " ", "−", "\u{2002}", "💣",
    ];
    for a in alphabet {
        for b in alphabet {
            for c in alphabet {
                let input = format!("{a}{b}{c}");
                assert!(http::parse_rfc3339(&input).is_err());
                assert!(http::parse_serde_timestamp(&input).is_err());
                assert!(http::parse_serde_date(&input).is_err());
            }
        }
    }
    let valid = "2024-02-29T01:02:03.123456789Z";
    for index in 0..=valid.len() {
        for extra in alphabet {
            let input = format!("{}{extra}{}", &valid[..index], &valid[index..]);
            let _ = http::parse_rfc3339(&input);
            let _ = http::parse_serde_timestamp(&input);
        }
    }
}
