#!/usr/bin/env python3
"""Inspect active SDK graphs and refuse Chrono even in the stricter metadata inventory."""
import json
from pathlib import Path
import subprocess
import tomllib

manifest = Path(__file__).resolve().with_name("Cargo.toml")
strict_failures = []
for features in ([], ["--no-default-features"], ["--all-features"]):
    label = " ".join(features) or "default"
    # Cargo tree resolves active features; dev and all target-specific edges are explicit.
    tree = subprocess.check_output([
        "cargo", "tree", "--manifest-path", str(manifest), "--locked",
        "--target", "all", "--edges", "normal,build,dev", "--prefix", "none",
        "--format", "{p}", *features,
    ], text=True)
    assert not any(line.startswith("chrono v") for line in tree.splitlines()), label
    print(f"Active graph has no Chrono: {label} (dev/build/all targets)", flush=True)
    # Do not silently equate that with a clean lockfile/metadata inventory.
    metadata = json.loads(subprocess.check_output([
        "cargo", "metadata", "--manifest-path", str(manifest),
        "--locked", "--format-version", "1", *features,
    ]))
    for package in metadata["packages"]:
        if package["name"] == "chrono":
            strict_failures.append(f"{label}: {package['id']}")
    for node in metadata["resolve"]["nodes"]:
        if "#serde_with@" in node["id"]:
            assert not any(f.startswith("chrono") for f in node["features"]), node
if strict_failures:
    raise SystemExit("Strict metadata inventory FAILED; inactive Chrono remains:\n" + "\n".join(strict_failures))
print("Strict metadata inventory has no Chrono")
lock = tomllib.loads(manifest.with_name("Cargo.lock").read_text())
assert not any(package["name"] == "chrono" for package in lock["package"]), "Chrono remains in lockfile"
print("Lockfile inventory has no Chrono")
