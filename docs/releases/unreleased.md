# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.9.1](v0.9.1.md). They become the next release's notes.

## Fixed

- `grants verify` uses its own closed synthetic history for global alert fixtures, so existing open personal aged-hold or last-Admin SCIM incidents do not collide with its rollback probes (#382). Existing incidents and unknown holds are not changed; no migrations or runtime-grant changes are needed.

## Verification

- Security-matrix failure diagnostics capture bounded, request-owned native database outcomes and SQLSTATEs across detached accounting tasks, without SQL text, error messages, identifiers or credentials. This is test-only instrumentation; the historical `accounting_unavailable` cause in #370 remains unresolved. Production deadlines and cancellation behavior are unchanged.
