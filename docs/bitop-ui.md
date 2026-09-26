# Bitop UI integration

## Sources and ownership

This dashboard uses **copy-and-own source**, not a published Bitop npm package or network registry endpoint. The source is the local, read-only checkout:

- `/Users/nicholascecere/projects/typescript/bitop-ui/registry.json`
- Component sources under that checkout’s `registry/bitop/ui/` and `registry/bitop/lib/`.
- Registry SHA-256 at import: `6c84c3db27ec20b5b7abe26755ca156159fb5288de03900a69e580cbf08d9358`.

`apps/web/bitop-provenance.json` records selected items, recursive registry dependencies, external package dependencies, source paths, target paths, original SHA-256 hashes, and copied SHA-256 hashes. The registry homepage contains a placeholder; it is deliberately not used as a release or installation URL. No source checkout files were changed. The updated source supplies an MIT license, copyright © 2026 Nicholas Cecere. Its notice is copied verbatim to `apps/web/src/components/ui/LICENSE` and included in the provenance manifest; this resolves the earlier missing-upstream-license caveat. It does not assign a license to unrelated gateway code.

The layout design reference is the read-only file:

`/Users/nicholascecere/projects/golang/open-rag-system/web/src/components/layout.tsx`

SHA-256: `dc176a5e83de1556806cc6e82ad8d9c0903e965bacf59b8554c0ccfe3a358e04`.

The gateway adopts its compact collapsible sidebar, Workspace/Admin mode switch, grouped People/Models/Oversight navigation, workspace switcher, breadcrumbs, header command palette, and sidebar account menu. The full-width content footer is omitted. It does **not** copy RAG API clients, auth, query definitions, capability rules, teams, knowledge-base logic, or storage keys.

## Reproduce the copy

From the gateway repository root:

```sh
node scripts/vendor-bitop.mjs /Users/nicholascecere/projects/typescript/bitop-ui
npm install
npm run typecheck:web
npm run test:web
npm run build:web
```

The script accepts another local checkout path as its only argument and requires the source `LICENSE` before copying anything. It reads `registry.json`, resolves selected items plus all recursive `@bitop/*` dependencies, and copies their declared files to the registry targets:

- `@ui/*` → `apps/web/src/components/ui/*`
- `@lib/*` → `apps/web/src/lib/*`

These source import aliases are rewritten:

- `@/registry/bitop/ui/` → `@/components/ui/`
- `@/registry/bitop/lib/` → `@/lib/`

One recorded copy-and-own patch adds an optional `className` to the command-palette popup. This lets gateway composition CSS use the higher-contrast muted-text token for search group labels/hints without changing vendor CSS or global theme tokens. The script applies this exact checked patch reproducibly and records `optional-popup-className` with its copied hash. Everything else—including CSS modules, neutral-theme tokens, Base UI behavior, comments, and client directives—is preserved. `"use client"` is harmless in Vite; it does not introduce Next.js. The script refuses non-local registry dependencies and unsafe targets. It performs no network calls. Rerunning it overwrites vendored files and the manifest, so review the diff before accepting a refresh. Compare hashes with the committed manifest to detect upstream or local drift. Dependency installation is a separate lockfile-managed step.

Selected items: app-shell, breadcrumbs, table, card, button, page-header, stat-card, input, field, badge, empty-state, command-palette, tabs. Recursive items include core, theme-neutral, avatar, layout, menu, tooltip, spinner, and kbd. The refreshed manifest tracks 45 files: 44 component/support files plus the upstream license notice. Runtime package dependencies are `@base-ui/react`, `lucide-react`, and `@fontsource-variable/inter`, alongside the existing React stack.

## Integration boundaries

- `src/main.tsx` imports `components/ui/styles/bitop.css` once, before gateway composition CSS. Core imports structural tokens, the neutral theme, reset, and local Inter font assets.
- TypeScript and Vite both resolve `@/*` to `apps/web/src/*`.
- `src/pages/home.tsx` uses the real Bitop `AppShell`, Sidebar parts, `Brand`, `WorkspaceSwitcher`, `TopBar`, `Main`, menus, and breadcrumbs. The native skip target, keyboard controls, collapsed tooltips, and sidebar toggle come from Bitop. Collapse state is memory-only and survives page changes.
- `src/components/jump-search.tsx` composes Bitop's CommandPalette, CommandPaletteTrigger, and keyboard shortcut hook. `src/lib/search.ts` filters session-scoped destinations with existing UI permissions; searches never fetch secrets or activity. The palette's upstream `finalFocus` hook handles in-place focus restoration. For navigation that remounts the scope-keyed shell, the stable dashboard parent focuses the destination heading after the remount; dialog and transient-secret isolation is unchanged. Dismissal and refresh retain default focus restoration. The refreshed Table primitive adds its keyboard-scroll region only when ResizeObserver detects overflow, not during server rendering.
- `src/components/ui.tsx` preserves gateway-facing wrappers and its original action lifecycle while composing actual Bitop Table/Card/PageHeader/Badge/EmptyState components. Forms use Field/Input/NativeSelect/Textarea and Bitop submit/cancel buttons. Overview and Costs use StatCard. Upstream Button now cancels activation for disabled/loading non-native render targets (such as links), in addition to marking them `aria-disabled`. Existing CRUD button classes bridge to the same tokens; they are not a second sidebar implementation.
- Governance uses Bitop Tabs with `variant="pills"` for authorized scope selection and effective/local/inherited details. Inactive panels are unmounted; the workspace opens by default. Base UI supplies tab roles, selection, and arrow-key behavior.
- Gateway-specific CSS is in `src/styles.css`. Vendored component styles are not edited to mimic the reference layout.
- The gateway owns all queries, permission guards, CSRF/same-origin transport, and CRUD semantics. Rust remains the authorization boundary. No secrets enter browser storage or TanStack query/mutation caches; one-time tokens remain in transient dialog state.
- The CSV helper accepts only an encoded workspace identifier and bounded integers, uses same-origin credentials with redirects disabled and no cache, checks CSV content type, caps streamed response bytes, aborts on scope exit, and revokes transient Blob URLs after download.
- Amounts remain decimal integer micro-USD strings. Formatting/conversion uses BigInt, never floating-point currency rounding. Budget and price inputs use decimal US dollars (up to six places), with exact conversion to the API's integer micro-USD strings. Whole-dollar amounts display two decimal places; meaningful subcent digits are preserved. Costs are estimates, never vendor invoices.

## Checks and limitations

Unit coverage includes real primitive rendering and accessibility attributes, role-specific governance/pricing/reconcile controls, routing residency preservation, absent-health observations, known versus unknown cost rendering, form validation, exact monetary boundaries, and bounded CSV/error handling. These tests use Vitest and server-rendered React; they are not interactive browser, keyboard, focus-trap, clipboard, or full backend integration tests. The shell and form migration need a fresh signed-in browser acceptance pass against the gateway; older dashboard screenshots do not certify this refresh.

Production remains a static Vite build served by Rust. Node.js is used only for vendoring, dependency installation, development, tests, and building.
