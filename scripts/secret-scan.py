#!/usr/bin/env python3
"""Run the pinned gitleaks scanner with the repository config (#596). Optional local hook.

    python3 -I scripts/secret-scan.py staged    # the content staged for the next commit
    python3 -I scripts/secret-scan.py history   # the history of the checked-out commit

It runs the same scanner version, config and options as the `secret-scan` job in
.github/workflows/ci.yml, and prints findings the same way: rule, file, line and commit,
never the matched text. It does not install anything and fails closed: a missing or
different scanner is an error, never a pass. To use it as a pre-commit hook, create
.git/hooks/pre-commit yourself (docs/verification.md, "Secret scanning"); nothing in this
repository installs it.

The scanner is found through $GITLEAKS_BIN (a path, or a name looked up on PATH), else
`gitleaks` on PATH.

`staged` writes the staged blobs themselves to a temporary directory and scans that, so a
`.gitattributes` `-diff` or a NUL byte cannot hide a secret from a patch. `history` scans
what CI scans for a push of the checked-out commit (`-m --text HEAD`); it does not read
other branches or the private refs other tools keep in the same repository (editors, agents
and checkpointing tools), which can hold snapshots of untracked files.
"""
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import NoReturn

# The pinned version; the install steps and the checksum are in docs/verification.md
# ("Secret scanning"). tests/supply-chain/secret-scan.test.mjs fails when this differs from
# the version of the release URL in ci.yml.
PINNED_VERSION = "8.30.1"

# Findings print as one line of rule, file, line and commit; same text as ci.yml.
REPORT_TEMPLATE = "{{ range . }}finding rule={{ .RuleID }} file={{ .File }} line={{ .StartLine }} commit={{ .Commit }}\n{{ end }}\n"


def fail(message: str, status: int = 2) -> NoReturn:
    print(f"secret-scan: {message}", file=sys.stderr)
    raise SystemExit(status)


def find_scanner() -> str:
    requested = os.environ.get("GITLEAKS_BIN") or "gitleaks"
    candidate = shutil.which(requested)
    if not candidate:
        fail(
            f"gitleaks {PINNED_VERSION} is not installed (looked for '{requested}'), so nothing was scanned and the commit is refused.\n"
            f"Install the pinned release as described in docs/verification.md#secret-scanning, then put it on PATH or set GITLEAKS_BIN."
        )
    try:
        result = subprocess.run([candidate, "version"], capture_output=True, text=True, timeout=30, check=False)
    except (OSError, subprocess.SubprocessError) as error:
        fail(f"could not run {candidate}: {error}")
    found = result.stdout.strip()
    if result.returncode != 0 or found.removeprefix("v") != PINNED_VERSION:
        fail(
            f"{candidate} reports version {found or 'unknown'}, but this repository pins {PINNED_VERSION}; refusing to scan with another version.\n"
            f"Install the pinned release as described in docs/verification.md#secret-scanning."
        )
    return candidate


def repository_root() -> Path:
    result = subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=False)
    if result.returncode != 0:
        fail("not inside a git work tree")
    return Path(result.stdout.strip())


def staged_paths(root: Path) -> bytes:
    result = subprocess.run(
        ["git", "diff", "--cached", "--name-only", "-z", "--diff-filter=ACMR"], cwd=root, capture_output=True, check=False
    )
    if result.returncode != 0:
        fail("git diff --cached failed")
    return result.stdout


def scan(scanner: str, mode: list[str], root: Path, work: Path, cwd: Path) -> int:
    template = work / "report.tmpl"
    template.write_text(REPORT_TEMPLATE)
    common = [
        "--config", str(root / ".gitleaks.toml"), "--redact", "--no-banner", "--no-color", "--ignore-gitleaks-allow",
        "--log-level", "info", "--report-format", "template", "--report-template", str(template), "--report-path", "-",
    ]
    command = [scanner, mode[0], *common, *mode[1:]]
    return subprocess.run(command, cwd=cwd, check=False).returncode


def main(argv: list[str]) -> int:
    if len(argv) != 2 or argv[1] not in ("staged", "history"):
        fail("usage: secret-scan.py staged|history", 64)
    root = repository_root()
    scanner = find_scanner()
    work = Path(tempfile.mkdtemp(prefix="secret-scan-work-"))
    try:
        if argv[1] == "staged":
            paths = staged_paths(root)
            if not paths:
                print("secret-scan: nothing staged")
                return 0
            export = work / "staged"
            export.mkdir()
            exported = subprocess.run(
                ["git", "checkout-index", "-z", "--stdin", f"--prefix={export}/"], cwd=root, input=paths, capture_output=True, check=False
            )
            if exported.returncode != 0:
                fail("could not export the staged files")
            # Run inside the export so file names are repository-relative, as in CI.
            status = scan(scanner, ["dir", "."], root, work, export)
        else:
            status = scan(scanner, ["git", "--log-opts=-m --text HEAD", str(root)], root, work, root)
    finally:
        shutil.rmtree(work, ignore_errors=True)
    if status != 0:
        print(
            "\nsecret-scan: the scanner reported a finding or failed (exit status %d). The matched text is not printed.\n"
            "  If it is a real credential: rotate or revoke it first, then remove it from the change.\n"
            "  Removing it from a later commit does not make it safe. Do not add a real secret to .gitleaks.toml.\n"
            "  If it is a test fixture or a false positive, add the narrowest entry to .gitleaks.toml with its reason." % status,
            file=sys.stderr,
        )
    return status


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
