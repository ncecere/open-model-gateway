# Offline OpenAPI metaschemas

These are unmodified upstream JSON files, not fetched during tests. `manifest.json` pins each file's exact SHA-256 and canonical source/retrieval URI. The test loader rejects missing/extra JSON files, changed bytes, mismatched `$id`s and duplicate names/URIs before validation. Updating a vendor file and its checksum requires a reviewed contract/tooling change.

- OpenAPI 3.1 structure and schema-base: [2022-10-07 schema](https://spec.openapis.org/oas/3.1/schema/2022-10-07) and [schema-base](https://spec.openapis.org/oas/3.1/schema-base/2022-10-07). Schema-base validates Schema Objects using the OAS dialect, unlike the structure-only metaschema. These describe OpenAPI 3.1.x; the gateway gate additionally requires `openapi: 3.1.0`.
- OAS 3.1 [base dialect](https://spec.openapis.org/oas/3.1/dialect/base) and [base vocabulary](https://spec.openapis.org/oas/3.1/meta/base).
- JSON Schema [Draft 2020-12](https://json-schema.org/draft/2020-12/schema) and its seven referenced meta-vocabularies (core, applicator, unevaluated, validation, meta-data, format-annotation and content).

OpenAPI files are from the OpenAPI Initiative's specification, under [Apache-2.0](LICENSE-OpenAPI). JSON Schema files retain the upstream [license notice](LICENSE-JSON-Schema) (BSD-3-Clause alternative). The validator crate is a separate, exactly pinned dev dependency; no validator macros or network/file fetchers are enabled.
