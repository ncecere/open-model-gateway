#!/usr/bin/env python3
"""Run the pinned gitleaks scanner with the repository config (#596). Optional local hook.

    python3 -I scripts/secret-scan.py staged    # what is staged for the next commit
    python3 -I scripts/secret-scan.py history   # branches, remote branches and tags

It runs the same scanner version, config and flags as the `secret-scan` job in
.github/workflows/ci.yml. It does not install anything and fails closed: a missing or
different scanner is an error, never a pass. To use it as a pre-commit hook, create
.git/hooks/pre-commit yourself (see docs/verification.md, "Secret scanning"); nothing in
this repository installs it.

The scanner is found through $GITLEAKS_BIN, else `gitleaks` on PATH. History is scanned
from --branches --remotes --tags and not --all: other tools keep private refs in the same
repository (editors, agents, stash-like checkpoints) that can hold snapshots of untracked
files, and those are not this project's history.
"""
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import NoReturn

# The pin. Keep it equal to the URL and checksum in the secret-scan job of ci.yml
# (tests/supply-chain/secret-scan.test.mjs fails when they differ).
PINNED_VERSION = "8.30.1"
PINNED_URL = "https://github.com/gitleaks/gitleaks/releases/download/v8.30.1/gitleaks_8.30.1_linux_x64.tar.gz"
PINNED_SHA256 = "551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb"

COMMON = ["--redact", "--no-banner", "--no-color", "--ignore-gitleaks-allow", "--verbose", "--log-level", "info"]


def fail(message: str, status: int = 2) -> NoReturn:
    print(f"secret-scan: {message}", file=sys.stderr)
    raise SystemExit(status)


def install_hint() -> str:
    return (
        f"install the pinned version {PINNED_VERSION} (linux x86_64; other platforms: take the matching asset and its\n"
        f"checksum from https://github.com/gitleaks/gitleaks/releases/tag/v{PINNED_VERSION}):\n"
        f"    mkdir -p ~/.local/bin/gitleaks-{PINNED_VERSION} && cd ~/.local/bin/gitleaks-{PINNED_VERSION}\n"
        f"    bash <repo>/.github/scripts/fetch-verified.sh {PINNED_URL} {PINNED_SHA256} gitleaks.tar.gz\n"
        f"    tar -xzf gitleaks.tar.gz gitleaks\n"
        f"then put that directory on PATH or set GITLEAKS_BIN to the gitleaks file."
    )


def find_scanner() -> str:
    candidate = os.environ.get("GITLEAKS_BIN") or shutil.which("gitleaks")
    if not candidate or not os.access(candidate, os.X_OK) or not Path(candidate).is_file():
        fail(f"gitleaks {PINNED_VERSION} is not installed, so nothing was scanned and the commit is refused.\n{install_hint()}")
    try:
        result = subprocess.run([candidate, "version"], capture_output=True, text=True, timeout=30, check=False)
    except (OSError, subprocess.SubprocessError) as error:
        fail(f"could not run {candidate}: {error}")
    found = result.stdout.strip()
    if result.returncode != 0 or found.removeprefix("v") != PINNED_VERSION:
        fail(f"{candidate} reports version {found or 'unknown'}, but this repository pins {PINNED_VERSION}; refusing to scan with another version.\n{install_hint()}")
    return candidate


def repository_root() -> Path:
    result = subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=False)
    if result.returncode != 0:
        fail("not inside a git work tree")
    return Path(result.stdout.strip())


def main(argv: list[str]) -> int:
    if len(argv) != 2 or argv[1] not in ("staged", "history"):
        fail("usage: secret-scan.py staged|history", 64)
    root = repository_root()
    scanner = find_scanner()
    config = ["--config", str(root / ".gitleaks.toml")]
    if argv[1] == "staged":
        command = [scanner, "git", "--staged", *config, *COMMON, str(root)]
    else:
        command = [scanner, "git", "--log-opts=--branches --remotes --tags", *config, *COMMON, str(root)]
    status = subprocess.run(command, cwd=root, check=False).returncode
    if status != 0:
        print(
            "\nsecret-scan: the scanner reported a finding or failed (exit status %d). Values are redacted above.\n"
            "  If it is a real credential: rotate or revoke it first, then remove it from the change.\n"
            "  Removing it from a later commit does not make it safe. Do not add a real secret to .gitleaks.toml.\n"
            "  If it is a test fixture or a false positive, add the narrowest entry to .gitleaks.toml with its reason." % status,
            file=sys.stderr,
        )
    return status


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
