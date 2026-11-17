# Repository-local OpenID Connect SDK patches

This is the published **openidconnect 4.1.0** source, patched for this gateway.
It is not an upstream release or a general drop-in replacement. The package keeps
its upstream name/version for the repository's existing upstream-base advisory
workflow, and `publish = false` prevents accidental publication. Original license,
authors, `Cargo.toml.orig`, `.cargo_vcs_info.json`, README and upgrade guide remain.

Source: [published archive](https://static.crates.io/crates/openidconnect/openidconnect-4.1.0.crate),
SHA-256 `df0d098a59353679624d406825940139e2b2d554a52b64c9fb1b1bc42a11fe5c`,
upstream commit `e20ddbb01ca68356f2979bc125f5613ab45b4e7e`. The manifest retains
that archive identity. Upstream CI and its lockfile are omitted; production uses
the repository root lockfile. Standalone library checks generate an ignored local
lockfile; keep its registry versions aligned with the integration lockfile.
The SDK manifests are isolated workspaces for scoped maintenance commands. The
root must also explicitly exclude and register both canonical vendor packages
under the existing upstream-base advisory workflow.

## Local API and behavior

Date/clock APIs use the first-party `gateway_datetime_compat::UtcInstant`
(re-exported as `openidconnect::UtcInstant`) from `../../crates/datetime`.
The carrier preserves the former bounded UTC range and leap observations;
conversion to native Time can refuse a leap without erasing it from claims,
ordering or wire serialization. Verifier clock, issue-time/auth-time callbacks,
ID-token dates, user-info `updated_at` and registration dates all use that carrier.
Expiry remains `now >= exp`. The default clock uses `UtcInstant::now()`; custom
clocks must return a value constructed by its bounded public API.

NumericDate input retains the published JSON Number/f64 calculation, including
fractional rounding artifacts and tiny negatives (`-1e-20`, `-1e-100`) represented
as seconds -1 plus 1000000000 nanoseconds. Output remains integer seconds,
including negative fractional/leap values. The optional RFC3339 feature uses the
shared strict parser, preserving T/t/space separators, offsets, truncated excess
fraction digits and retained leap seconds. It never uses the carrier's relaxed
Serde parser for protocol claims. Wire bounds remain
`-8334601228800..=8210266876799`.

The JWT/JWS, cryptographic implementations, key selection, issuer/audience/nonce
checks and transport adapters are the published implementations. Original tests
for all nine asymmetric algorithms remain. SDK-local Serde sequence/option
visitors replace only the discovery/JWKS `VecSkipError` adapters. They preserve
invalid-entry skipping, duplicate members, ordering and container/syntax refusal;
`serde_with` keeps only its macros feature so inactive Chrono is absent too.
There is no copied Serde fork.

Development diagnostics use pinned color-backtrace 0.7.4 and env_logger 0.11.11;
unused async-std was removed. These remove stale development dependencies while
retaining example targets and upstream test ignore attributes. Mechanical lint
fixes update lifetimes, formatting references and the certification test helper.
The effective MSRV is the shared crate's pinned repository toolchain, 1.99.1.

## Patch ownership and upgrades

Repository maintainers own these local patches and their security review; this is
an update procedure, not a promise of upstream maintenance. Review upstream
releases and advisories when preparing dependency updates and each gateway
release. Do not infer patched-source security from an unchanged upstream version.

For an update, verify the published archive checksum and licenses, read upstream
release notes and source/dependency changes (especially crypto, network and build
scripts), and rebase the small local patch onto that published base. Preserve the
original archive metadata and update root vendor registrations/lockfile through
the integration owner. Never add an advisory ignore to make adoption pass.
Review every changed security boundary, with independent and adversarial reviews
under AGENTS.md. Re-run SDK tests below, root graph/lock inventories (default,
no-default, all-feature, dev/build/all-target edges), unchanged cargo-deny, and
`.github/scripts/vendored-advisories.mjs`. The root integration must separately
prove gateway authentication/authorization and running-gateway refusal paths.

Interactive examples display authorization URLs and device user codes only on
an attached controlling terminal (`/dev/tty` on Unix, `CONOUT$` on Windows), never
through stdout or stderr. They refuse missing/nonterminal output and control
characters in a prompt. Device user codes remain visible for the verification
step required by RFC 8628; device codes and tokens are never displayed. Callback
examples compare returned CSRF state before exchanging the code. Normal example
diagnostics contain status messages rather than code/state, token, claim or raw
provider-error dumps. These example-only changes do not change SDK transports or
verifiers. Unix regression tests exercise real PTYs and redirected output using
Python 3; Windows terminal execution remains unproved by those tests.

Scoped commands from repository root, Cargo jobs capped at two:

```sh
CARGO_BUILD_JOBS=2 cargo test --manifest-path vendor/openidconnect/Cargo.toml --all-targets
CARGO_BUILD_JOBS=2 cargo test --manifest-path vendor/openidconnect/Cargo.toml --all-targets --all-features
CARGO_BUILD_JOBS=2 cargo test --manifest-path vendor/openidconnect/Cargo.toml --all-features --doc
CARGO_BUILD_JOBS=2 cargo clippy --manifest-path vendor/openidconnect/Cargo.toml --all-targets --all-features -- -D warnings
cargo fmt --manifest-path vendor/openidconnect/Cargo.toml --all -- --check
```

All-feature native transport checks require host OpenSSL/curl/pkg-config headers.
Loopback tests use synthetic data and disable redirects and ambient proxies; no
provider credentials or paid requests are needed. Issuer-dependent RP tests keep
their upstream ignores. Cross-target/WASM execution is not proved by host tests.
