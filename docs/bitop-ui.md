# Bitop UI integration

## Sources and ownership

This dashboard uses **copy-and-own source**, not a published Bitop npm package or network registry endpoint. The source is a local, read-only bitop-ui checkout (by default `../../../../typescript/bitop-ui` from `apps/web`, i.e. `~/projects/typescript/bitop-ui`); components are copied from its `registry/bitop/ui/` and `registry/bitop/lib/` with bitop-ui's own installer (`packages/cli` in that checkout), which only reads the checkout.

`apps/web/bitop-lock.json` (written by the installer) records every installed item and the SHA-256 of each file it wrote; `apps/web/src/lib/bitop-lock.test.ts` fails if a vendored file no longer matches its recorded hash, so local edits or partial refreshes can't slip in unnoticed. `apps/web/components.json` holds only the import aliases: no registry URL is configured, and the checkout path is passed on each run with `--registry`. The registry homepage contains a placeholder; it is deliberately not used as a release or installation URL. The source's MIT license notice (copyright © 2026 Nicholas Cecere) is kept verbatim in `apps/web/src/components/ui/LICENSE` and checked by the same test. It does not assign a license to unrelated gateway code.

The layout design reference is the read-only file:

`/Users/nicholascecere/projects/golang/open-rag-system/web/src/components/layout.tsx`

SHA-256: `dc176a5e83de1556806cc6e82ad8d9c0903e965bacf59b8554c0ccfe3a358e04`.

The gateway adopts its compact collapsible sidebar, Workspace/Admin mode switch, grouped People/Models/Oversight navigation, workspace switcher, breadcrumbs, header command palette, and sidebar account menu. The full-width content footer is omitted. It does **not** copy RAG API clients, auth, query definitions, capability rules, teams, knowledge-base logic, or storage keys.

## Reproduce the copy

From `apps/web`, with the bitop-ui checkout pulled to the version you want:

```sh
BITOP="node ../../../../typescript/bitop-ui/packages/cli/bin/bitop.mjs"
SRC=../../../../typescript/bitop-ui   # any local bitop-ui checkout

$BITOP diff --check --registry $SRC   # exit 1 if anything differs from the checkout (writes nothing)
$BITOP diff --registry $SRC           # show the differences
$BITOP update --registry $SRC         # refresh every installed item; locally edited files are skipped and listed
$BITOP add <item> --registry $SRC     # add another item with its dependencies
```

Then, from the repository root:

```sh
npm install              # only if the installer reported new npm dependencies (use --no-install to just print them)
npm run typecheck:web
npm run test:web
npm run build:web
```

The installer resolves the selected items plus all recursive `@bitop/*` dependencies and copies their declared files to the registry targets, found through the `@/*` → `src/*` path in `tsconfig.json`:

- `@ui/*` → `apps/web/src/components/ui/*`
- `@lib/*` → `apps/web/src/lib/*`

These source import aliases are rewritten:

- `@/registry/bitop/ui/` → `@/components/ui/`
- `@/registry/bitop/lib/` → `@/lib/`

CSS modules, neutral-theme tokens, Base UI behavior, comments, and client directives are otherwise copied unchanged. `"use client"` is harmless in Vite; it does not introduce Next.js. The installer refuses targets outside `src/components/ui` and `src/lib`, won't write through symlinks, and makes no network calls for a local checkout. Packages the gateway already lists keep their versions; new npm dependencies are installed through the repository lockfile.

There are **no local patches**. The earlier copy-and-own patch that added an optional `className` to the command-palette popup (so gateway CSS can use the higher-contrast muted-text token for search group labels and hints) is now upstream in bitop-ui (`CommandPalette` `className`). If the gateway needs a change to a vendored file, make it in bitop-ui with docs and tests, then `update`; don't edit the copy here (the lock test fails, and `update` would skip the file).

Selected items: app-shell, breadcrumbs, table, card, button, page-header, stat-card, input, field, badge, empty-state, command-palette, tabs, checkbox. Recursive items include core, theme-neutral, avatar, layout, menu, tooltip, spinner, and kbd. The lock tracks 46 component/support files; the license notice is kept alongside them. Existing gateway metric tiles remain unlinked. `sparkline`, `meter` and `copy-button` were added later with `bitop add` for the shared UI kit in `src/components/templates/` (see its README). Runtime package dependencies are `@base-ui/react`, `lucide-react`, and `@fontsource-variable/inter`, alongside the existing React stack.

## Integration boundaries

- `src/main.tsx` imports `components/ui/styles/bitop.css` once, before gateway composition CSS. Core imports structural tokens, the neutral theme, reset, and local Inter font assets.
- TypeScript and Vite both resolve `@/*` to `apps/web/src/*`.
- `src/pages/home.tsx` uses the real Bitop `AppShell`, Sidebar parts, `Brand`, `WorkspaceSwitcher`, `TopBar`, `Main`, menus, and breadcrumbs. The native skip target, keyboard controls, collapsed tooltips, and sidebar toggle come from Bitop. Collapse state is memory-only and survives page changes.
- `src/components/jump-search.tsx` composes Bitop's CommandPalette, CommandPaletteTrigger, and keyboard shortcut hook. `src/lib/search.ts` filters session-scoped destinations with existing UI permissions; searches never fetch secrets or activity. The palette's upstream `finalFocus` hook handles in-place focus restoration. For navigation that remounts the scope-keyed shell, the stable dashboard parent focuses the destination heading after the remount; dialog and transient-secret isolation is unchanged. Dismissal and refresh retain default focus restoration. The refreshed Table primitive adds its keyboard-scroll region only when ResizeObserver detects overflow, not during server rendering.
- `src/components/ui.tsx` preserves gateway-facing wrappers and its original action lifecycle while composing actual Bitop Table/Card/PageHeader/Badge/EmptyState components. Forms use Field/Input/NativeSelect/Textarea, labelled Checkbox/CheckboxGroup selections, and Bitop submit/cancel buttons. Overview and Costs use StatCard. Upstream Button now cancels activation for disabled/loading non-native render targets (such as links), in addition to marking them `aria-disabled`. Existing CRUD button classes bridge to the same tokens; they are not a second sidebar implementation.
- Governance uses Bitop Tabs with `variant="pills"` for authorized scope selection and effective/local/inherited details. Inactive panels are unmounted; the workspace opens by default. Base UI supplies tab roles, selection, and arrow-key behavior.
- Gateway-specific CSS is in `src/styles.css`. Vendored component styles are not edited to mimic the reference layout.
- The gateway owns all queries, permission guards, CSRF/same-origin transport, and CRUD semantics. Rust remains the authorization boundary. No secrets enter browser storage or TanStack query/mutation caches; one-time tokens remain in transient dialog state.
- The CSV helper accepts only an encoded workspace identifier and bounded integers, uses same-origin credentials with redirects disabled and no cache, checks CSV content type, caps streamed response bytes, aborts on scope exit, and revokes transient Blob URLs after download.
- Amounts remain decimal integer micro-USD strings. Formatting/conversion uses BigInt, never floating-point currency rounding. Budget and price inputs use decimal US dollars (up to six places), with exact conversion to the API's integer micro-USD strings. Whole-dollar amounts display two decimal places; meaningful subcent digits are preserved. Costs are estimates, never vendor invoices.

## Checks and limitations

Unit coverage includes real primitive rendering and accessibility attributes, role-specific governance/pricing/reconcile controls, routing residency preservation, absent-health observations, known versus unknown cost rendering, form validation, exact monetary boundaries, and bounded CSV/error handling. These tests use Vitest and server-rendered React; they are not interactive browser, keyboard, focus-trap, clipboard, or full backend integration tests. The shell and form migration need a fresh signed-in browser acceptance pass against the gateway; older dashboard screenshots do not certify this refresh.

Production remains a static Vite build served by Rust. Node.js is used only for vendoring, dependency installation, development, tests, and building.
