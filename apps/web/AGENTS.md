# Dashboard

React + Vite SPA with TypeScript, TanStack Router, TanStack Query and Bitop. The approved enterprise rebuild must match Grounded's current UI library, layout and Workspace/Admin patterns, not merely take inspiration from them. See `docs/enterprise-rebuild.md` at the repository root.

- Use relative same-origin API paths. Vite proxies them in development; Axum serves the built UI in production.
- `GATEWAY_INTERNAL_URL` is development-server configuration only, not browser configuration.
- Never put credentials or secrets in `VITE_*` variables; these are public build-time values.
- Rust owns all authorization and database access. Route guards in the browser are not security boundaries.
- Use TanStack Query for server state and TanStack Router for navigation.
- One enterprise installation has sibling Personal/Team/Project workspaces, no organization selector/routes. Platform Admin and Auditor can enter Admin; Auditor is read-only. Platform User stays in Workspace. Shared creation is Platform Admin only and belongs on dedicated Teams/Projects pages, not the switcher. Human-key issuance requires actual membership.
- Match Grounded's sidebar, mode switch, workspace selector, headers, breadcrumbs, tables, forms, dialogs, theme and responsive behavior. Use gateway-owned compositions; do not copy RAG-specific authorization or content features.
- Do not include foreign personal workspaces in shared directories or fetch their keys/request details. Platform reporting can show authorized personal usage/cost totals; ordinary members see only own activity. Display live defaults, replacement platform overrides and tighten-only local restrictions separately.
- Use vendored Bitop UI primitives and tokens when adding administrative screens; see `docs/bitop-ui.md`. Keep gateway logic outside the copied primitives. Vendored files are installed and refreshed with bitop-ui's CLI (`bitop add`/`update --registry <checkout>`), tracked by `apps/web/bitop-lock.json`; don't edit them here (`src/lib/bitop-lock.test.ts` fails). Needed changes go into bitop-ui first.
- The Bitop and Grounded reference checkouts are read-only; no local cross-repository runtime imports or unpublished registry URLs. Refresh shared primitives through Bitop's supported CLI, preserving hashes, license and provenance.
- Use Grounded's Bitop pill tabs and settings/page/dialog conventions instead of stacking long panels. Governance defaults to the current workspace; platform policy is separate and permission-gated.
- Budget and pricing forms accept US-dollar decimal strings, not raw micro-USD. Money still travels to the API as integer micro-USD strings; use exact BigInt conversion and preserve up to six decimal places, never floating-point rounding. Display at least two decimals without unnecessary trailing zeros. Missing cost/health remains unknown, not zero/healthy.
- Run `npm run typecheck:web`, `npm run test:web`, and `npm run build:web` from the repository root.
