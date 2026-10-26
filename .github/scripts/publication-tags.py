#!/usr/bin/env python3
"""Refuse native publication outside the trusted ref's exact tag set."""
import os
import re
import sys

IMAGE = "ghcr.io/ncecere/open-model-gateway"
VERSION = re.compile(r"refs/tags/(v([0-9]+)\.([0-9]+)\.[0-9]+)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?")


def validate(tags, ref, sha):
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise ValueError("invalid source SHA")
    if ref == "refs/heads/main":
        expected = {f"{IMAGE}:sha-{sha[:7]}"}
    else:
        match = VERSION.fullmatch(ref)
        if match is None:
            raise ValueError("unsupported publication ref")
        _, major, minor, prerelease = match.groups()
        tag = ref[len("refs/tags/"):]
        if len(tag) > 128:
            raise ValueError("version tag too long")
        if prerelease:
            for part in prerelease.split("."):
                if part.isdigit() and len(part) > 1 and part.startswith("0"):
                    raise ValueError("invalid numeric prerelease identifier")
        expected = {f"{IMAGE}:{tag}"}
        if prerelease is None:
            expected.update({f"{IMAGE}:v{major}.{minor}", f"{IMAGE}:latest-release"})
    actual = tags.split("\n")
    if len(actual) != len(set(actual)) or set(actual) != expected:
        raise ValueError("tags do not exactly match the trusted publication ref")


def main():
    try:
        if len(sys.argv) != 1:
            raise ValueError("arguments are not supported")
        validate(os.environ["TAGS"], os.environ["GITHUB_REF"], os.environ["GITHUB_SHA"])
    except (KeyError, ValueError) as error:
        print(f"publication-tags: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
