# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.15.1](v0.15.1.md). They become the next release's notes.

## Contributors

- **The local supply-chain tests need Bash 4.4 or newer first on `PATH`** (#380). `npm run test:supply-chain` and `tests/oci-tool.test.mjs` now fail immediately, naming the required and the found version, when `bash` is older, missing or not Bash; they are never skipped. Under macOS's `/bin/bash` 3.2 two refusal tests used to see guards return 0, because before Bash 4.1 a failing `[[ ]]` does not end a `set -e` script. Install a newer Bash (for example Homebrew's) and put it first on `PATH`. CI is unchanged (Bash 5.2). Details: [verification.md](../verification.md#bash-for-the-shell-fixtures).
