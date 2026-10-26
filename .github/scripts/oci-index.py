#!/usr/bin/env python3
"""Bind native creation metadata and raw index bytes to approved parent graphs.

Data only: parents were archive-validated upstream. No layer reads, transport,
credentials, extraction or producer authentication are implemented here.
"""
import hashlib
import importlib.util
import os
from pathlib import Path
import stat
import sys

CONTROL = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("oci_layout", CONTROL / "oci-layout.py")
layout = importlib.util.module_from_spec(spec)
spec.loader.exec_module(layout)


def read(path):
    path = Path(os.path.abspath(path))
    layout._require(not any(p.is_symlink() for p in (path, *path.parents)),
                    "symlink input path")
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        layout._require(stat.S_ISREG(info.st_mode) and info.st_size <= layout.MAX_JSON_BYTES,
                        "nonregular or oversized JSON file")
        data = stream.read(layout.MAX_JSON_BYTES + 1)
        layout._require(len(data) == info.st_size, "JSON file changed or exceeds limit")
    return data


def document(path, expected):
    layout._require(type(expected) is str and layout.DIGEST.fullmatch(expected),
                    "invalid expected digest")
    raw = read(path)
    layout._require("sha256:" + hashlib.sha256(raw).hexdigest() == expected,
                    "raw index digest mismatch")
    value = layout._json(raw)
    layout._document(value, layout.INDEX, ("manifests",))
    return raw, value


def verify(metadata, amd64, digest_amd64, arm64, digest_arm64, published, candidate):
    meta = layout._json(read(metadata))
    layout._require(type(meta) is dict and "containerimage.descriptor" in meta,
                    "missing native creation descriptor")
    descriptor = meta["containerimage.descriptor"]
    layout._descriptor(descriptor, {layout.INDEX})
    raw, index = document(published, candidate)
    layout._require(descriptor["digest"] == candidate and descriptor["size"] == len(raw),
                    "creation descriptor mismatch")
    approved = []
    for path, expected, arch in ((amd64, digest_amd64, "amd64"),
                                 (arm64, digest_arm64, "arm64")):
        _, parent = document(path, expected)
        layout._children(parent, "linux/" + arch)
        approved.extend(parent["manifests"])
    children = index["manifests"]
    layout._require(type(children) is list and len(children) == len(approved),
                    "changed published child count")
    for child in children:
        layout._descriptor(child, {layout.MANIFEST}, allow_reference=True)
    layout._require(len({c["digest"] for c in approved}) == len(approved),
                    "colliding approved children")
    # Canonical bytes preserve JSON types (unlike Python equality: 1 == True).
    layout._require(sorted(map(layout._encoded, children)) == sorted(map(layout._encoded, approved)),
                    "published graph is not the exact approved union")


def main():
    try:
        layout._require(len(sys.argv) == 8, "expected metadata and three file/digest pairs")
        verify(*sys.argv[1:])
    except (ValueError, OSError):
        print("oci-index: validation failed", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
