# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.12.0](v0.12.0.md). They become the next release's notes.

## New features

- **Reviewed onboarding templates for Teams and Projects** (#84, part 1: authoring and review). Platform Admins write templates (`/platform/workspace-templates`): the workspace kind, its catalogs (inherit, or a replacement list), catalog models to select, optional tighten-only local limits and member slots (one owner slot, plus admin and member slots). Versions are immutable; a Platform Admin other than the author reviews a version before it can be published; publishing retires the previous version, and any version can be retired. Content holds references only (no secrets, emails, platform roles or group mappings). Applying a template to create a workspace, and the dashboard, follow. Requires migration **0074** and the updated runtime grants.
