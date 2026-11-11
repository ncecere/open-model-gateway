# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.15.1](v0.15.1.md). They become the next release's notes.

## Contracts and documentation

- **SCIM: what to do before the first push** (#390). [SCIM](../scim.md#before-the-first-push-bootstrap-admin-and-group-mapping) now gives the order (a bootstrap Platform Admin who can sign in, then the SSO group mapping, then the first sync), the expected denial for a pushed user with no grant, and observations from the live Authentik run. The SCIM, staging, roadmap and README statements now record that live SCIM acceptance passed once, on v0.9.1 (#60).
