# Open Model Gateway: unreleased

Notes for changes on `main` since v0.4.0. They become the next release's notes.

## Inference

- `POST /v1/responses` with `stream: true` now forwards text as `response.output_text.delta` events as the model generates it, instead of after the model finishes. Tool calls are still emitted whole, after the text; usage, settlement and the 4 MiB output bound are unchanged (#22).
- `POST /v1/messages` with `stream: true` now sends `message_start` immediately and streams text as `text_delta` events; tool calls arrive whole after the text. `message_start` usage is now `{"input_tokens":0}` (a placeholder); the observed input, cache and output counts arrive in `message_delta`, which the Anthropic SDKs apply (#23).

## Dashboard

- Add model now marks required pricing ceilings, shows a linked validation summary and focuses the first invalid field. Text and System One output-ceiling errors consistently require a positive value (#18).

## Deployment

- New `open-model-gateway grants apply` applies the reviewed runtime grants (`deploy/staging/runtime-grants.sql`, now embedded in the binary); `grants verify` runs the rollback-only privilege probes. The Helm migration Job and the staging `grant-runtime` step use it, so the chart no longer needs a PostgreSQL client image (`migrations.grants.image` is removed) or a copied SQL file (#20).

## Verification

- Browser regressions cover invalid and corrected Add-model submission at desktop and phone widths. The Usage & costs resize check waits for the mobile shell to render before asserting zero horizontal overflow.
