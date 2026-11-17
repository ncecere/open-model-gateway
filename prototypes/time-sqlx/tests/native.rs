use sqlx::{Encode, Postgres, Type, postgres::PgArgumentBuffer};
use time::{Date, Duration, OffsetDateTime, PrimitiveDateTime, macros::datetime};

fn encoded<'a, T: Encode<'a, Postgres>>(value: T) -> Result<Vec<u8>, sqlx::error::BoxDynError> {
    let mut buffer = PgArgumentBuffer::default();
    let _null = value.encode_by_ref(&mut buffer)?;
    Ok(buffer.to_vec())
}

#[test]
fn native_types_and_literal_binary_expectations() {
    assert_eq!(
        <OffsetDateTime as Type<Postgres>>::type_info().to_string(),
        "TIMESTAMPTZ"
    );
    assert_eq!(
        <PrimitiveDateTime as Type<Postgres>>::type_info().to_string(),
        "TIMESTAMP"
    );
    assert_eq!(<Date as Type<Postgres>>::type_info().to_string(), "DATE");
    for (value, micros) in [
        (datetime!(2000-01-01 0:00), 0_i64),
        (datetime!(1999-12-31 23:59:59.999999), -1),
        (datetime!(2000-01-01 0:00:00.000001), 1),
        (datetime!(1970-01-01 0:00), -946_684_800_000_000),
        (datetime!(1969-12-31 23:59:59.999999), -946_684_800_000_001),
        (datetime!(2000-02-29 0:00), 5_097_600_000_000),
    ] {
        assert_eq!(encoded(value).unwrap(), micros.to_be_bytes());
        assert_eq!(encoded(value.assume_utc()).unwrap(), micros.to_be_bytes());
    }
    for (value, days) in [
        (datetime!(2000-01-01 0:00).date(), 0_i32),
        (datetime!(1999-12-31 0:00).date(), -1),
        (datetime!(1970-01-01 0:00).date(), -10_957),
        (datetime!(2000-02-29 0:00).date(), 59),
    ] {
        assert_eq!(encoded(value).unwrap(), days.to_be_bytes());
    }
    assert!(encoded(None::<OffsetDateTime>).unwrap().is_empty());
}

#[test]
fn native_submicroseconds_truncate_toward_postgres_epoch() {
    let epoch = datetime!(2000-01-01 0:00);
    for (nanos, micros) in [
        (-1501, -1_i64),
        (-1001, -1),
        (-1000, -1),
        (-999, 0),
        (-1, 0),
        (0, 0),
        (1, 0),
        (999, 0),
        (1000, 1),
        (1001, 1),
        (1501, 1),
    ] {
        let value = epoch + Duration::nanoseconds(nanos);
        assert_eq!(encoded(value).unwrap(), micros.to_be_bytes(), "{nanos}");
        assert_eq!(encoded(value.assume_utc()).unwrap(), micros.to_be_bytes());
    }
}

#[test]
fn native_encode_reports_timestamp_i64_overflow() {
    for value in [PrimitiveDateTime::MIN, PrimitiveDateTime::MAX] {
        assert!(encoded(value).is_err());
        assert!(encoded(value.assume_utc()).is_err());
    }
    // DATE's i32 wire range is broader than time's large-dates range.
    assert!(encoded(Date::MIN).is_ok());
    assert!(encoded(Date::MAX).is_ok());
}
