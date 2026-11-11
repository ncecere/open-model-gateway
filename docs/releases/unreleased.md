# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.15.1](v0.15.1.md). They become the next release's notes.

## Contributors

- The supply-chain test now refuses Dockerfile constructs that could make it read a stage BuildKit does not build (#485): heredocs (`<<`), parser-directive comments (`# escape=`, `# syntax=`), non-ASCII characters and CRLF, a `FROM` that spans lines or takes its image from an `ARG`, and blank lines or comments inside a continued instruction. Neither Dockerfile used any of them. The rules are in [verification.md](../verification.md#supply-chain).
