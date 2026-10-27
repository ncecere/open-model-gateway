# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.9.1](v0.9.1.md). They become the next release's notes.

## Fixed

- `grants verify` uses its own closed synthetic history for global alert fixtures, so existing open personal aged-hold or last-Admin SCIM incidents do not collide with its rollback probes (#382). Existing incidents and unknown holds are not changed; no migrations or runtime-grant changes are needed.
