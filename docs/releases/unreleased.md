# Open Model Gateway: unreleased

Notes for changes on `main` since v0.4.0. They become the next release's notes.

## Dashboard

- Add model now marks required pricing ceilings, shows a linked validation summary and focuses the first invalid field. Text and System One output-ceiling errors consistently require a positive value (#18).

## Verification

- Browser regressions cover invalid and corrected Add-model submission at desktop and phone widths. The Usage & costs resize check waits for the mobile shell to render before asserting zero horizontal overflow.
