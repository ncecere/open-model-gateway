# Maintained datetime dependencies

The gateway replaces Chrono's complete production graph (#674) with `time`
**0.3.55** (`large-dates`) through the first-party
[`gateway-datetime-compat`](../crates/datetime/README.md) crate. Jiff remains the
separate timezone/DST scheduling implementation. Timestamp precision, parser
acceptance/refusal, calendar/cursor bounds, microsecond SQL encoding and expiry
comparisons remain gateway contracts, rather than adopting the broader native
Time range or parser grammar. This does not implement #409's HTTP range changes.

SQLx **0.8.6** already provides maintained Time codecs. Its Chrono feature is
removed. The shared carriers delegate primitive wire parsing and encoding while
checking date construction, avoiding native DATE overflow panics and TIMESTAMP
infinity normalization. No migration or runtime grant changes are required.
Native conversion cannot preserve a retained leap observation and explicitly
returns an error; the gateway and SDKs keep the owned representation instead.

OpenID Connect **4.1.0** and OAuth2 **5.0.0** still require Chrono upstream. The
reviewed published source ports live at `vendor/openidconnect` and
`vendor/oauth2`, with the exact original names/versions, archive provenance,
original manifests and licenses. Local maintenance belongs to the gateway
maintainers until an upstream release removes the dependency. See each package's
`MAINTENANCE.md` for changes and the update/review procedure. They are excluded
from workspace membership and registered under root
`workspace.metadata.vendored`; neither an advisory ignore nor a source-policy
exception is introduced. The normal cargo-deny check and canonical
`.github/scripts/vendored-advisories.mjs` scan both remain required, including
upstream-base scanner coverage. Root Cargo.lock controls production versions;
SDK-only development helpers use standalone resolution when their excluded
package tests run and do not enter the gateway binary.

The source ports preserve the original crypto/JWT/JWKS, signature algorithms and
transport implementations. Public dates/clocks now use the bounded owned carrier.
NumericDate retains integer/f64 rounding and integer anchor serialization,
including accepted leap observations and tiny negative fractions. Optional
RFC3339 accepts only the former separators and preserves its refusal vectors;
expiry at `exp == now` is still refused. `serde_with` core/macros and typed list
visitors avoid pulling Chrono through inactive optional adapters while preserving
discovery/JWKS entry and container behavior.

Update checks: review upstream changelogs and full source deltas, refresh source
provenance/registration/lock together, run complete graph inventories, both normal
and upstream-base advisory gates, SDK default/all-feature/all-target/doc tests,
carrier literal/property/PostgreSQL tests and gateway HTTP/OIDC contracts. SQL
and authentication changes require two independent reviews, one adversarial,
followed by the coordinator's diff review and normal protected PR checks.
Do not claim a native Time conversion establishes leap compatibility.

Primary review anchors: [Time changelog](https://github.com/time-rs/time/blob/v0.3.55/CHANGELOG.md),
[SQLx 0.8.6 codecs](https://github.com/launchbadge/sqlx/tree/v0.8.6/sqlx-postgres/src/types/time),
[OpenID Connect source](https://github.com/ramosbugs/openidconnect-rs/tree/e20ddbb01ca68356f2979bc125f5613ab45b4e7e),
[OAuth2 source](https://github.com/ramosbugs/oauth2-rs/tree/f3424b4b2190c83c6d031fdc71eed2351d49e0df), and
[Chrono maintenance advisory](https://github.com/RustSec/advisory-db/blob/main/crates/chrono/RUSTSEC-2026-0543.md).
