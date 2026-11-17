# Auth SDK maintained-time port prototype — Ref #674

This standalone workspace ports the published OpenID Connect and OAuth2 SDKs to
`time = 0.3.55` (`large-dates`, `std`, `parsing`). It is not wired into the gateway.
The repository root manifest, lockfile, application code, advisory policy and CI
are unchanged. Both SDK packages are `publish = false`; their upstream names and
versions identify the source base, not a publishable compatible replacement.

## Source and review boundary

| Package | Published archive (crates.io SHA-256 verified before extraction) | Upstream commit |
| --- | --- | --- |
| openidconnect 4.1.0 | [archive](https://static.crates.io/crates/openidconnect/openidconnect-4.1.0.crate), `df0d098a59353679624d406825940139e2b2d554a52b64c9fb1b1bc42a11fe5c` | `e20ddbb01ca68356f2979bc125f5613ab45b4e7e` |
| oauth2 5.0.0 | [archive](https://static.crates.io/crates/oauth2/oauth2-5.0.0.crate), `51e219e79014df21a225b1860a479e2dcd7cbd9130f4defd4bd0e191ea31d67d` | `f3424b4b2190c83c6d031fdc71eed2351d49e0df` |

Original licenses, copyright/author declarations, `Cargo.toml.orig`,
`.cargo_vcs_info.json`, READMEs and upgrade guides are retained. Upstream CI
configuration and lockfiles are omitted; this workspace has one Cargo lockfile.
Active package manifests also record the upstream archive identity. No build
scripts were added. The source import commit is separate so reviewers can inspect
the port as a small diff against the published source.

Read alongside the packaged upgrade guides:
[OIDC 4.1.0 release/changelog](https://github.com/ramosbugs/openidconnect-rs/releases/tag/4.1.0),
[OAuth2 5.0.0 release/changelog](https://github.com/ramosbugs/oauth2-rs/releases/tag/5.0.0),
and [time 0.3.55 changelog](https://github.com/time-rs/time/blob/v0.3.55/CHANGELOG.md).
OIDC 4.1.0's crypto dependency changes are already part of the published base;
this port retains the published crypto dependency declarations and original
signature/JWT/key-selection implementations. Transport implementations are
unchanged. OAuth request encoding
has only two redundant formatting references removed for current Clippy;
lifetimes, doc indentation, byte-string spelling and a test `is_empty()` fix
are mechanical lint fixes. The RP certification `PanicIfFail` helper is moved
unchanged into its sole consumer (code-flow tests), avoiding a dead-code lint in
the dynamic-registration target without adding a lint allowance.

## Serde and development dependency cleanup

`serde_with = 3.24.0` is pinned with only `macros` enabled, without `std` or
`alloc`. Its timestamp `DeserializeAs`/`SerializeAs` traits, `serde_as` for
required/optional timestamps and `skip_serializing_none` remain in use. The sole
used adapter requiring its allocation feature was `VecSkipError`, on the JWKS
`keys` list and ten discovery algorithm lists (one required, nine optional).

Those lists now use two small SDK-local deserialization functions built from
public Serde sequence/option visitors and a derived untagged entry enum. Serde
buffers each entry before attempting the original item type; failed items are
consumed and skipped, while syntax/container errors propagate. No conversion via
`serde_json::Value` occurs, so duplicate members still reach the original item
parser. Plain derived serialization retains the existing list/optional-list wire
shape. The existing JWK parser, filtering, signature verification and transports
are unchanged; only the list Serde attributes route through the new functions.
No third-party Serde source was copied or forked.

Two new SDK tests were first run against the previous `VecSkipError` setup and
then against the replacement. They cover every affected discovery field,
missing/null/empty optional lists, order and duplicate elements, duplicate object
members, unknown/ill-typed items, malformed containers/JSON, empty usable-key
sets, and original signature refusal for incomplete keys. Existing timestamp,
cryptographic, discovery and HTTP tests are retained.

Development-only updates:

* `color-backtrace = 0.7.4` replaces atty-based terminal detection with the standard
  library. Default backtrace diagnostics remain enabled. Its locked `backtrace`
  moves from 0.3.71 to 0.3.77, removing the old adler path while retaining native
  symbolication (addr2line 0.27.1, gimli 0.34.0, object 0.40.0).
* `env_logger = 0.11.11` preserves its default logging/color/filter features and
  existing `init()` call sites; timestamp formatting uses Jiff instead of
  humantime. Jiff remains separate from the SDK's protocol timestamp type.
* The unused `async-std` dev declaration is removed. No SDK source, example or
  test used it; the existing Tokio runtime and asynchronous tests remain enabled.
  No test, example target or ignore attribute was removed or added by this cleanup.

Direct updates are exact manifest pins; every registry package is version/checksum
pinned in this workspace's Cargo.lock. All 16 added/upgraded archives were checked
against those checksums before compilation. None declares or contains a build
script. Source/manifests and the relevant packaged/tagged changelogs were inspected:
[color-backtrace](https://github.com/athre0z/color-backtrace/blob/v0.7.4/CHANGELOG.md),
[env_logger](https://github.com/rust-cli/env_logger/blob/v0.11.11/CHANGELOG.md),
[backtrace](https://github.com/rust-lang/backtrace-rs/blob/backtrace-v0.3.77/CHANGELOG.md),
and their terminal/parser dependencies. The changed dependencies are development
diagnostics; no network/crypto dependency versions changed. The lockfile shrinks
from 342 to 300 packages. No ignored advisory or source-policy exception was added.

## API and wire contract

* OIDC ID-token claims (`exp`, `iat`, `auth_time`), standard/user-info `updated_at`,
  registration dates, and verifier clock/iat/auth-time callbacks now use
  `time::OffsetDateTime` instead of `chrono::DateTime<Utc>`.
* OAuth introspection `exp`/`iat`/`nbf` trait getters and setters, plus device-flow
  clock callbacks, now use `OffsetDateTime`. Sleep/timeout/expires-in APIs retain
  `std::time::Duration`; internal deadline conversion uses checked
  `time::Duration` conversion/addition. OAuth's stated MSRV rises to 1.88 for time.
* Explicit adapters preserve integer-second output and the published UTC wire
  range `-8334601228800..=8210266876799`, including subsecond values at either end.
  Serialization and custom clocks outside that range fail closed. Time's default
  Serde representation is never used. Fractional serialization floors, including
  negative fractions. Introspection still refuses non-integer dates.
* OIDC retains the published `serde_json::Number`/f64 fractional calculation,
  including its rounding artifacts: `1.999999999` yields 999999998 nanoseconds.
  Range checks precede float-to-integer conversion. This is timestamp compatibility,
  not a money representation or a new rounding scheme.
* OIDC expiry remains `now >= exp`; device polling remains `now > deadline`.
  OIDC has no built-in leeway setting: custom clock functions continue to implement
  caller-selected leeway. The signed-token regression checks one nanosecond before,
  exactly at, and one nanosecond after expiry with a 30-second custom-clock shift.
* Issuer/audience/nonce checks, the original signing and verification functions,
  all asymmetric algorithms (RS256/384/512, PS256/384/512, ES256/384, EdDSA), and
  JWKS selection/refusal behavior are retained. Upstream tests cover these paths.
  Gateway `azp`, verified-email and signed-group checks are untouched and untested here.

**NumericDate compatibility limit:** upstream's f64 calculation can round tiny
negative fractions (`-1e-20`, `-1e-100`) to seconds `-1` plus **1000000000**
nanoseconds, which Chrono accepts as a leap-second representation. This was
confirmed using the baseline Chrono build. OffsetDateTime cannot represent that
state; the port fails closed instead of normalizing it to epoch zero. Tests pin
that refusal. Thus the port does **not** preserve every upstream fractional input;
exact all-input compatibility requires a representation/API decision before adoption.

**Optional RFC3339 compatibility limit:** ordinary offset/fractional timestamps
are parsed and normalized to UTC. Leap seconds are explicitly refused: Chrono can
represent them, while time otherwise normalizes them to an earlier instant.
This narrower opt-in feature is not drop-in compatible and requires an adoption
scope decision. The gateway does not enable this feature today. Cross-target
compilation, full RFC3339 acceptance equivalence and WASM clocks are not proven.

## Reproduce from repository root

Use Rust 1.99.1 (the implementation host), Cargo jobs capped at two. Native
all-feature builds require `pkg-config` and OpenSSL/curl/zlib development headers.
No live providers, credentials, Kubernetes or paid calls are involved.

```sh
export CARGO_BUILD_JOBS=2
cargo fmt --manifest-path prototypes/oidc-time/Cargo.toml --all -- --check
cargo test --manifest-path prototypes/oidc-time/Cargo.toml --locked --workspace --all-targets
cargo test --manifest-path prototypes/oidc-time/Cargo.toml --locked --workspace --all-targets --all-features
cargo test --manifest-path prototypes/oidc-time/Cargo.toml --locked -p oauth2 --all-features --test time_port_http
cargo clippy --manifest-path prototypes/oidc-time/Cargo.toml --locked --workspace --all-targets --all-features -- -D warnings
cargo test --manifest-path prototypes/oidc-time/Cargo.toml --locked --workspace --all-features --doc
python3 prototypes/oidc-time/check-graph.py
cargo deny --manifest-path prototypes/oidc-time/Cargo.toml --all-features --config deny.toml check
```

Local results after cleanup: formatting and strict all-target/all-feature Clippy
passed. Default all-target tests passed (69 OAuth2 + 74 OIDC library tests);
all-feature all-target tests passed (69 + 77), including the loopback HTTP test
with seven positive/refusal responses. The 21 issuer-dependent certification tests
retain their upstream ignore attributes, unchanged. Full workspace documentation
tests passed (11 OAuth2 + 7 OIDC; four upstream ignores unchanged). Strict default,
no-default and all-feature inventories, including dev/build/all-target edges,
unfiltered metadata and Cargo.lock, contain no Chrono. Unchanged cargo-deny 0.20.2
passes advisories, bans, licenses and sources (duplicate-version warnings remain
allowed by the existing policy). No gateway suite was run.

At prior head `7fe5faf9`, the strict inventory failed on
inactive Chrono 0.4.45 via serde_with 3.24.0, and cargo-deny failed on adler
(RUSTSEC-2025-0056), async-std (RUSTSEC-2025-0052), atty (RUSTSEC-2021-0145 and
RUSTSEC-2024-0375), humantime (RUSTSEC-2026-0542), and pin-utils
(RUSTSEC-2026-0441). Those dependency failures are now resolved by this cleanup;
the representation incompatibilities above remain **OPEN and unchanged**.

Baseline contracts were run successfully against the unported SDKs before the
port; the graph assertion first failed on their direct Chrono dependency. New
contracts cover NumericDate ranges/fractions, integer serialization, actual claim
adapters, introspection dates, checked device deadlines, and original signed JWT
expiry verification. The loopback HTTP test uses the original blocking reqwest
adapter, with synthetic responses and exact request-body assertions; it proves
positive and malformed-date refusal paths. Upstream RP certification tests are
compiled by all-target Clippy but are never run (they require a separate issuer).

## Decisions and proofs still required before adoption

1. **OPEN, unchanged:** the NumericDate tiny-negative-fraction/leap-second
   representation gap and optional RFC3339 leap-second scope described above.
   No normalization, representation change or claim of exact equivalence is made.
2. Cargo-deny sees these workspace SDKs as **path packages**, so its passing
   advisory scan does not establish upstream-base coverage for them. Final
   adoption must use an approved maintained upstream release, or explicitly
   authorize fork ownership, update cadence, patch review and the repository's
   canonical vendored registration/coverage workflow. The current scanner expects
   root registrations and canonical `vendor/...` paths, not this standalone layout.
   No source-policy exception or permanent maintenance is authorized by this work.
3. Gateway integration still needs explicit timestamp API changes and dependency
   source decisions. Parent-owned fresh independent security reviews (including
   an adversarial review), gateway/PostgreSQL/browser/Spark end-to-end proofs and
   CI remain outstanding. Cross-target/WASM compilation is unproven. No claim is
   made that #674 is fixed; the gateway itself still uses Chrono.
