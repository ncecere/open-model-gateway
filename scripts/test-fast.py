#!/usr/bin/env python3
"""Run the tests that a change touches (the fast tier).

    python3 -I scripts/test-fast.py                  # tests for files changed since origin/main
    python3 -I scripts/test-fast.py --base v0.18.0   # another base
    python3 -I scripts/test-fast.py --all            # every gateway lib test except the contract fixtures
    python3 -I scripts/test-fast.py --contract       # include the OpenAPI contract fixtures
    python3 -I scripts/test-fast.py --plan           # print the selection, run nothing

It is a development shortcut, not the merge gate: GitHub CI runs everything on
every pull request. The selection is by module: a changed file selects the
tests of its own module (the nearest enclosing module that has tests) and the
tests in the same top-level module whose name carries the file's name. A
change to a cross-cutting file selects every lib test. The OpenAPI contract
fixtures (most of the suite's time) run only when the contract surface
changed, or with --contract.

Standard library only; needs cargo, git and (for web changes) npm.
"""
import argparse
import os
import pathlib
import re
import subprocess
import sys
import time

REPO = pathlib.Path(__file__).resolve().parent.parent
GATEWAY_SRC = "apps/gateway/src/"
PACKAGE = "open-model-gateway"

# Files whose change can affect any module: every lib test is selected.
CROSS_CUTTING = (
    "apps/gateway/src/lib.rs",
    "apps/gateway/src/main.rs",
    "apps/gateway/src/http.rs",
    "apps/gateway/src/store.rs",
    "apps/gateway/src/config.rs",
    "apps/gateway/src/db.rs",
    "apps/gateway/src/db/",
    "apps/gateway/src/dbtest_runtime/",
    "apps/gateway/Cargo.toml",
    "apps/gateway/build.rs",
    "apps/gateway/enterprise_migrations/",
    "apps/gateway/migrations/",
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "crates/",
    "vendor/",
)

# The contract surface: a change here also selects the contract fixtures.
CONTRACT_SURFACE = (
    "apps/gateway/src/management_openapi/",
    "apps/gateway/src/inference_openapi/",
    "apps/gateway/src/route_policy.rs",
    "apps/gateway/src/route_policy.snapshot",
    "docs/openapi/",
)

# A top-level module with at most this many tests runs whole; a larger one
# (today only `management`) is narrowed to the changed file's own tests.
SMALL_MODULE = 400

# Tests that are always part of a Rust selection: the source rules read the
# whole tree, so any source change can trip them.
ALWAYS = ("source_rules::",)

# Changes the fast tier has no test for: named in the plan and at the end, so
# a run that passed is not read as covering them.
NOT_COVERED = "NOT COVERED: "
CI_ONLY = (
    ("deploy/staging/runtime-grants.sql", "the runtime_privileges target (gateway-special in CI)"),
)

# Non-Rust areas and the npm scripts that cover them.
NPM_AREAS = (
    (("apps/web/",), ("typecheck:web", "test:web")),
    ((".github/", "Dockerfile", "deploy/loadtest/", "deploy/staging/", ".dockerignore", ".gitleaks.toml"),
     ("test:supply-chain", "test:ci")),
    (("deploy/helm/",), ("test:helm",)),
    (("deploy/monitoring/",), ("test:monitoring",)),
    (("deploy/spark/", "scripts/spark-", "scripts/journeys/", "scripts/demo-", "tests/spark-", "tests/journeys",
      "tests/demo-"), ("test:demo",)),
    (("apps/gateway/src/", "tests/structure"), ("test:structure",)),
)


def is_contract_fixture(name):
    """The OpenAPI contract fixtures: thousands of request vectors per test."""
    return "openapi" in name and "fixtures" in name.rsplit("::", 1)[-1]


def module_candidates(path):
    """Module paths for a gateway source file, most specific first.

    `apps/gateway/src/management/keys/issuance.rs` gives
    `management::keys::issuance`, `management::keys`, `management`.
    """
    parts = path[len(GATEWAY_SRC):].split("/")
    parts[-1] = re.sub(r"\.rs$", "", parts[-1])
    if parts[-1] == "mod":
        parts.pop()
    cleaned = [re.sub(r"_tests?$", "", part) for part in parts]
    candidates = []
    for names in (parts, cleaned):
        for length in range(len(names), 0, -1):
            candidate = "::".join(names[:length])
            if candidate and candidate not in candidates:
                candidates.append(candidate)
    return sorted(candidates, key=lambda c: -c.count("::"))


def keyword(path):
    """The file's own name as a search word: `access_reviews.rs` gives `access_review`."""
    stem = re.sub(r"_tests?$", "", pathlib.PurePosixPath(path).stem)
    if stem in ("mod", "tests", "lib", "main") or len(stem) < 3:
        return None
    return stem[:-1] if stem.endswith("s") else stem


def select(changed, tests, everything=False, contract=False):
    """Returns (selected test names, reasons, integration targets, npm scripts)."""
    reasons, chosen, targets, scripts = [], set(), [], []
    rust = False
    for path in changed:
        for prefixes, names in NPM_AREAS:
            if path.startswith(prefixes):
                scripts.extend(name for name in names if name not in scripts)
        for prefix, owner in CI_ONLY:
            if path.startswith(prefix):
                reasons.append(f"{NOT_COVERED}{path}: run by {owner}")
        if path.startswith(CONTRACT_SURFACE):
            contract = True
            reasons.append(f"{path}: contract surface, contract fixtures included")
        if path.startswith(CROSS_CUTTING):
            everything = True
            rust = True
            reasons.append(f"{path}: cross-cutting, every lib test")
            continue
        if path.startswith("apps/gateway/tests/") and path.endswith(".rs") and "/" not in path[len("apps/gateway/tests/"):]:
            target = pathlib.PurePosixPath(path).stem
            if target not in targets:
                targets.append(target)
            reasons.append(f"{path}: integration target {target}")
            continue
        if path.startswith("apps/gateway/tests/"):
            # `tests/<target>/...` is that target's own module tree; anything
            # else (`tests/common/`) is shared by several targets.
            target = path[len("apps/gateway/tests/"):].split("/")[0]
            if (REPO / "apps/gateway/tests" / f"{target}.rs").is_file():
                if target not in targets:
                    targets.append(target)
                reasons.append(f"{path}: integration target {target}")
            else:
                reasons.append(f"{NOT_COVERED}{path}: shared by the integration targets, which CI runs")
            continue
        if not (path.startswith(GATEWAY_SRC) and path.endswith(".rs")):
            continue
        rust = True
        # `management_openapi/` is mounted as `management::openapi` (a #[path] module).
        path = path.replace(GATEWAY_SRC + "management_openapi/", GATEWAY_SRC + "management/openapi/")
        top = path[len(GATEWAY_SRC):].split("/")[0].removesuffix(".rs")
        word = keyword(path)
        whole = {t for t in tests if t.startswith(top + "::") and not is_contract_fixture(t)}
        if 0 < len(whole) <= SMALL_MODULE:
            reasons.append(f"{path}: all {len(whole)} tests under {top}::")
            chosen |= {t for t in tests if t.startswith(top + "::")}
            continue
        matched = set()
        # A large module: the nearest enclosing module below the top level that has tests.
        for candidate in module_candidates(path):
            if "::" not in candidate:
                break
            matched = {t for t in tests if t.startswith(candidate + "::")}
            if matched:
                reasons.append(f"{path}: {len(matched)} tests under {candidate}::")
                break
        # Tests elsewhere in the top-level module that carry the file's name
        # (`management/access_reviews.rs` and `management::tests::access_review_*`).
        if word:
            named = {t for t in tests if t.startswith(top + "::") and word in t} - matched
            if named:
                reasons.append(f"{path}: {len(named)} tests in {top}:: named *{word}*")
                matched |= named
        if not matched:
            matched = {t for t in tests if t.startswith(top + "::")}
            if matched:
                reasons.append(f"{path}: {len(matched)} tests under {top}:: (nothing narrower found)")
        if not matched:
            everything = True
            reasons.append(f"{path}: no tests found for its module, every lib test")
        chosen |= matched
    if everything:
        chosen = set(tests)
    if rust or everything:
        chosen |= {t for t in tests if t.startswith(ALWAYS)}
    if contract:
        chosen |= {t for t in tests if is_contract_fixture(t) or "openapi" in t}
    if not contract:
        skipped = {t for t in chosen if is_contract_fixture(t)}
        if skipped:
            reasons.append(f"{len(skipped)} contract fixtures left to CI (pass --contract to run them)")
        chosen -= skipped
    return sorted(chosen), reasons, targets, scripts


def git(*args):
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    return subprocess.run(["git", *args], cwd=REPO, env=env, check=True, capture_output=True, text=True).stdout


def changed_files(base):
    merge_base = git("merge-base", base, "HEAD").strip()
    names = set(git("diff", "--name-only", merge_base).splitlines())
    names |= set(git("ls-files", "--others", "--exclude-standard").splitlines())
    return sorted(name for name in names if name)


def cargo_test(jobs, extra):
    command = ["cargo", "test", "-p", PACKAGE, "--all-features"]
    if jobs:
        command += ["-j", str(jobs)]
    return command + extra


def list_tests(jobs):
    listed = subprocess.run(cargo_test(jobs, ["--lib", "--", "--list"]), cwd=REPO, check=True,
                            stdout=subprocess.PIPE, text=True).stdout
    return [line[: -len(": test")] for line in listed.splitlines() if line.endswith(": test")]


def run(command):
    print("+ " + " ".join(command if len(command) < 12 else command[:11] + [f"... ({len(command) - 11} more)"]),
          flush=True)
    started = time.monotonic()
    code = subprocess.run(command, cwd=REPO).returncode
    print(f"  {'ok' if code == 0 else 'FAILED'} in {time.monotonic() - started:.0f}s", flush=True)
    return code


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--base", default="origin/main", help="compare against this ref (default origin/main)")
    parser.add_argument("--all", action="store_true", help="every gateway lib test except the contract fixtures")
    parser.add_argument("--contract", action="store_true", help="include the OpenAPI contract fixtures")
    parser.add_argument("--plan", action="store_true", help="print the selection and stop")
    parser.add_argument("--jobs", type=int, default=0, help="cargo build jobs (default: cargo's)")
    parser.add_argument("--test-threads", type=int, default=0, help="libtest threads (default: libtest's)")
    args = parser.parse_args(argv)

    changed = changed_files(args.base)
    touches_rust = args.all or any(
        path.startswith((GATEWAY_SRC,) + CROSS_CUTTING + CONTRACT_SURFACE) for path in changed)
    tests = list_tests(args.jobs) if touches_rust else []
    selected, reasons, targets, scripts = select(changed, tests, everything=args.all, contract=args.contract)

    print(f"{len(changed)} changed files against {args.base}")
    for reason in reasons:
        print("  " + reason)
    print(f"selected: {len(selected)} of {len(tests)} lib tests"
          + (f", integration targets {', '.join(targets)}" if targets else "")
          + (f", npm {', '.join(scripts)}" if scripts else ""))
    if args.plan:
        return 0

    failed = []
    if touches_rust and run(["cargo", "fmt", "--all", "--", "--check"]) != 0:
        failed.append("cargo fmt")
    if selected:
        tail = ["--lib", "--", "--exact", *selected]
        if args.test_threads:
            tail.insert(2, f"--test-threads={args.test_threads}")
        if run(cargo_test(args.jobs, tail)) != 0:
            failed.append("lib tests")
    for target in targets:
        if run(cargo_test(args.jobs, ["--test", target])) != 0:
            failed.append(f"--test {target}")
    for script in scripts:
        if run(["npm", "run", "--silent", script]) != 0:
            failed.append(f"npm run {script}")
    if failed:
        print("FAILED: " + ", ".join(failed))
        return 1
    uncovered = [reason[len(NOT_COVERED):] for reason in reasons if reason.startswith(NOT_COVERED)]
    if not (selected or targets or scripts):
        print("the fast tier has no test for this change; CI is the check")
    elif uncovered:
        print("fast tier passed for what it covers; left to CI: " + "; ".join(uncovered))
    else:
        print("fast tier passed; the full suite runs in CI")
    return 0


if __name__ == "__main__":
    sys.exit(main())
