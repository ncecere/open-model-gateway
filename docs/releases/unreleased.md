# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.15.1](v0.15.1.md). They become the next release's notes.

## Contributors

- The supply-chain test now refuses Dockerfile constructs that could make it read a stage BuildKit does not build, or let an unpinned image or remote content into a stage (#485): `<<` in any instruction (heredocs, also split across a `\` continuation, here-strings and shifts), parser-directive comments (`# escape=`, `# syntax=`) and `#!` lines, any instruction outside a fixed list (so `ADD`, `ONBUILD`, `MAINTAINER` and `//syntax=` lines), non-ASCII and control characters and CRLF, a `FROM` that spans lines or takes its image from an `ARG`, blank lines or comments inside a continued instruction, `COPY --from=` or `RUN --mount=...,from=` naming anything but an earlier stage or a digest-pinned image, and quoted or variable `COPY`/`RUN` flags. Neither Dockerfile used any of them. Each failure names the file, the line and the rule; the rules are in [verification.md](../verification.md#supply-chain).
