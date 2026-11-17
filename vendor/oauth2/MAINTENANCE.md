# Repository-local OAuth2 SDK patches

This is the published **oauth2 5.0.0** source, patched for this gateway, with
`publish = false` and its original upstream name/version. It is not a published
replacement or a generally source-compatible upstream release. Original MIT and
Apache-2.0 licenses, authors, `Cargo.toml.orig`, `.cargo_vcs_info.json`, README and
upgrade guide remain.

Source: [published archive](https://static.crates.io/crates/oauth2/oauth2-5.0.0.crate),
SHA-256 `51e219e79014df21a225b1860a479e2dcd7cbd9130f4defd4bd0e191ea31d67d`,
upstream commit `f3424b4b2190c83c6d031fdc71eed2351d49e0df`. Manifest metadata
retains that identity. Upstream CI and its lockfile are omitted. Production uses
the root lockfile; standalone SDK checks generate an ignored local lockfile whose
registry versions should match the integration lockfile.

Introspection date getters/setters and device-flow clocks now use first-party
`gateway_datetime_compat::UtcInstant` (re-exported as `oauth2::UtcInstant`) from
`../../crates/datetime`. Introspection keeps integer-only input/output, null and
missing behavior, the UTC range `-8334601228800..=8210266876799`, and refusal of
fractional/string dates. The carrier retains leap observations for custom clocks
and integer serialization, without forcing them through native Time.

Sleep, expires-in and timeout parameters retain `std::time::Duration`. Device
polling converts to the shared signed duration and adds it with checked arithmetic;
unrepresentable deadlines refuse before HTTP. It retains strict `now > deadline`,
including at a retained leap observation, in both blocking and async loops. The
default clock uses `UtcInstant::now()`; out-of-range custom clock values cannot be
constructed through its public API. Transports, request construction, token
handling and other OAuth behavior are unchanged, apart from mechanical lint fixes.
The effective MSRV is the shared crate's pinned repository toolchain, 1.99.1.

Repository maintainers own these patches. Follow the sibling
[OpenID Connect maintenance and update procedure](../openidconnect/MAINTENANCE.md#patch-ownership-and-upgrades),
including upstream archive/license verification, dependency source/changelog
review, two security reviews, root canonical vendor registrations and unchanged
normal plus upstream-base advisory checks. This documents responsibility and the
update procedure; it offers no maintenance guarantee or policy exception.

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
CARGO_BUILD_JOBS=2 cargo test --manifest-path vendor/oauth2/Cargo.toml --all-targets
CARGO_BUILD_JOBS=2 cargo test --manifest-path vendor/oauth2/Cargo.toml --all-targets --all-features
CARGO_BUILD_JOBS=2 cargo test --manifest-path vendor/oauth2/Cargo.toml --all-features --doc
CARGO_BUILD_JOBS=2 cargo clippy --manifest-path vendor/oauth2/Cargo.toml --all-targets --all-features -- -D warnings
cargo fmt --manifest-path vendor/oauth2/Cargo.toml --all -- --check
```

The loopback introspection test covers acceptance and malformed-date refusal
through the original blocking adapter. No paid/live provider calls are needed.
All-feature native transport checks require host OpenSSL/curl/pkg-config headers.
Cross-target/WASM execution is not proved by host tests.
