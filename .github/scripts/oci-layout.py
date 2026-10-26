"""Bounded local OCI-data inspection, not transport or provenance verification.

Only per-platform OCI indexes with one runnable and bound SPDX/SLSA evidence
are supported. TAR extensions (including PAX), root manifests and no-attestation
roots fail closed. Layer bodies stay opaque; nothing is extracted or executed.
Internal digest binding does not authenticate producers or approve roots: trusted
Preflight artifact-ID selection and verified-root allowlisting remain mandatory
external requirements.
"""
import hashlib
import json
import os
import re
import stat
import tarfile

MAX_MEMBERS = 4096
MAX_TOTAL_BYTES = 2 * 1024**3
MAX_JSON_BYTES = 32 * 1024**2
MAX_DEPTH = 32
INDEX = "application/vnd.oci.image.index.v1+json"
MANIFEST = "application/vnd.oci.image.manifest.v1+json"
CONFIG = "application/vnd.oci.image.config.v1+json"
INTOTO = "application/vnd.in-toto+json"
REF = "vnd.docker.reference.digest"
KIND = "vnd.docker.reference.type"
SPDX = "https://spdx.dev/Document"
SLSA = "https://slsa.dev/provenance/v0.2"
LAYERS = {"application/vnd.oci.image.layer.v1.tar" + suffix
          for suffix in ("", "+gzip", "+zstd")}
DIGEST = re.compile(r"sha256:[0-9a-f]{64}\Z")
BLOB = re.compile(r"blobs/sha256/[0-9a-f]{64}\Z")


def _require(condition, message):
    if not condition:
        raise ValueError(message)


def _fields(value, required, optional=()):
    _require(type(value) is dict and set(required) <= value.keys()
             and value.keys() <= set(required) | set(optional), "unsupported object shape")


def _annotations(value):
    _require(type(value) is dict and all(type(k) is str and type(v) is str
                                       for k, v in value.items()), "invalid annotations")
    return value


def _pairs(pairs):
    result = {}
    for key, value in pairs:
        _require(key not in result, "duplicate JSON key")
        result[key] = value
    return result


def _constant(value):
    raise ValueError("non-finite JSON number")


def _json(data):
    _require(len(data) <= MAX_JSON_BYTES, "JSON byte limit")
    # Bound nesting before the recursive stdlib decoder sees hostile input.
    depth, quoted, escaped = 0, False, False
    for byte in data:
        if quoted:
            if escaped:
                escaped = False
            elif byte == 92:
                escaped = True
            elif byte == 34:
                quoted = False
        elif byte == 34:
            quoted = True
        elif byte in (91, 123):
            depth += 1
            _require(depth <= MAX_DEPTH, "JSON depth limit")
        elif byte in (93, 125):
            depth -= 1
    try:
        return json.loads(data.decode("utf-8"), object_pairs_hook=_pairs,
                          parse_constant=_constant, parse_float=_finite_float)
    except (UnicodeError, RecursionError) as exc:
        raise ValueError("invalid JSON") from exc


def _finite_float(value):
    result = float(value)
    _require(abs(result) != float("inf"), "non-finite JSON number")
    return result


def _encoded(value):
    try:
        data = json.dumps(value, sort_keys=True, separators=(",", ":"),
                          allow_nan=False).encode()
    except (TypeError, RecursionError, UnicodeError) as exc:
        raise ValueError("invalid report JSON") from exc
    _json(data)
    return data


def _descriptor(value, media, allow_reference=False):
    _fields(value, ("mediaType", "digest", "size"), ("platform", "annotations"))
    _require(type(value["mediaType"]) is str and value["mediaType"] in media
             and type(value["digest"]) is str
             and DIGEST.fullmatch(value["digest"]), "unsupported descriptor")
    _require(type(value["size"]) is int and 0 <= value["size"] <= MAX_TOTAL_BYTES,
             "invalid descriptor size")
    annotations = _annotations(value.get("annotations", {}))
    _require(allow_reference or not {KIND, REF} & annotations.keys(), "unexpected reference association")
    if "platform" in value:
        platform = value["platform"]
        _fields(platform, ("os", "architecture"), ("variant", "os.version", "os.features"))
        _require(all(type(v) is str for k, v in platform.items() if k != "os.features"),
                 "invalid platform")
        if "os.features" in platform:
            _require(type(platform["os.features"]) is list
                     and all(type(v) is str for v in platform["os.features"]), "invalid features")


def _document(value, media, children):
    _fields(value, ("schemaVersion", "mediaType", *children), ("annotations",))
    _require(type(value["schemaVersion"]) is int and value["schemaVersion"] == 2
             and value["mediaType"] == media, "unsupported OCI document")
    annotations = _annotations(value.get("annotations", {}))
    _require(not {KIND, REF} & annotations.keys(), "unexpected document association")


def _children(index, platform):
    _require(platform in ("linux/amd64", "linux/arm64"), "unsupported platform")
    _document(index, INDEX, ("manifests",))
    children = index["manifests"]
    _require(type(children) is list and 2 <= len(children) <= MAX_MEMBERS, "missing evidence")
    runnable, attestations, seen = [], [], set()
    for child in children:
        _descriptor(child, {MANIFEST}, allow_reference=True)
        _require(child["digest"] not in seen, "colliding child descriptors")
        seen.add(child["digest"])
        actual = child.get("platform", {})
        annotations = child.get("annotations", {})
        if actual.get("os") == "linux" and actual.get("architecture") == platform[6:]:
            _require(KIND not in annotations and REF not in annotations, "unexpected association")
            runnable.append(child)
        else:
            _require(actual == {"os": "unknown", "architecture": "unknown"}
                     and annotations.get(KIND) == "attestation-manifest", "unknown child")
            attestations.append(child)
    _require(len(runnable) == 1 and attestations, "expected one runnable and evidence")
    for child in attestations:
        _require(child["annotations"].get(REF) == runnable[0]["digest"], "unbound attestation")
    return runnable[0], attestations


def _read_tar(stream, physical_size):
    """Parse fixed headers only: never let tarfile consume extension payloads."""
    files, seen, payload, members = {}, set(), 0, 0
    _require(1024 <= physical_size <= MAX_TOTAL_BYTES and physical_size % 512 == 0,
             "archive byte limit or invalid TAR length")
    while stream.tell() < physical_size:
        header = stream.read(512)
        _require(len(header) == 512, "truncated header")
        if header == bytes(512):
            _require(physical_size - stream.tell() >= 512, "missing TAR terminator")
            while stream.tell() < physical_size:
                block = stream.read(min(65536, physical_size - stream.tell()))
                _require(block and not any(block), "nonzero TAR trailer")
            return files
        members += 1
        _require(members <= MAX_MEMBERS, "member limit")
        try:
            info = tarfile.TarInfo.frombuf(header, "utf-8", "strict")
        except (tarfile.HeaderError, UnicodeError, ValueError) as exc:
            raise ValueError("invalid TAR header") from exc
        _require(info.type in (tarfile.REGTYPE, tarfile.AREGTYPE, tarfile.DIRTYPE)
                 and not info.linkname and info.size >= 0, "unsupported TAR member/extension")
        _require(header[257:263] in (b"ustar\0", bytes(6)), "unsupported TAR format")
        raw_parts = []
        for field in (header[345:500], header[:100]):
            raw, _, padding = field.partition(b"\0")
            _require(not any(padding), "noncanonical TAR name padding")
            if raw:
                raw_parts.append(raw.decode("utf-8"))
        raw_name, name = "/".join(raw_parts), info.name
        _require(raw_name == name or (info.type == tarfile.DIRTYPE and raw_name == name + "/"),
                 "noncanonical TAR name")
        if info.type == tarfile.DIRTYPE:
            # TarInfo removes the conventional trailing slash from directory names.
            _require(name in ("blobs", "blobs/sha256") and info.size == 0, "invalid directory")
        else:
            _require(name in ("oci-layout", "index.json") or BLOB.fullmatch(name), "unsafe path")
        _require(name not in seen, "duplicate TAR path")
        seen.add(name)
        payload += info.size
        padded = (info.size + 511) // 512 * 512
        _require(payload <= MAX_TOTAL_BYTES and stream.tell() + padded <= physical_size,
                 "payload byte limit or truncated member")
        offset, remaining, hasher = stream.tell(), info.size, hashlib.sha256()
        while remaining:
            block = stream.read(min(65536, remaining))
            _require(block, "truncated payload")
            hasher.update(block)
            remaining -= len(block)
        padding = stream.read(padded - info.size)
        _require(not any(padding), "nonzero member padding")
        if info.type != tarfile.DIRTYPE:
            hashed = "sha256:" + hasher.hexdigest()
            if BLOB.fullmatch(name):
                _require(hashed == "sha256:" + name[13:], "blob digest mismatch")
            files[name] = (offset, info.size, hashed)
    raise ValueError("missing TAR terminator")


def inspect_archive(path, expected_root_digest, expected_platform):
    """Validate a complete graph; return metadata only, without rewriting bytes."""
    _require(type(expected_root_digest) is str and DIGEST.fullmatch(expected_root_digest),
             "invalid root digest")
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        before = os.fstat(stream.fileno())
        _require(stat.S_ISREG(before.st_mode), "archive must be regular")
        files = _read_tar(stream, before.st_size)
        visited, json_bytes = set(), 0

        def load(name, descriptor=None, raw=False):
            nonlocal json_bytes
            _require(name in files, "missing graph content")
            offset, size, hashed = files[name]
            if descriptor is not None:
                _require(size == descriptor["size"] and hashed == descriptor["digest"],
                         "descriptor content mismatch")
            json_bytes += size
            _require(json_bytes <= MAX_JSON_BYTES, "aggregate JSON byte limit")
            stream.seek(offset)
            data = stream.read(size)
            _require(len(data) == size and "sha256:" + hashlib.sha256(data).hexdigest() == hashed,
                     "archive changed during inspection")
            visited.add(name)
            return data if raw else _json(data)

        def resolve(descriptor, media, parse=True, allow_reference=False, raw=False):
            _descriptor(descriptor, media, allow_reference=allow_reference)
            name = "blobs/sha256/" + descriptor["digest"][7:]
            _require(name in files and files[name][1:] == (descriptor["size"], descriptor["digest"]),
                     "missing or mismatched blob")
            visited.add(name)
            return load(name, descriptor, raw=raw) if parse else None

        layout = load("oci-layout")
        _require(layout == {"imageLayoutVersion": "1.0.0"}, "unsupported layout")
        wrapper = load("index.json")
        _document(wrapper, INDEX, ("manifests",))
        _require(type(wrapper["manifests"]) is list and len(wrapper["manifests"]) == 1,
                 "ambiguous layout root")
        root = wrapper["manifests"][0]
        _descriptor(root, {INDEX})
        _require(root["digest"] == expected_root_digest and "platform" not in root,
                 "wrong root")
        root_data = resolve(root, {INDEX}, raw=True)
        index = _json(root_data)
        runnable, attestations = _children(index, expected_platform)
        predicates = set()
        for child in [runnable, *attestations]:
            manifest = resolve(child, {MANIFEST}, allow_reference=child is not runnable)
            _document(manifest, MANIFEST, ("config", "layers"))
            config = resolve(manifest["config"], {CONFIG})
            platform = child["platform"]
            _require(type(config) is dict and config.get("os") == platform["os"]
                     and config.get("architecture") == platform["architecture"], "config platform mismatch")
            layers, rootfs = manifest["layers"], config.get("rootfs")
            _require(type(layers) is list and type(rootfs) is dict and rootfs.get("type") == "layers"
                     and type(rootfs.get("diff_ids")) is list, "invalid image structure")
            _require(all(type(d) is str and DIGEST.fullmatch(d) for d in rootfs["diff_ids"]),
                     "invalid diff IDs")
            if child is runnable:
                _require(len(layers) == len(rootfs["diff_ids"]), "layer count mismatch")
                for layer in layers:
                    resolve(layer, LAYERS, parse=False)
            else:
                _require(layers and not rootfs["diff_ids"], "invalid attestation structure")
                for layer in layers:
                    statement = resolve(layer, {INTOTO})
                    _fields(statement, ("_type", "predicateType", "subject", "predicate"))
                    predicate = statement["predicateType"]
                    _require(statement["_type"] == "https://in-toto.io/Statement/v0.1"
                             and predicate in (SPDX, SLSA) and type(statement["predicate"]) is dict
                             and layer.get("annotations", {}).get("in-toto.io/predicate-type") == predicate,
                             "unsupported predicate or annotation mismatch")
                    subjects = statement["subject"]
                    _require(type(subjects) is list and len(subjects) == 1, "ambiguous subject")
                    _fields(subjects[0], ("name", "digest"))
                    _require(type(subjects[0]["name"]) is str and subjects[0]["digest"] ==
                             {"sha256": runnable["digest"][7:]}, "unbound statement")
                    predicates.add(predicate)
        _require(predicates == {SPDX, SLSA}, "missing SPDX/SLSA evidence")
        _require(visited == files.keys(), "unreferenced archive content")
        after = os.fstat(stream.fileno())
        _require((before.st_size, before.st_mtime_ns, before.st_ctime_ns) ==
                 (after.st_size, after.st_mtime_ns, after.st_ctime_ns), "archive changed")
        blobs = [v for k, v in files.items() if BLOB.fullmatch(k)]
        return dict(root=root, root_json=root_data.decode("utf-8"), index=index, platform=expected_platform,
                    blob_count=len(blobs), total_bytes=sum(v[1] for v in blobs))


def assemble_index(reports, annotations):
    """Bind metadata to exact root bytes, not authenticate roots or discarded bodies."""
    _require(type(reports) is list and len(reports) == 2, "two reports required")
    # Bound JSON shape/depth, then inspect original types (no key coercion).
    _encoded(reports)
    merged, children, platforms, digests, roots = {}, [], set(), set(), set()
    for report in reports:
        _fields(report, ("root", "root_json", "index", "platform", "blob_count", "total_bytes"))
        _require(type(report["blob_count"]) is int and 0 < report["blob_count"] <= MAX_MEMBERS
                 and type(report["total_bytes"]) is int and 0 < report["total_bytes"] <= MAX_TOTAL_BYTES,
                 "invalid report counters")
        _descriptor(report["root"], {INDEX})
        _require("platform" not in report["root"] and report["root"]["digest"] not in roots,
                 "unexpected or duplicate root")
        roots.add(report["root"]["digest"])
        _require(type(report["root_json"]) is str, "invalid raw root JSON")
        root_data = report["root_json"].encode("utf-8")
        _require(len(root_data) == report["root"]["size"]
                 and "sha256:" + hashlib.sha256(root_data).hexdigest() == report["root"]["digest"],
                 "root content mismatch")
        index = _json(root_data)
        _require(report["index"] == index, "report index/root mismatch")
        _children(index, report["platform"])
        _require(report["platform"] not in platforms, "duplicate platform")
        platforms.add(report["platform"])
        for child in index["manifests"]:
            _require(child["digest"] not in digests, "colliding descriptors")
            digests.add(child["digest"])
            children.append(child)
        for key, value in index.get("annotations", {}).items():
            _require(key not in merged or merged[key] == value, "root annotation conflict")
            merged[key] = value
    _require(not roots & digests, "root/child descriptor collision")
    _require(not {KIND, REF} & _annotations(annotations).keys(), "unexpected supplied association")
    for key, value in annotations.items():
        _require(key not in merged or merged[key] == value, "supplied annotation conflict")
        merged[key] = value
    return _encoded(dict(schemaVersion=2, mediaType=INDEX, manifests=children, annotations=merged))
