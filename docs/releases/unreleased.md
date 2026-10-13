# Open Model Gateway: unreleased

Notes for changes on `main` since v0.4.0. They become the next release's notes.

## Inference

- `POST /v1/responses` with `stream: true` now forwards text as `response.output_text.delta` events as the model generates it, instead of after the model finishes. Tool calls are still emitted whole, after the text; usage, settlement and the 4 MiB output bound are unchanged (#22).
- `POST /v1/messages` with `stream: true` now sends `message_start` immediately and streams text as `text_delta` events; tool calls arrive whole after the text. `message_start` usage is now `{"input_tokens":0}` (a placeholder); the observed input, cache and output counts arrive in `message_delta`, which the Anthropic SDKs apply (#23).

## Scale

- Dashboard lists stay fast with tens of thousands of users (#65). The platform users list builds rows only for the requested page and per-user lookups are indexed (migration `0037_user_lookup_indexes`): page 1 took 1.1 s and deep pages minutes to hours with 50,000 users. The platform users, user detail and workspaces lists, and workspace members, keys and service accounts, read from the same lock-free snapshots as reports, so one slow list no longer queues every other admin request behind it; like those reads, a statement running over 10 s is cancelled and returns 503 "The request took too long".

## Dashboard

- Add model now marks required pricing ceilings, shows a linked validation summary and focuses the first invalid field. Text and System One output-ceiling errors consistently require a positive value (#18).

## Deployment

- New `open-model-gateway grants apply` applies the reviewed runtime grants (`deploy/staging/runtime-grants.sql`, now embedded in the binary); `grants verify` runs the rollback-only privilege probes. The Helm migration Job and the staging `grant-runtime` step use it, so the chart no longer needs a PostgreSQL client image (`migrations.grants.image` is removed) or a copied SQL file (#20).

- Helm: a first install with `migrations.run=true` is now one step, including the chart-managed CloudNativePG Cluster. The migration Job runs as a plain resource on install (a `pre-upgrade` hook on upgrades) and waits for the database; gateway pods wait for the schema (`wait-for-database --schema`) instead of crash-looping. New `open-model-gateway wait-for-database [--schema]` (#19).

## Verification

- Browser regressions cover invalid and corrected Add-model submission at desktop and phone widths. The Usage & costs resize check waits for the mobile shell to render before asserting zero horizontal overflow.
