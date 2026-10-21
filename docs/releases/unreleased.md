# Open Model Gateway: unreleased

Notes for changes on `main` since v0.7.0. They become the next release's notes.

## Added

- **Budget headroom (#103).** Workspace and key budget views separate settled estimates, pending holds and retained unknown-cost floors. Remaining budget is shown only for complete accounting; hidden parent usage stays restricted, and unknown or invalid accounting never becomes available money. Effective headroom is the lowest remaining amount across applicable layers **within each independent period**, not a sum of overlapping scopes. Older gateways without the new facts show headroom unavailable. Reporting only: admission, limits, schema and runtime privileges are unchanged.
