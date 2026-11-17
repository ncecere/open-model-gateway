# Gateway UTC and calendar carriers

`gateway-datetime-compat` is a first-party workspace crate on maintained
[`time` 0.3.55](https://github.com/time-rs/time/blob/v0.3.55/CHANGELOG.md), with
`large-dates`. It preserves the gateway's former UTC/calendar contract without
retaining Chrono. `UtcInstant` owns a whole-second UTC anchor plus an observed
subsecond; a retained leap second has a subsecond in `[1e9,2e9)`. Native
`OffsetDateTime` conversion explicitly refuses those observations. Do not replace
carrier ordering with Unix integer ordering: leap encodings overlap the next
second. `CalendarDate` retains astronomical years -262143 through 262142.

HTTP RFC3339, relaxed serde, fixed-length date filters, financial filters and
cursor bounds are separate contracts. Keep callers' existing limits and status
mapping. UTC JSON uses exactly 0/3/6/9 fractional digits; SCIM/syslog use exactly
three. Calendar months clamp to the final valid day. Duration arithmetic counts
only a leap observation at the starting value; crossing it does not establish a
global leap-second calendar. Jiff's timezone/DST scheduler remains independent.

The optional SQLx adapter delegates wire parsing/encoding to SQLx 0.8.6's native
primitive and date codecs. Binary decode performs checked date arithmetic before
constructing the bounded carrier. Native SQLx DATE decode can panic on infinity
and dates outside Time's range, and native TIMESTAMP decode can turn infinity
into bogus finite values; neither decoder is used for binary date construction.
Option and array decoding use these same element implementations, preserving
NULL while returning an error for an invalid non-NULL value. Encoding uses signed
whole microseconds from PostgreSQL's 2000 epoch, truncating toward that epoch,
including the former leap-subsecond encoding. No schema or grant changes occur.

Source review anchors: [SQLx datetime codecs](https://github.com/launchbadge/sqlx/blob/v0.8.6/sqlx-postgres/src/types/time/datetime.rs),
[DATE codec](https://github.com/launchbadge/sqlx/blob/v0.8.6/sqlx-postgres/src/types/time/date.rs),
[former Chrono codec](https://github.com/launchbadge/sqlx/blob/v0.8.6/sqlx-postgres/src/types/chrono/datetime.rs),
and [PostgreSQL 17.6 timestamp bounds](https://github.com/postgres/postgres/blob/REL_17_6/src/include/datatype/timestamp.h).
Time .55 supplies checked offset/timestamp boundary fixes; .54 renamed primitives
with compatibility aliases. Tests pin independent wire literals, generated dates,
leap arithmetic, HTTP grammars and PostgreSQL scalar/optional/array refusals.

Run `cargo test -p gateway-datetime-compat`. Real database tests require
`--features postgres-tests --test postgres` and explicitly selected
`DATETIME_PG_PORT`, `DATETIME_PG_USERNAME`, `DATETIME_PG_PASSWORD`,
`DATETIME_PG_DBNAME` variables (or an explicit `DATETIME_PG_URL`, as selected by
CI) for a fresh PostgreSQL 17 database forwarded on
127.0.0.1. They never use an ambient installation DATABASE_URL. Use the Spark
skill and `kspark` for disposable databases; never plain kubectl.
