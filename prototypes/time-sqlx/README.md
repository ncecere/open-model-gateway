# Native SQLx/time codec proof (#674)

Standalone, unpublished workspace: SQLx **0.8.6**, native `time` feature, and
`time` **0.3.55** with `large-dates`. The gateway's manifests, lockfile and
`deny.toml` are unchanged. This is one feasibility component, not the gateway
migration or resolution of #674. It has no Chrono dependency, including its
lockfile, dev-dependencies and all-feature graph. SQLx's default features are
disabled; only PostgreSQL, Tokio, `time` and derive support are enabled. Optional
MySQL/SQLite packages in Cargo.lock are SQLx's resolver entries, not enabled drivers.

All PostgreSQL `Type`, `Encode`, `Decode` and array implementations come from
SQLx. There is no custom binary codec, SQLx upgrade, copied gateway, HTTP parser,
financial arithmetic, or floating-point epoch conversion here. `PrimitiveDateTime`
is the non-deprecated compatibility alias for time 0.3.54's `PlainDateTime`.

## What the tests establish

- Scalars: `OffsetDateTime`/`Date`/`PrimitiveDateTime`, NULL and non-NULL `Option`s,
  derived `FromRow`, tuples, `try_get`, NULL/type mismatch errors.
- Arrays: all three types with NULL elements; independent SQL literal equality;
  empty and NULL arrays in both directions.
- UTC normalization: equivalent positive/negative offsets bind to the same
  instant; decoded offsets are UTC even with a non-UTC server session.
- Exact microseconds: literal binary expectations and PostgreSQL's own
  `timestamp_send`, `timestamptz_send`, `date_send` results, including negative
  pre-1970/pre-2000 values. These do not rely solely on a client round trip.
- Submicroseconds: ±1/999/1000/1001/1501 nanoseconds around PostgreSQL's 2000
  epoch, plus Unix-epoch neighbors. Native binary encoding truncates the signed
  duration **toward the PostgreSQL epoch**, not toward the Unix epoch. For
  example, `1999-12-31 23:59:59.999998499` binds as `.999999`; PostgreSQL text
  input rounds it to `.999998`. The pinned Chrono SQLx encoder also converts a
  signed duration from 2000 to whole microseconds; a migration must preserve
  that convention instead of substituting Unix flooring or SQL text rounding.
- Calendar/ranges: BC versus astronomical year zero, leap-year rules,
  9999/10000, old Chrono ceiling, PostgreSQL finite timestamp endpoints,
  overflow encoding errors and server SQLSTATE `22008` refusals. No leap-second
  representation or HTTP acceptance equivalence is claimed.

## Bounds that integration must preserve

| Layer | Bounds / required behavior |
| --- | --- |
| Existing log/session/hold microsecond cursor acceptance | Integer Unix microseconds `-210866803200000000..=8210266876799999999`: astronomical `-4713-11-24 00:00:00` through `262142-12-31 23:59:59.999999` UTC. Keep signed i64 parsing and this intersection; reject the next microsecond. |
| PostgreSQL 17 finite TIMESTAMP/TIMESTAMPTZ | Same lower instant, upper `294276-12-31 23:59:59.999999` UTC. Maximum Unix microseconds `9224318015999999999` needs i128 and exceeds i64. |
| Native time with `large-dates` | Calendar years `-999999..=999999`. Broader than PostgreSQL timestamps and the existing gateway contract. Without this feature even the existing Chrono ceiling is unrepresentable. |
| PostgreSQL DATE | Astronomical `-4713-11-24..=5874897-12-31`, plus infinities. Finite dates after year 999999 exceed native time. |

`legacy_cursor_time` demonstrates the existing narrow cursor guard using checked
integer conversion. It is **not** a replacement cursor parser. Preserve all
existing endpoint-specific calendar, signed-i64, syntax and range refusals;
reporting's 1..=9999-year restriction remains separate. Do not use the broader
`pg_timestamp` guard to widen HTTP inputs, address #409, or rewrite stored
accounting periods. `pg_instant` demonstrates checked UTC normalization before
server bounds checking, without silently reducing precision.

Native decoding is not a universal clean-error boundary:

- SQLx 0.8.6 binary DATE adds days to its epoch with unchecked range arithmetic.
  Infinity and a valid PostgreSQL `1000000-01-01` DATE **panic**, rather than
  returning `sqlx::Error::Decode`.
- With `large-dates`, binary TIMESTAMP/TIMESTAMPTZ infinity sentinels decode
  **successfully to bogus finite values** outside PostgreSQL's finite range.
- Timestamp encoding rejects wire i64 overflow, but encodable finite values
  outside PostgreSQL's range are rejected by the server. DATE encoding can
  succeed even when the server cannot store the date.

The database tests contain DATE panics only to characterize the upstream defect;
`catch_unwind` is not a proposed runtime fix. The tests also exercise SQL `CASE`
projections that exclude nonfinite timestamps and dates outside time's range
**before decoding**, returning a separate refusal flag alongside NULL. A NULL
must not disguise a refusal as missing data. Real integration must enforce or
establish equivalent invariants for scalar columns, array elements and SQL
expressions before native decoding; helpers that validate after `try_get` cannot
prevent a DATE panic. A clean suite here does not establish those gateway
invariants. Text/simple-protocol decoding, real auth/API paths, TLS, runtime
privileges and existing-installation data validation are outside this proof.

## Run

Use the repository's Rust 1.99.1 with rustfmt/Clippy, and at most two Cargo jobs.
From this directory:

```sh
export CARGO_BUILD_JOBS=2
cargo test --locked
cargo fmt -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo metadata --locked --format-version 1
cargo metadata --locked --format-version 1 --no-default-features
cargo metadata --locked --format-version 1 --all-features
```

Inspect metadata (including dev-dependencies) and Cargo.lock for absence of a
package named `chrono`. Do not enable the gateway's Chrono feature through a
shared workspace while evaluating this component.

Real database tests are opt-in via `postgres-tests`; when enabled, missing
configuration fails instead of silently skipping. Supply secret environment
variables `TIME_SQLX_PG_PORT`, `TIME_SQLX_PG_USERNAME`, `TIME_SQLX_PG_PASSWORD`
and `TIME_SQLX_PG_DBNAME` for a **fresh disposable PostgreSQL 17.6** behind a
loopback forward. The host is fixed to `127.0.0.1`; these read-only tests create no schema or rows.
A loopback host alone cannot prove disposability: the operator must select a
fresh test database. Never point them at an existing installation.

```sh
cargo test --locked --all-features -- --test-threads=1
cargo deny --locked --config ../../deny.toml check
```

Before creating a throwaway database, read the `spark-k3s` skill. Use `kspark`,
only your credential base and a fresh namespace, wait for its two isolation
policies, pin the PostgreSQL image, and clean up resources afterwards. Never
use plain kubectl/helm or an existing database. Credentials belong in ignored
private files or environment, never terminal output or this package.

## Pinned source review

- [SQLx 0.8.6 datetime codec](https://github.com/launchbadge/sqlx/blob/v0.8.6/sqlx-postgres/src/types/time/datetime.rs),
  [date codec](https://github.com/launchbadge/sqlx/blob/v0.8.6/sqlx-postgres/src/types/time/date.rs),
  [Chrono encoder comparison](https://github.com/launchbadge/sqlx/blob/v0.8.6/sqlx-postgres/src/types/chrono/datetime.rs):
  reviewed type mapping, array support, UTC normalization, integer precision,
  binary overflow and decode addition. No codec source is copied here.
- [time 0.3.55 changelog](https://github.com/time-rs/time/blob/v0.3.55/CHANGELOG.md):
  reviewed checked offset/timestamp boundary fixes in .55, compatibility aliases
  in .54, subsecond macro precision fix in .52, and large-date formatting changes
  in .48. Native constants and checked conversion behavior are tested directly.
- [PostgreSQL 17.6 bounds](https://github.com/postgres/postgres/blob/REL_17_6/src/include/datatype/timestamp.h):
  finite limits, Julian epochs and infinity sentinels underpin the independent
  literal cases. PostgreSQL BC years have no year zero; time uses astronomical
  year numbering.

Parent integration still owns graph remediation for the OIDC/OAuth SDKs, HTTP
compatibility, running-gateway proof, full CI and two independent security
reviews (one adversarial). Root Chrono/advisory blocking remains unresolved by
this component. No advisory policy or ignore is changed.
