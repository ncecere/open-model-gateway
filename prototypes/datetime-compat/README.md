# Date/time compatibility foundation prototype

Ref #674. This standalone workspace prepares bounded interfaces and independent
contract fixtures for a future gateway conversion. Production Cargo manifests,
consumers and the root lockfile are unchanged and still depend on Chrono. This
component does **not** resolve #674 or authorize a permanent SDK fork.

## Interfaces

| Boundary | API and responsibility |
| --- | --- |
| UTC observation | `UtcInstant`: nanoseconds, signed Unix seconds/microseconds, checked duration addition, UTC normalization, serde string |
| Calendar date | `CalendarDate`: proleptic Gregorian dates including year zero, signed expanded-year strings, checked day and clamped calendar-month shifts |
| Native interop | `TryFrom<time::OffsetDateTime>` / `TryFrom<UtcInstant>`, checked `TryFrom<time::Date>` / infallible `From<CalendarDate>` after validation |
| Timestamp input | `http::parse_rfc3339` and `http::parse_serde_timestamp` implement different existing grammars |
| Date input | Separate serde date, length-ten date filter, and financial-report date parsers |
| Date periods | Separate logs/usage, storage, financial-report and inclusive subject-export functions; caller supplies today's UTC date |
| Timestamp output | `format::utc_json`, `scim_millis`, `syslog_millis`, `otlp_unix_nanos` |
| Cursor timestamp | `cursor::parse_micros` preserves old integer acceptance; `parse_canonical_micros` additionally requires canonical decimal spelling for holds |

Constructors constrain the date carrier to years `-262143..=262142`. Cursor
timestamps additionally constrain signed Unix microseconds to
`-210866803200000000..=8210266876799999999`, the old PostgreSQL/carrier
intersection. Native Time's larger bounds do not expand either interface.

UTC JSON emits `Z` and exactly 0/3/6/9 fractional digits, including signed
expanded years. Both timestamp input paths truncate fractional digits after
nine without rounding (including the existing 19-digit, 40-byte import fixture).
The RFC3339 path retains the old `T`/`t`/space separators, padded fields and colon
offsets; the serde path additionally accepts unpadded fields, signed expanded
years, component whitespace, `UTC`, and colonless offsets. Both preserve the
old U+2212 offset-minus acceptance. These are compatibility grammars, not claims
about every ISO 8601 spelling. Callers retain their own byte limits.

Date-only contracts remain distinct. Logs/usage use exclusive 1–93-day periods
ending no later than tomorrow; storage permits 1–366 days; subject export is
inclusive, permits 1–93 days and ends no later than today. Financial reports
add years `1..=9999`. The length-ten filter retains the old numeric parser's
padding/leading-component whitespace behavior; it does not introduce a new
strict regex. Each handler still owns defaults, authorization, HTTP statuses and
error text. In particular, this prototype does not change #409.

## Leap-second boundary and open incompatibilities

`UtcInstant` retains a whole-second UTC anchor plus a subsecond value. For
`:60`, the anchor is the preceding `:59` and the subsecond value is
`1000000000..2000000000`. The formatter and serde preserve the observation and
fraction, including after offset normalization. Acceptance does not consult a
leap-second table or impose a month-end condition, matching the former parser.
Derived ordering keeps regular `:59` before `:60` before the next minute.

Legacy integer conversions intentionally retain their former semantics:
`unix_seconds` returns the anchor second; microseconds and optional signed
nanoseconds add the full subsecond value and can overlap the next second.
Integer cursor conversion therefore cannot round-trip a leap observation, just
as before; cursor timestamps decoded from integers are ordinary instants.

**Open incompatibility:** native `time::OffsetDateTime` cannot represent that
observation. Conversion returns `Error::LeapSecond`, as does duration addition
on a retained leap observation (even a zero duration). No silent clamp or
normalization is performed. Production consumers that previously did arithmetic
or SQL/SDK conversion on accepted leap seconds still require an explicit,
tested resolution. This error is not proof of production behavior compatibility.
Always feed original input into the compatibility parser: information already
lost by Time's native leap-second parser cannot be recovered here.

## Dependencies and source review

`time = "=0.3.55"` uses explicit `std`, `large-dates`, `serde`, `formatting` and
`parsing` features with default features disabled. Serde and the JSON test
dependency also have exact versions (matching the production lock's versions).
The committed ordinary Cargo.lock pins registry versions and integrity checksums
for the complete standalone graph. Neither the lock nor the resolved all-feature,
all-target graph contains Chrono. This is not a cargo-deny result for the gateway.

Before building, reviewed Time's [v0.3.55 changelog](https://github.com/time-rs/time/blob/v0.3.55/CHANGELOG.md)
and published source: Cargo features, serde representation, RFC3339 parser,
`Date`/month arithmetic, signed durations, timestamp construction and checked
offset conversion. The 0.3.55 fixes cover deserialization bounds and overflow,
and checked timestamp/offset conversions returning errors instead of panicking.
0.3.54 introduced `SignedDuration` / `PlainDateTime`; 0.3.48 records bounded
subsecond parsing for format descriptions. The explicit parser here scans the
caller-bounded string once and truncates without parsing unbounded integers.
Time's native RFC3339 formatter rejects expanded years and minimizes fractions;
its parser accepts a broader separator and clamps accepted leap seconds.
Literal tests demonstrate these differences.

The previous carrier's [parser](https://github.com/chronotope/chrono/blob/v0.4.45/src/format/parse.rs),
[offset scanner](https://github.com/chronotope/chrono/blob/v0.4.45/src/format/scan.rs),
[formatter](https://github.com/chronotope/chrono/blob/v0.4.45/src/format/formatting.rs),
date bounds and integer conversion source informed the fixtures. No old carrier
is compiled or used as the test oracle. Build scripts of the active Serde,
serde_core, serde_json and zmij dependencies were read before execution; they
perform compiler/target detection and local generated configuration, without
network access. Other active helper crates declare no build scripts.

## Verification

Run from the repository root with the repository-pinned Rust 1.99.1:

```sh
export CARGO_BUILD_JOBS=2
cargo test --locked --manifest-path prototypes/datetime-compat/Cargo.toml
cargo fmt --manifest-path prototypes/datetime-compat/Cargo.toml -- --check
cargo clippy --locked --manifest-path prototypes/datetime-compat/Cargo.toml --all-targets --all-features -- -D warnings
cargo tree --locked --manifest-path prototypes/datetime-compat/Cargo.toml --all-features --target all
```

Tests contain literal accept/refuse and expected-value fixtures for precision,
UTC normalization, pre-epoch flooring, expanded years, old cursor bounds, leap
observations, native conversion refusal, date limits, month clamping and each
exporter. Reproducible generated properties exercise 20,000 cursor values,
20,000 nanosecond values, a 400-year Gregorian cycle across year zero and short
UTF-8 parser mutations. Those properties supplement the literal expectations.
They are not an exhaustive parser equivalence proof.

## Remaining integration work

- No SQLx implementations, PostgreSQL wire codecs, SDK sources or production
  consumers are included. Native interop still needs the separately owned SQLx
  and OIDC/OAuth proofs, including the leap boundary above.
- Existing PostgreSQL-generated JSON stays a separate serialization path.
  No scalar/optional/array/database precision or session-timezone claim is made.
- Calendar primitives do not replace accounting day/week/month windows, the
  lifetime sentinel, retention, expiry/deadline decisions or application clocks.
  Jiff's timezone/DST scheduler remains outside this conversion.
- UUID/session cursor syntax, tie ordering, actual endpoint defaults/statuses,
  syslog framing, SCIM envelopes and OTLP bodies remain consumer-owned and need
  running-gateway tests. Nanosecond-to-microsecond conversion remains lossy by
  the existing contract; native conversion alone does not prove SQL rounding.
- No gateway, database, browser, Spark, supply-chain suite or cargo-deny run is
  claimed. The parent owns complete-graph remediation, two independent security
  reviews (including adversarial review), real-path acceptance and integration.

This is a reviewable foundation with fixtures for the covered interfaces. It is
not a claim that every production date/time behavior is already wire compatible.
