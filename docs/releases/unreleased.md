# Open Model Gateway: unreleased

Notes for changes on `main` since v0.4.0. They become the next release's notes.

## Dashboard

- Add model now marks required pricing ceilings, shows a linked validation summary and focuses the first invalid field. Text and System One output-ceiling errors consistently require a positive value (#18).

## Deployment

- New `open-model-gateway grants apply` applies the reviewed runtime grants (`deploy/staging/runtime-grants.sql`, now embedded in the binary); `grants verify` runs the rollback-only privilege probes. The Helm migration Job and the staging `grant-runtime` step use it, so the chart no longer needs a PostgreSQL client image (`migrations.grants.image` is removed) or a copied SQL file (#20).

- Helm: a first install with `migrations.run=true` is now one step, including the chart-managed CloudNativePG Cluster. The migration Job runs as a plain resource on install (a `pre-upgrade` hook on upgrades) and waits for the database; gateway pods wait for the schema (`wait-for-database --schema`) instead of crash-looping. New `open-model-gateway wait-for-database [--schema]` (#19).

## Verification

- Browser regressions cover invalid and corrected Add-model submission at desktop and phone widths. The Usage & costs resize check waits for the mobile shell to render before asserting zero horizontal overflow.
