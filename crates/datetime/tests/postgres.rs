//! Real PostgreSQL literals against a fresh explicitly selected throwaway database.
#![cfg(feature = "postgres-tests")]
use gateway_datetime_compat::{CalendarDate, UtcInstant, http};
use sqlx::{
    Connection, PgConnection, Row,
    postgres::{PgConnectOptions, PgSslMode},
};
async fn database() -> PgConnection {
    let required =
        |name| std::env::var(name).expect("set DATETIME_PG_* for a fresh PostgreSQL database");
    let options = PgConnectOptions::new()
        .host("127.0.0.1")
        .port(required("DATETIME_PG_PORT").parse().unwrap())
        .username(&required("DATETIME_PG_USERNAME"))
        .password(&required("DATETIME_PG_PASSWORD"))
        .database(&required("DATETIME_PG_DBNAME"))
        .ssl_mode(PgSslMode::Disable);
    let mut db = PgConnection::connect_with(&options).await.unwrap();
    let version: String = sqlx::query_scalar("SHOW server_version_num")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert!(
        (170000..180000).contains(&version.parse::<u32>().unwrap()),
        "PostgreSQL 17 required"
    );
    db
}
#[tokio::test]
async fn scalar_optional_array_expression_refusals_preserve_null() {
    let mut db = database().await;
    for literal in [
        "'infinity'",
        "'-infinity'",
        "'1000000-01-01'",
        "'262143-01-01'",
        "'262144-01-01'",
    ] {
        let row = sqlx::query(&format!(
            "SELECT {literal}::date AS day, ARRAY[NULL::date,{literal}::date] AS days"
        ))
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert!(row.try_get::<CalendarDate, _>("day").is_err(), "{literal}");
        assert!(
            row.try_get::<Option<CalendarDate>, _>("day").is_err(),
            "{literal}"
        );
        assert!(
            row.try_get::<Vec<Option<CalendarDate>>, _>("days").is_err(),
            "{literal}"
        );
    }
    for literal in ["'infinity'", "'-infinity'", "'262143-01-01 00:00:00+00'"] {
        let row = sqlx::query(&format!("SELECT {literal}::timestamptz AS at, ARRAY[NULL::timestamptz,{literal}::timestamptz] AS times"))
            .fetch_one(&mut db).await.unwrap();
        assert!(row.try_get::<UtcInstant, _>("at").is_err(), "{literal}");
        assert!(
            row.try_get::<Option<UtcInstant>, _>("at").is_err(),
            "{literal}"
        );
        assert!(
            row.try_get::<Vec<Option<UtcInstant>>, _>("times").is_err(),
            "{literal}"
        );
    }
    type NullableValues = (
        Option<CalendarDate>,
        Option<UtcInstant>,
        Vec<Option<CalendarDate>>,
        Vec<Option<UtcInstant>>,
    );
    let (date, instant, dates, instants): NullableValues = sqlx::query_as(
        "SELECT NULL::date, NULL::timestamptz, ARRAY[NULL::date], ARRAY[NULL::timestamptz]",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(
        (date, instant, dates, instants),
        (None, None, vec![None], vec![None])
    );
}
#[tokio::test]
async fn literal_calendar_and_timestamp_arrays_and_wire_precision() {
    let mut db = database().await;
    for (input, expected) in [
        (
            "1999-12-31T23:59:59.999998499Z",
            "1999-12-31T23:59:59.999999Z",
        ),
        (
            "2000-01-01T00:00:00.000001501Z",
            "2000-01-01T00:00:00.000001Z",
        ),
        ("1969-12-31T23:59:59.999999999Z", "1970-01-01T00:00:00Z"),
        (
            "2016-12-31T23:59:60.123456789Z",
            "2017-01-01T00:00:00.123456Z",
        ),
        (
            "1999-12-31T23:59:60.123456789Z",
            "2000-01-01T00:00:00.123456Z",
        ),
        (
            "1969-12-31T23:59:60.123456789Z",
            "1970-01-01T00:00:00.123457Z",
        ),
    ] {
        let at = http::parse_rfc3339(input).unwrap();
        let expected = http::parse_rfc3339(expected).unwrap();
        let (actual, identical, wire): (UtcInstant, bool, String) = sqlx::query_as(
            "SELECT $1::timestamptz, $1::timestamptz = $2::timestamptz, encode(timestamptz_send($1),'hex')")
            .bind(at).bind(expected).fetch_one(&mut db).await.unwrap();
        assert_eq!(actual, expected, "{input}");
        assert!(identical, "{input}, {wire}");
    }
    for (year, text) in [
        (0, "0001-02-29 BC"),
        (10000, "10000-02-29"),
        (262142, "262142-02-28"),
    ] {
        let day = CalendarDate::new(year, 2, if year == 262142 { 28 } else { 29 }).unwrap();
        let row = sqlx::query(&format!(
            "SELECT $1::date = DATE '{text}' AS equal, ARRAY[$1::date,NULL] AS days"
        ))
        .bind(day)
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert!(row.get::<bool, _>("equal"));
        assert_eq!(
            row.get::<Vec<Option<CalendarDate>>, _>("days"),
            vec![Some(day), None]
        );
    }
    let wire: String = sqlx::query_scalar("SELECT encode(timestamptz_send($1),'hex')")
        .bind(http::parse_rfc3339("1999-12-31T23:59:59.999998499Z").unwrap())
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(wire, "ffffffffffffffff");
}

#[tokio::test]
async fn scalar_bounds_wrong_types_and_empty_arrays() {
    let mut db = database().await;
    let row = sqlx::query(
        "SELECT TIMESTAMPTZ '4714-11-24 00:00:00 BC' AS first,
        TIMESTAMPTZ '262142-12-31 23:59:59.999999+00' AS last,
        ARRAY[]::date[] AS dates, NULL::timestamptz[] AS times, 1 AS wrong",
    )
    .fetch_one(&mut db)
    .await
    .unwrap();
    assert_eq!(
        row.get::<UtcInstant, _>("first").unix_micros(),
        -210_866_803_200_000_000
    );
    assert_eq!(
        row.get::<UtcInstant, _>("last").unix_micros(),
        8_210_266_876_799_999_999
    );
    assert_eq!(
        row.get::<Vec<CalendarDate>, _>("dates"),
        Vec::<CalendarDate>::new()
    );
    assert_eq!(row.get::<Option<Vec<UtcInstant>>, _>("times"), None);
    assert!(row.try_get::<CalendarDate, _>("wrong").is_err());
    assert!(row.try_get::<UtcInstant, _>("wrong").is_err());
}

#[tokio::test]
async fn text_protocol_whole_seconds_offsets_nullable_and_arrays() {
    let mut db = database().await;
    use sqlx::Executor;
    for (zone, literal, rendered, seconds, nanos) in [
        (
            "UTC",
            "2024-02-29 00:00:00+00",
            "2024-02-29 00:00:00+00",
            1_709_164_800,
            0,
        ),
        (
            "UTC",
            "1969-12-31 23:59:59+00",
            "1969-12-31 23:59:59+00",
            -1,
            0,
        ),
        (
            "UTC",
            "1969-12-31 23:59:59.999999+00",
            "1969-12-31 23:59:59.999999+00",
            -1,
            999_999_000,
        ),
        (
            "Asia/Kathmandu",
            "2000-01-01 00:00:00+00",
            "2000-01-01 05:45:00+05:45",
            946_684_800,
            0,
        ),
        (
            "America/Los_Angeles",
            "2000-01-01 00:00:00+00",
            "1999-12-31 16:00:00-08",
            946_684_800,
            0,
        ),
    ] {
        db.execute(format!("SET TIME ZONE '{zone}'").as_str())
            .await
            .unwrap();
        let sql = format!(
            "SELECT value AS at, value::text AS rendered, ARRAY[value, NULL::timestamptz] AS times,
             NULL::timestamptz AS absent FROM (SELECT TIMESTAMPTZ '{literal}' AS value) AS input"
        );
        let row = db.fetch_one(sqlx::raw_sql(&sql)).await.unwrap();
        assert!(matches!(
            row.try_get_raw("at").unwrap().format(),
            sqlx::postgres::PgValueFormat::Text
        ));
        assert_eq!(row.try_get::<String, _>("rendered").unwrap(), rendered);
        let expected = UtcInstant::from_unix_parts(seconds, nanos).unwrap();
        assert_eq!(row.try_get::<UtcInstant, _>("at").unwrap(), expected);
        assert_eq!(
            row.try_get::<Option<UtcInstant>, _>("at").unwrap(),
            Some(expected)
        );
        assert_eq!(
            row.try_get::<Vec<Option<UtcInstant>>, _>("times").unwrap(),
            vec![Some(expected), None]
        );
        assert_eq!(
            row.try_get::<Option<UtcInstant>, _>("absent").unwrap(),
            None
        );
        assert!(row.try_get::<UtcInstant, _>("absent").is_err());
    }
}

#[tokio::test]
async fn text_protocol_safe_refusal_and_ordinary_values() {
    let mut db = database().await;
    use sqlx::Executor;
    for kind in ["date", "timestamptz"] {
        for sentinel in ["infinity", "-infinity"] {
            let row = db
                .fetch_one(sqlx::raw_sql(&format!(
                    "SELECT '{sentinel}'::{kind} AS value"
                )))
                .await
                .unwrap();
            if kind == "date" {
                assert!(row.try_get::<CalendarDate, _>(0).is_err());
            } else {
                assert!(row.try_get::<UtcInstant, _>(0).is_err());
            }
        }
    }
    let row = db
        .fetch_one(sqlx::raw_sql(
            "SELECT DATE '2024-02-29', TIMESTAMPTZ '1969-12-31 23:59:59.999999+00'",
        ))
        .await
        .unwrap();
    assert_eq!(
        row.try_get::<CalendarDate, _>(0).unwrap(),
        CalendarDate::new(2024, 2, 29).unwrap()
    );
    assert_eq!(row.try_get::<UtcInstant, _>(1).unwrap().unix_micros(), -1);
}
