# Dashboard

React + Vite SPA with TypeScript, TanStack Router, TanStack Query, and Tailwind CSS.

- Use relative same-origin API paths. Vite proxies them in development; Axum serves the built UI in production.
- `GATEWAY_INTERNAL_URL` is development-server configuration only, not browser configuration.
- Never put credentials or secrets in `VITE_*` variables; these are public build-time values.
- Rust owns all authorization and database access. Route guards in the browser are not security boundaries.
- Use TanStack Query for server state and TanStack Router for navigation.
- Platform admins configure global infrastructure without selecting an organization. Organization admins see assigned models and delegated management, never provider/deployment/routing setup. Admin navigation follows Platform → Organizations → Teams/Projects → Members. The context selector switches existing resources only; never put organization/team creation in it. Creation belongs on the Organizations/Teams/Members pages.
- Teams and projects are sibling shared workspaces internally; do not include private personal workspaces in shared directories or platform user metadata. Display inherited platform/parent policy ceilings separately from editable local restrictions.
- Use vendored Bitop UI primitives and tokens when adding administrative screens; see `docs/bitop-ui.md`. Keep gateway logic outside the copied primitives. Vendored files are installed and refreshed with bitop-ui's CLI (`bitop add`/`update --registry <checkout>`), tracked by `apps/web/bitop-lock.json`; don't edit them here (`src/lib/bitop-lock.test.ts` fails). Needed changes go into bitop-ui first.
- The Bitop and open-rag-system reference checkouts are read-only; no local cross-repository runtime imports or unpublished registry URLs.
- Use Bitop pill tabs (`TabsList variant="pills"`) for settings with multiple scopes or detail views instead of stacking long panels. Governance defaults to the current workspace; organization tabs remain permission-gated.
- Budget and pricing forms accept US-dollar decimal strings, not raw micro-USD. Money still travels to the API as integer micro-USD strings; use exact BigInt conversion and preserve up to six decimal places, never floating-point rounding. Display at least two decimals without unnecessary trailing zeros. Missing cost/health remains unknown, not zero/healthy.
- Run `npm run typecheck:web`, `npm run test:web`, and `npm run build:web` from the repository root.
