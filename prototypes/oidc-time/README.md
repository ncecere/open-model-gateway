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
this port retains its dependency declarations and original signature/JWT/JWKS
implementations. Transport implementations are unchanged. OAuth request encoding
has only two redundant formatting references removed for current Clippy;
lifetimes, doc indentation, byte-string spelling and a test `is_empty()` fix
are mechanical lint fixes. The RP certification `PanicIfFail` helper is moved
unchanged into its sole consumer (code-flow tests), avoiding a dead-code lint in
the dynamic-registration target without adding a lint allowance.

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
cargo test --manifest-path prototypes/oidc-time/Cargo.toml --locked --workspace --lib
cargo test --manifest-path prototypes/oidc-time/Cargo.toml --locked --workspace --lib --all-features
cargo test --manifest-path prototypes/oidc-time/Cargo.toml --locked -p oauth2 --all-features --test time_port_http
cargo clippy --manifest-path prototypes/oidc-time/Cargo.toml --locked --workspace --all-targets --all-features -- -D warnings
cargo test --manifest-path prototypes/oidc-time/Cargo.toml --locked -p openidconnect --all-features --doc -- src/lib.rs
python3 prototypes/oidc-time/check-graph.py
cargo deny --manifest-path prototypes/oidc-time/Cargo.toml --all-features --config deny.toml check
```

Local results: formatting and strict all-target/all-feature Clippy passed. Default
library tests passed (69 OAuth2 + 72 OIDC); all-feature library tests passed
(69 + 75). The loopback HTTP integration test passed all seven response cases.
OIDC library documentation examples compiled (five passed, two upstream ignored,
two outside the selected `src/lib.rs` filter). Strict metadata inventory and
cargo-deny failed for the reasons below. No gateway suite was run.

Baseline contracts were run successfully against the unported SDKs before the
port; the graph assertion first failed on their direct Chrono dependency. New
contracts cover NumericDate ranges/fractions, integer serialization, actual claim
adapters, introspection dates, checked device deadlines, and original signed JWT
expiry verification. The loopback HTTP test uses the original blocking reqwest
adapter, with synthetic responses and exact request-body assertions; it proves
positive and malformed-date refusal paths. Upstream RP certification tests are
compiled by all-target Clippy but are never run (they require a separate issuer).

## Findings that block adoption

1. `check-graph.py` **fails** the strict inventory requirement. Active default,
   no-default and all-feature graphs (including dev/build/all-target edges) have
   no Chrono, and no `serde_with` Chrono feature is active. Nevertheless Cargo's
   unfiltered metadata and lockfile retain **chrono 0.4.45** through
   **serde_with 3.24.0**'s inactive optional/weak-feature references. The checker
   reports both facts and exits nonzero; this is not a fully clean inventory.
   Removing that residual requires a reviewed Serde dependency strategy; no
   additional third-party fork or policy exception is introduced here.
2. Unchanged **cargo-deny 0.20.2** policy fails advisories on published upstream
   dev dependencies: `adler` (RUSTSEC-2025-0056 via color-backtrace/backtrace),
   `async-std` (RUSTSEC-2025-0052), `atty` (RUSTSEC-2021-0145 and
   RUSTSEC-2024-0375 via color-backtrace/env_logger), `humantime`
   (RUSTSEC-2026-0542 via env_logger), and `pin-utils` (RUSTSEC-2026-0441 via
   async-std). Bans, licenses and sources pass; no Chrono advisory is reported
   for the active graph. The existing root RSA exception is unchanged. This is
   not a passing supply-chain gate.
3. Cargo-deny sees these workspace SDKs as **path packages**, so its advisory
   scan does not establish upstream-base coverage for them. Final adoption must
   use an approved maintained upstream release, or explicitly authorize fork
   ownership, update cadence, patch review and the repository's canonical vendored
   registration/coverage workflow. The current scanner expects root registrations
   and canonical `vendor/...` paths, not this standalone prototype layout. No
   source-policy exception or permanent fork maintenance is authorized by this work.
4. Gateway integration still needs explicit timestamp API changes, dependency
   source decisions, resolution of the inactive Chrono inventory and dev-tool
   advisories, and the NumericDate/leap-second representation and optional RFC3339 scope decisions. Parent-owned independent
   security reviews (including an adversarial review), gateway/PostgreSQL/browser
   end-to-end proofs and CI remain outstanding. No claim is made that #674 is fixed;
   the gateway itself still uses Chrono.
