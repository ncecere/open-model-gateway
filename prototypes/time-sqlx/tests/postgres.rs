//! Opt-in, read-only proof against an explicitly selected disposable PostgreSQL 17.6.
use sqlx::{
    Connection, FromRow, PgConnection, Row,
    postgres::{PgConnectOptions, PgSslMode},
};
use std::{
    env,
    panic::{AssertUnwindSafe, catch_unwind},
};
use time::{Date, Duration, Month, OffsetDateTime, PrimitiveDateTime, UtcOffset, macros::datetime};
use time_sqlx_proof::{legacy_cursor_time, pg_timestamp};

async fn database() -> PgConnection {
    let required = |name| {
        env::var(name).expect("set the TIME_SQLX_PG_* variables for a fresh disposable database")
    };
    // Only a local forward is accepted; no ambient DATABASE_URL or existing gateway DB.
    let options = PgConnectOptions::new()
        .host("127.0.0.1")
        .port(required("TIME_SQLX_PG_PORT").parse().expect("valid port"))
        .username(&required("TIME_SQLX_PG_USERNAME"))
        .password(&required("TIME_SQLX_PG_PASSWORD"))
        .database(&required("TIME_SQLX_PG_DBNAME"))
        .ssl_mode(PgSslMode::Disable);
    let mut db = PgConnection::connect_with(&options)
        .await
        .expect("connect to selected disposable database");
    let version: String = sqlx::query_scalar("SHOW server_version_num")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, "170006", "this proof targets PostgreSQL 17.6");
    sqlx::query("SET TIME ZONE 'UTC'")
        .execute(&mut db)
        .await
        .unwrap();
    db
}

#[derive(Debug, FromRow, PartialEq)]
struct Scalars {
    instant: OffsetDateTime,
    day: Date,
    wall: PrimitiveDateTime,
    optional_instant: Option<OffsetDateTime>,
    optional_day: Option<Date>,
    optional_wall: Option<PrimitiveDateTime>,
}

#[tokio::test]
async fn scalar_literal_decode_and_all_row_access_paths() {
    let mut db = database().await;
    let query = "SELECT TIMESTAMPTZ '1969-12-31 23:59:59.999999+00' AS instant,
        DATE '2000-02-29' AS day, TIMESTAMP '1999-12-31 23:59:59.999999' AS wall,
        NULL::timestamptz AS optional_instant, NULL::date AS optional_day,
        NULL::timestamp AS optional_wall";
    let expected = Scalars {
        instant: datetime!(1969-12-31 23:59:59.999999 UTC),
        day: datetime!(2000-02-29 0:00).date(),
        wall: datetime!(1999-12-31 23:59:59.999999),
        optional_instant: None,
        optional_day: None,
        optional_wall: None,
    };
    assert_eq!(
        sqlx::query_as::<_, Scalars>(query)
            .fetch_one(&mut db)
            .await
            .unwrap(),
        expected
    );
    let tuple: (
        OffsetDateTime,
        Date,
        PrimitiveDateTime,
        Option<OffsetDateTime>,
        Option<Date>,
        Option<PrimitiveDateTime>,
    ) = sqlx::query_as(query).fetch_one(&mut db).await.unwrap();
    assert_eq!(
        tuple,
        (
            expected.instant,
            expected.day,
            expected.wall,
            None,
            None,
            None
        )
    );
    let row = sqlx::query(query).fetch_one(&mut db).await.unwrap();
    assert_eq!(
        row.try_get::<OffsetDateTime, _>("instant").unwrap(),
        expected.instant
    );
    assert_eq!(row.try_get::<Date, _>(1).unwrap(), expected.day);
    assert_eq!(
        row.try_get::<PrimitiveDateTime, _>("wall").unwrap(),
        expected.wall
    );
    assert_eq!(row.try_get::<Option<OffsetDateTime>, _>(3).unwrap(), None);
    assert!(row.try_get::<OffsetDateTime, _>(3).is_err());
    assert!(row.try_get::<Date, _>("instant").is_err());
    let nonnull: (
        Option<OffsetDateTime>,
        Option<Date>,
        Option<PrimitiveDateTime>,
    ) = sqlx::query_as("SELECT $1::timestamptz, $2::date, $3::timestamp")
        .bind(Some(expected.instant))
        .bind(Some(expected.day))
        .bind(Some(expected.wall))
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(
        nonnull,
        (
            Some(expected.instant),
            Some(expected.day),
            Some(expected.wall)
        )
    );
    let nulls: (bool, bool, bool) =
        sqlx::query_as("SELECT $1::timestamptz IS NULL, $2::date IS NULL, $3::timestamp IS NULL")
            .bind(None::<OffsetDateTime>)
            .bind(None::<Date>)
            .bind(None::<PrimitiveDateTime>)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(nulls, (true, true, true));
}

#[tokio::test]
async fn binds_match_independent_sql_literals_and_server_wire() {
    let mut db = database().await;
    // PostgreSQL's send functions inspect the value received by the server, not SQLx's decoder.
    for (value, sql, hex) in [
        (
            datetime!(2000-01-01 0:00),
            "2000-01-01 00:00:00",
            "0000000000000000",
        ),
        (
            datetime!(1999-12-31 23:59:59.999999),
            "1999-12-31 23:59:59.999999",
            "ffffffffffffffff",
        ),
        (
            datetime!(2000-01-01 0:00:00.000001),
            "2000-01-01 00:00:00.000001",
            "0000000000000001",
        ),
        (
            datetime!(1969-12-31 23:59:59.999999),
            "1969-12-31 23:59:59.999999",
            "fffca2fec4c81fff",
        ),
    ] {
        let row: (bool, bool, String, String) = sqlx::query_as(
            "SELECT $1::timestamp = $3::text::timestamp,
             $2::timestamptz = ($3::text || '+00')::timestamptz,
             encode(timestamp_send($1), 'hex'), encode(timestamptz_send($2), 'hex')",
        )
        .bind(value)
        .bind(value.assume_utc())
        .bind(sql)
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert_eq!(row, (true, true, hex.into(), hex.into()));
    }
    let date_wire: (bool, String) =
        sqlx::query_as("SELECT $1::date = DATE '1999-12-31', encode(date_send($1), 'hex')")
            .bind(datetime!(1999-12-31 0:00).date())
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(date_wire, (true, "ffffffff".into()));
}

#[tokio::test]
async fn offsets_normalize_to_utc_even_in_non_utc_session() {
    let mut db = database().await;
    sqlx::query("SET TIME ZONE 'Asia/Kathmandu'")
        .execute(&mut db)
        .await
        .unwrap();
    for value in [
        datetime!(2000-01-01 5:45 +5:45),
        datetime!(1999-12-31 16:00 -8),
        datetime!(2000-01-01 0:00 UTC),
    ] {
        let (decoded, equal, wire): (OffsetDateTime, bool, String) = sqlx::query_as(
            "SELECT $1::timestamptz, $1 = TIMESTAMPTZ '2000-01-01 00:00:00+00', encode(timestamptz_send($1), 'hex')")
            .bind(value).fetch_one(&mut db).await.unwrap();
        assert_eq!(decoded, datetime!(2000-01-01 0:00 UTC));
        assert_eq!(decoded.offset(), UtcOffset::UTC);
        assert!(equal);
        assert_eq!(wire, "0000000000000000");
    }
    let decoded: OffsetDateTime = sqlx::query_scalar("SELECT TIMESTAMPTZ '1999-12-31 16:00:00-08'")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(decoded, datetime!(2000-01-01 0:00 UTC));
    assert_eq!(decoded.offset(), UtcOffset::UTC);
}

#[derive(Debug, FromRow, PartialEq)]
struct Arrays {
    instants: Vec<Option<OffsetDateTime>>,
    days: Vec<Option<Date>>,
    walls: Vec<Option<PrimitiveDateTime>>,
}

#[tokio::test]
async fn arrays_preserve_null_elements_empty_and_null_arrays() {
    let mut db = database().await;
    let expected = Arrays {
        instants: vec![
            Some(datetime!(1969-12-31 23:59:59.999999 UTC)),
            None,
            Some(datetime!(2000-01-01 0:00 UTC)),
        ],
        days: vec![
            Some(datetime!(2000-02-29 0:00).date()),
            None,
            Some(datetime!(1999-12-31 0:00).date()),
        ],
        walls: vec![
            Some(datetime!(1999-12-31 23:59:59.999999)),
            None,
            Some(datetime!(2000-01-01 0:00)),
        ],
    };
    let sql = "SELECT ARRAY[TIMESTAMPTZ '1969-12-31 23:59:59.999999+00', NULL, TIMESTAMPTZ '2000-01-01 00:00+00'] AS instants,
        ARRAY[DATE '2000-02-29', NULL, DATE '1999-12-31'] AS days,
        ARRAY[TIMESTAMP '1999-12-31 23:59:59.999999', NULL, TIMESTAMP '2000-01-01 00:00'] AS walls";
    assert_eq!(
        sqlx::query_as::<_, Arrays>(sql)
            .fetch_one(&mut db)
            .await
            .unwrap(),
        expected
    );
    let bound: Arrays = sqlx::query_as(
        "SELECT $1::timestamptz[] AS instants, $2::date[] AS days, $3::timestamp[] AS walls",
    )
    .bind(&expected.instants)
    .bind(&expected.days)
    .bind(&expected.walls)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(bound, expected);
    let matches: bool = sqlx::query_scalar(&format!("SELECT $1::timestamptz[] IS NOT DISTINCT FROM instants AND $2::date[] IS NOT DISTINCT FROM days AND $3::timestamp[] IS NOT DISTINCT FROM walls FROM ({sql}) AS literals"))
        .bind(&expected.instants).bind(&expected.days).bind(&expected.walls).fetch_one(&mut db).await.unwrap();
    assert!(matches);
    let row = sqlx::query("SELECT ARRAY[]::timestamptz[], ARRAY[]::date[], ARRAY[]::timestamp[], NULL::timestamptz[], NULL::date[], NULL::timestamp[]")
        .fetch_one(&mut db).await.unwrap();
    assert!(row.try_get::<Vec<OffsetDateTime>, _>(0).unwrap().is_empty());
    assert!(row.try_get::<Vec<Date>, _>(1).unwrap().is_empty());
    assert!(
        row.try_get::<Vec<PrimitiveDateTime>, _>(2)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        row.try_get::<Option<Vec<OffsetDateTime>>, _>(3).unwrap(),
        None
    );
    assert_eq!(row.try_get::<Option<Vec<Date>>, _>(4).unwrap(), None);
    assert_eq!(
        row.try_get::<Option<Vec<PrimitiveDateTime>>, _>(5).unwrap(),
        None
    );
    let encoded_nulls: (bool, bool, bool) = sqlx::query_as(
        "SELECT $1::timestamptz[] IS NULL, $2::date[] IS NULL, $3::timestamp[] IS NULL",
    )
    .bind(None::<Vec<Option<OffsetDateTime>>>)
    .bind(None::<Vec<Option<Date>>>)
    .bind(None::<Vec<Option<PrimitiveDateTime>>>)
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(encoded_nulls, (true, true, true));
    let encoded_empty: (bool, bool, bool) = sqlx::query_as(
        "SELECT $1::timestamptz[] = ARRAY[]::timestamptz[],
            $2::date[] = ARRAY[]::date[], $3::timestamp[] = ARRAY[]::timestamp[]",
    )
    .bind(Vec::<Option<OffsetDateTime>>::new())
    .bind(Vec::<Option<Date>>::new())
    .bind(Vec::<Option<PrimitiveDateTime>>::new())
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(encoded_empty, (true, true, true));
}

#[tokio::test]
async fn submicrosecond_precision_matches_epoch_relative_integer_truncation() {
    let mut db = database().await;
    for (nanos, expected_micros) in [
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
        let input = datetime!(2000-01-01 0:00) + Duration::nanoseconds(nanos);
        let (wall, instant, correct): (PrimitiveDateTime, OffsetDateTime, bool) = sqlx::query_as(
            "SELECT $1::timestamp, $2::timestamptz,
            $1::timestamp = CASE $3::bigint
                WHEN -1 THEN TIMESTAMP '1999-12-31 23:59:59.999999'
                WHEN 0 THEN TIMESTAMP '2000-01-01 00:00:00'
                WHEN 1 THEN TIMESTAMP '2000-01-01 00:00:00.000001' END",
        )
        .bind(input)
        .bind(input.assume_utc())
        .bind(expected_micros)
        .fetch_one(&mut db)
        .await
        .unwrap();
        let expected = datetime!(2000-01-01 0:00) + Duration::microseconds(expected_micros);
        assert!(correct, "{nanos}");
        assert_eq!(wall, expected);
        assert_eq!(instant, expected.assume_utc());
    }
    for (input, literal) in [
        (
            datetime!(1969-12-31 23:59:59.999999999),
            "1970-01-01 00:00:00",
        ),
        (
            datetime!(1970-01-01 0:00:00.000000001),
            "1970-01-01 00:00:00.000001",
        ),
    ] {
        let correct: bool = sqlx::query_scalar("SELECT $1::timestamp = $2::text::timestamp")
            .bind(input)
            .bind(literal)
            .fetch_one(&mut db)
            .await
            .unwrap();
        assert!(correct);
    }
    // Text input is rounded by PostgreSQL; this differs from SQLx binary truncation.
    let rounded: PrimitiveDateTime =
        sqlx::query_scalar("SELECT TIMESTAMP '1999-12-31 23:59:59.999998499'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(rounded, datetime!(1999-12-31 23:59:59.999998));
}

#[tokio::test]
async fn calendar_and_finite_timestamp_boundaries() {
    let mut db = database().await;
    for (wall, literal) in [
        (datetime!(-4713-11-24 0:00), "4714-11-24 00:00:00 BC"),
        (datetime!(0000-01-01 0:00), "0001-01-01 00:00:00 BC"),
        (
            datetime!(-0001-12-31 23:59:59.999999),
            "0002-12-31 23:59:59.999999 BC",
        ),
        (datetime!(0001-01-01 0:00), "0001-01-01 00:00:00"),
        (datetime!(1900-03-01 0:00), "1900-03-01 00:00:00"),
        (
            datetime!(2000-02-29 23:59:59.999999),
            "2000-02-29 23:59:59.999999",
        ),
        (
            datetime!(9999-12-31 23:59:59.999999),
            "9999-12-31 23:59:59.999999",
        ),
        (datetime!(+10000-01-01 0:00), "10000-01-01 00:00:00"),
        (
            datetime!(+262142-12-31 23:59:59.999999),
            "262142-12-31 23:59:59.999999",
        ),
        (
            datetime!(+294276-12-31 23:59:59.999999),
            "294276-12-31 23:59:59.999999",
        ),
    ] {
        pg_timestamp(wall).unwrap();
        let (decoded, instant, day, equal): (PrimitiveDateTime, OffsetDateTime, Date, bool) = sqlx::query_as(
            "SELECT $1::timestamp, $2::timestamptz, $3::date, $1::timestamp = $4::text::timestamp")
            .bind(wall).bind(wall.assume_utc()).bind(wall.date()).bind(literal)
            .fetch_one(&mut db).await.unwrap();
        assert!(equal, "{literal}");
        assert_eq!(decoded, wall);
        assert_eq!(instant, wall.assume_utc());
        assert_eq!(day, wall.date());
        let independent: PrimitiveDateTime = sqlx::query_scalar("SELECT $1::text::timestamp")
            .bind(literal)
            .fetch_one(&mut db)
            .await
            .unwrap();
        assert_eq!(independent, wall);
    }
    assert!(Date::from_calendar_date(1900, Month::February, 29).is_err());
    assert!(Date::from_calendar_date(2000, Month::February, 29).is_ok());
    assert!(legacy_cursor_time(8_210_266_876_800_000_000).is_err());
    // Native + PG can represent the first instant rejected by the old Chrono ceiling.
    let widened = datetime!(+262143-01-01 0:00 UTC);
    let equal: bool =
        sqlx::query_scalar("SELECT $1::timestamptz = TIMESTAMPTZ '262143-01-01 00:00:00+00'")
            .bind(widened)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert!(equal);
}

#[tokio::test]
async fn clean_server_and_encode_errors_do_not_poison_connection() {
    let mut db = database().await;
    for value in [
        datetime!(-4713-11-24 0:00) - Duration::microseconds(1),
        datetime!(+294277-01-01 0:00),
    ] {
        assert!(pg_timestamp(value).is_err());
        for timestamp in [false, true] {
            let result = if timestamp {
                sqlx::query("SELECT $1::timestamptz")
                    .bind(value.assume_utc())
                    .execute(&mut db)
                    .await
            } else {
                sqlx::query("SELECT $1::timestamp")
                    .bind(value)
                    .execute(&mut db)
                    .await
            };
            let error = result.unwrap_err();
            assert_eq!(
                error.as_database_error().unwrap().code().as_deref(),
                Some("22008")
            );
        }
    }
    for value in [PrimitiveDateTime::MIN, PrimitiveDateTime::MAX] {
        let error = sqlx::query("SELECT $1::timestamp")
            .bind(value)
            .execute(&mut db)
            .await
            .unwrap_err();
        assert!(matches!(error, sqlx::Error::Encode(_)));
    }
    let date_error = sqlx::query("SELECT $1::date")
        .bind(Date::MIN)
        .execute(&mut db)
        .await
        .unwrap_err();
    assert_eq!(
        date_error.as_database_error().unwrap().code().as_deref(),
        Some("22008")
    );
    let max_date: bool = sqlx::query_scalar("SELECT $1::date = DATE '999999-12-31'")
        .bind(Date::MAX)
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert!(max_date);
    let answer: i32 = sqlx::query_scalar("SELECT 42")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(answer, 42);
}

#[tokio::test]
async fn native_nonfinite_decode_hazards_and_sql_rejection() {
    let mut db = database().await;
    // Test-only containment characterizes an upstream hazard; never a runtime codec shim.
    for literal in ["infinity", "-infinity", "1000000-01-01"] {
        let row = sqlx::query("SELECT $1::text::date AS day")
            .bind(literal)
            .fetch_one(&mut db)
            .await
            .unwrap();
        assert!(
            catch_unwind(AssertUnwindSafe(|| row.try_get::<Date, _>("day"))).is_err(),
            "{literal}"
        );
    }
    for literal in ["infinity", "-infinity"] {
        let row =
            sqlx::query("SELECT $1::text::timestamp AS wall, $1::text::timestamptz AS instant")
                .bind(literal)
                .fetch_one(&mut db)
                .await
                .unwrap();
        // With large-dates, i64 infinity sentinels become bogus finite values.
        // Unlike DATE, this is silent success rather than a panic or Decode error.
        let wall = row.try_get::<PrimitiveDateTime, _>("wall").unwrap();
        let instant = row.try_get::<OffsetDateTime, _>("instant").unwrap();
        assert_eq!(instant, wall.assume_utc());
        assert!(pg_timestamp(wall).is_err());
    }
    // SQL refuses unsafe values before any native decoder sees them.
    // NULL is accompanied by a refusal flag; it must not disguise a rejection.
    for literal in ["infinity", "-infinity", "2000-01-01"] {
        let (wall, instant, refused): (Option<PrimitiveDateTime>, Option<OffsetDateTime>, bool) =
            sqlx::query_as(
                "SELECT CASE WHEN isfinite(t) THEN t END,
                 CASE WHEN isfinite(t) THEN t AT TIME ZONE 'UTC' END,
                 NOT isfinite(t)
                 FROM (SELECT $1::text::timestamp AS t) AS input",
            )
            .bind(literal)
            .fetch_one(&mut db)
            .await
            .unwrap();
        let accepted = literal == "2000-01-01";
        assert_eq!(refused, !accepted);
        assert_eq!(wall, accepted.then_some(datetime!(2000-01-01 0:00)));
        assert_eq!(instant, accepted.then_some(datetime!(2000-01-01 0:00 UTC)));
    }
    for (literal, expected) in [
        ("infinity", None),
        ("-infinity", None),
        ("1000000-01-01", None),
        ("999999-12-31", Some(Date::MAX)),
        ("4714-11-24 BC", Some(datetime!(-4713-11-24 0:00).date())),
    ] {
        let (safe, refused): (Option<Date>, bool) = sqlx::query_as(
            "WITH input AS (SELECT $1::text::date AS d), checked AS (
                SELECT d, isfinite(d) AND d BETWEEN DATE '4714-11-24 BC' AND DATE '999999-12-31' AS ok
                FROM input)
             SELECT CASE WHEN ok THEN d END, NOT ok FROM checked")
            .bind(literal).fetch_one(&mut db).await.unwrap();
        assert_eq!(safe, expected, "{literal}");
        assert_eq!(refused, expected.is_none(), "{literal}");
    }
}
