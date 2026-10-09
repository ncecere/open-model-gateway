# Accessibility

The dashboard is checked automatically with [axe-core](https://github.com/dequelabs/axe-core) (`@axe-core/playwright`) in the browser suite (`npm run test:browser`, see [verification](verification.md#browser-suite)). This is regression coverage, not a WCAG conformance certification; screen-reader and zoom passes are still manual.

## What the suite checks

`tests/browser/accessibility.spec.ts` runs after the journey has created real data (a Team, a priced mock model, a key and a request):

| Scope | Pages | Widths |
| --- | --- | --- |
| Signed out | Sign-in | 1440, 390, dark |
| Platform Admin | Overview, Users, Teams + record, Models + record + Add model, Connections, Catalogs, SSO groups, Logs, Usage & costs, Key safety, Audit, Settings › General, Settings › Alerts | 1440, 390 |
| Platform Auditor | Overview, Teams, model record, Audit, Home | 1440, 390 |
| Team admin (Alex) | Home, workspace Overview, Models, API keys, Logs, Usage & costs, Settings | 1440, 390 |
| Member (Blair) | Home, API keys, key record, Logs, request detail, Usage & costs, Notifications, Profile | 1440, 390; dark for Home/keys/Logs/Usage |
| Overlays | Create key dialog, search palette, account menu (inside the navigation drawer at 390) | 1440, 390 |

Rules: `wcag2a`, `wcag2aa`, `wcag21a`, `wcag21aa`, `wcag22aa` and `best-practice`. **Serious or critical violations fail the run.** Moderate/minor violations and axe "needs review" results are written to `target/browser-tests/state/axe-findings.jsonl` for review; nothing is silently ignored. Scans wait for finite animations to finish so fade-ins are not measured mid-transition.

Additional keyboard and preference checks:

- The first Tab lands on the visible **Skip to content** link, which moves focus into `<main>`.
- Modal dialogs trap focus (25 Tab presses stay inside the Create key dialog) and Escape returns focus to the opener.
- Every `aria-controls` on the page references a rendered element.
- With `prefers-reduced-motion: reduce`, no element keeps an animation or transition longer than 1 ms.
- Key pages (list, record, Create key dialog) have no horizontal document scroll at 390px (journey spec).

## Findings and fixes (2026-10-09)

| Finding | Impact | Fix |
| --- | --- | --- |
| Narrow-window sidebar toggle kept `aria-controls` pointing at the drawer while the closed drawer was unmounted (dangling reference) | Invalid ARIA reference (axe `aria-valid-attr-value`, found by the suite's reference check) | Gateway-owned `ShellSidebarToggle` in `components/layout/shell.tsx` wraps the Bitop toggle behaviour and sets `aria-controls` only while the sidebar is rendered. The vendored `SidebarToggle` is unchanged (`TopBar sidebarToggle={false}`). |
| Admin › Catalogs empty state used an `h3` directly under the page `h1` (`heading-order`) | Moderate | `Empty` in `components/ui.tsx` takes `titleAs`; the catalog list passes `h2`. |
| Search palette contrast reported at 1.7–3.3:1 | — (false positive) | Measured during the popup fade-in over the backdrop. The suite now waits for animations; the settled palette passes. |

Already in place and confirmed by the scans: the skip link and `main` landmark, labelled sidebar/banner/breadcrumb landmarks, named icon buttons (`Actions for …`, `Copy model ID`, `Collapse sidebar`), labelled form controls including Bitop selects/switches/radio cards, table captions with row headers, labelled scroll regions for wide tables, and a global reduced-motion rule in the Bitop styles.

## Manual review items (not failures)

- **Keyboard glyphs** (`⌘`, `K`, `Esc` in the search trigger and palette footer) are `aria-hidden` decorative hints; axe cannot rate non-text glyph contrast.
- **Sign-in page** text sits on a subtle radial gradient. Measured against both gradient stops in light mode: headings ≥15.5:1, body ≥6.2:1, the footnote 4.71:1. Dark mode text is measured against the page background (≥6.7:1); the gradient there is a low-alpha tint.
- **"On this page" section links** on record pages are sometimes partly covered by the sticky header while axe scrolls, so contrast is "needs review"; the same link tokens pass on other pages.
- **Base UI focus guards** (`data-base-ui-focus-guard`) and the `aria-hidden` `#root` while a modal is open are reported as `aria-hidden-focus` "needs review". The guards hand focus back into the popup; the focus-trap check above verifies it.
- **Account menu inside the 390px drawer** is portalled to `<body>`, outside a landmark (`region`, moderate). It is a transient Bitop/Base UI menu; changing that belongs in bitop-ui, not the vendored copy.
- Not automated: screen-reader announcements (VoiceOver/NVDA), 200%/400% zoom and text spacing, Windows high-contrast mode, charts' data alternatives beyond their text labels.
