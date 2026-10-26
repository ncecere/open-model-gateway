"""Phase 1 contract: synthetic local OCI graphs, not real BuildKit/ORAS proof.
Run: python3 -m unittest discover -s tests -p oci_layout_test.py -v
"""
import copy
import gzip
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from contextlib import ExitStack
from unittest import mock

MODULE = Path(__file__).resolve().parents[1] / ".github/scripts/oci-layout.py"
INDEX = "application/vnd.oci.image.index.v1+json"
MANIFEST = "application/vnd.oci.image.manifest.v1+json"
CONFIG = "application/vnd.oci.image.config.v1+json"
LAYER = "application/vnd.oci.image.layer.v1.tar"
INTOTO = "application/vnd.in-toto+json"
REF = "vnd.docker.reference.digest"
KIND = "vnd.docker.reference.type"
SPDX = "https://spdx.dev/Document"
PROVENANCE = "https://slsa.dev/provenance/v0.2"
NATIVE_PROVENANCE = "https://slsa.dev/provenance/v1"
ARTIFACT = "application/vnd.docker.attestation.manifest.v1+json"
EMPTY = "application/vnd.oci.empty.v1+json"

def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()

def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()

def duplicate_json(value):
    key = next(iter(value))
    return b"{" + encoded(key) + b":" + encoded(value[key]) + b"," + encoded(value)[1:]

def blob_path(desc):
    return "blobs/sha256/" + desc["digest"].split(":")[1]

def fixture(arch="amd64", edits=None, native=False):
    """Rehash ancestors after semantic edits; no approved artifact/hash stand-ins."""
    edits, blobs, descriptors = edits or {}, {}, {}
    def put(name, value, media):
        value = edits.get(name, lambda x: x)(copy.deepcopy(value))
        data = value if isinstance(value, bytes) else encoded(value)
        desc = dict(mediaType=media, digest=digest(data), size=len(data))
        blobs[blob_path(desc)] = data
        descriptors[name] = desc
        return copy.deepcopy(desc)
    layer = put("layer", b"opaque uncompressed fixture layer\x00", LAYER)
    config = put("config", dict(os="linux", architecture=arch,
                 rootfs=dict(type="layers", diff_ids=[layer["digest"]])), CONFIG)
    runnable = put("runnable", dict(schemaVersion=2, mediaType=MANIFEST,
                   config=config, layers=[layer]), MANIFEST)
    statements = []
    for name, predicate_type, predicate in [
        ("spdx", SPDX, dict(spdxVersion="SPDX-2.3", dataLicense="CC0-1.0",
         SPDXID="SPDXRef-DOCUMENT", name="fixture", documentNamespace="https://example.invalid/spdx/fixture",
         creationInfo=dict(created="2026-01-01T00:00:00Z", creators=["Tool: fixture"]), packages=[])),
        ("provenance", NATIVE_PROVENANCE if native else PROVENANCE, dict(builder=dict(id="https://example.invalid/builder"),
         buildType="https://example.invalid/build", invocation={}, metadata={}, materials=[])),
    ]:
        if native and name == "provenance":
            predicate = dict(buildDefinition=dict(buildType="https://example.invalid/build",
                             externalParameters={}, internalParameters={}, resolvedDependencies=[]),
                             runDetails=dict(builder=dict(id="https://example.invalid/builder"), metadata={}))
        statement = dict(_type="https://in-toto.io/Statement/v1" if native else "https://in-toto.io/Statement/v0.1",
                         predicateType=predicate_type,
                         subject=[] if native else [dict(name="fixture", digest=dict(sha256=runnable["digest"][7:]))],
                         predicate=predicate)
        desc = put(name, statement, INTOTO)
        desc["annotations"] = {"in-toto.io/predicate-type": predicate_type}
        statements.append(desc)
    if native:
        att_config = put("att_config", {}, EMPTY)
        att_config["data"] = "e30="
    else:
        att_config = put("att_config", dict(os="unknown", architecture="unknown", rootfs=dict(type="layers", diff_ids=[])), CONFIG)
    attestation_value = dict(schemaVersion=2, mediaType=MANIFEST, config=att_config, layers=statements)
    if native:
        attestation_value.update(artifactType=ARTIFACT, subject=copy.deepcopy(runnable))
    attestation = put("attestation", attestation_value, MANIFEST)
    runnable.update(platform=dict(os="linux", architecture=arch), annotations={"fixture.role": "runnable"})
    attestation.update(platform=dict(os="unknown", architecture="unknown"),
                       annotations={KIND: "attestation-manifest", REF: runnable["digest"], "fixture.role": "evidence"})
    root_value = dict(schemaVersion=2, mediaType=INDEX, manifests=[runnable, attestation],
                      annotations={"fixture.shared": "retained"})
    root = put("root", root_value, INDEX)
    root["annotations"] = {"org.opencontainers.image.ref.name": "fixture-" + arch}
    wrapper = edits.get("wrapper", lambda x: x)(dict(schemaVersion=2, mediaType=INDEX, manifests=[root]))
    files = {"oci-layout": encoded({"imageLayoutVersion": "1.0.0"}), "index.json": encoded(wrapper), **blobs}
    try:
        index = json.loads(blobs[blob_path(root)])
    except ValueError:
        index = None  # Malformed root bytes must reach the inspector, not fail fixture construction.
    return dict(files=files, root=root, index=index, descriptors=descriptors)

class OciLayoutTests(unittest.TestCase):
    def setUp(self):
        self.stack = ExitStack()
        self.addCleanup(self.stack.close)
        for target in ("tarfile.TarFile.extract", "tarfile.TarFile.extractall", "socket.socket",
                       "socket.create_connection", "subprocess.Popen", "os.system"):
            self.stack.enter_context(mock.patch(target, side_effect=AssertionError("forbidden I/O: " + target)))
        self.tmp = Path(self.stack.enter_context(tempfile.TemporaryDirectory()))
        spec = importlib.util.spec_from_file_location("oci_layout", MODULE)
        self.api = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.api)  # Missing module/API is RED, never a skip.
        self.serial = 0
    def archive(self, graph, extra=()):
        self.serial += 1
        path = self.tmp / (str(self.serial) + ".tar")
        with tarfile.open(path, "w", format=tarfile.PAX_FORMAT) as tar:
            for directory in ("blobs", "blobs/sha256"):
                info = tarfile.TarInfo(directory)
                info.type = tarfile.DIRTYPE
                tar.addfile(info)
            for name, data in graph["files"].items():
                info = tarfile.TarInfo(name)
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))
            for info, data in extra:
                tar.addfile(info, io.BytesIO(data))
        return path
    def inspect(self, graph=None, arch="amd64", **kwargs):
        graph = fixture(arch) if graph is None else graph
        return self.api.inspect_archive(self.archive(graph), kwargs.get("root", graph["root"]["digest"]),
                                        kwargs.get("platform", "linux/" + arch))
    def rejected(self, graph, **kwargs):
        with self.assertRaises((ValueError, OSError)):
            self.inspect(graph, **kwargs)
    def reports(self, arm_edits=None):
        return [self.inspect(), self.inspect(fixture("arm64", arm_edits), arch="arm64")]
    def test_report_selects_recorded_root_and_preserves_children_without_layer_bodies(self):
        graph = fixture()
        path = self.archive(graph)
        before = path.read_bytes()
        report = self.api.inspect_archive(path, graph["root"]["digest"], "linux/amd64")
        self.assertEqual(report["root"], graph["root"])
        self.assertNotEqual(report["root"]["digest"], digest(graph["files"]["index.json"]))
        self.assertEqual(report["index"], graph["index"])
        self.assertEqual(report["platform"], "linux/amd64")
        blobs = {k: v for k, v in graph["files"].items() if k.startswith("blobs/")}
        self.assertEqual(report["blob_count"], len(blobs))
        self.assertEqual(report["total_bytes"], sum(map(len, blobs.values())))
        self.assertNotIn("opaque uncompressed fixture", repr(report))
        self.assertNotIn("SPDXRef-DOCUMENT", repr(report))
        self.assertEqual(path.read_bytes(), before)  # Includes exact attestation bytes.
    def test_report_preserves_exact_noncanonical_utf8_root_bytes_for_assembly(self):
        def noncanonical_root(value):
            value["annotations"]["fixture.unicode"] = "café"
            return (" \n" + json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
        graph = fixture(edits={"root": noncanonical_root})
        raw = graph["files"][blob_path(graph["root"])]
        self.assertNotEqual(raw, encoded(graph["index"]))
        report = self.inspect(graph)
        self.assertIn("root_json", report)
        self.assertIsInstance(report["root_json"], str)
        self.assertEqual(report["root_json"].encode("utf-8"), raw)
        self.assertEqual(len(raw), report["root"]["size"])
        self.assertEqual(digest(raw), report["root"]["digest"])
        reports = json.loads(json.dumps([report, self.inspect(arch="arm64")]))
        result = json.loads(self.api.assemble_index(reports, {}))
        self.assertEqual(result["manifests"], graph["index"]["manifests"] + reports[1]["index"]["manifests"])
        self.assertEqual(result["annotations"]["fixture.unicode"], "café")
    def test_native_empty_subject_reports_preserve_raw_blobs_roots_and_assembly(self):
        reports, graphs = [], []
        def noncanonical(value):
            return (" \n" + json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode()
        for arch in ("amd64", "arm64"):
            graph = fixture(arch, {"root": noncanonical}, native=True)
            path = self.archive(graph)
            before = path.read_bytes()
            report = self.api.inspect_archive(path, graph["root"]["digest"], "linux/" + arch)
            self.assertEqual(path.read_bytes(), before)
            self.assertEqual(report["root"], graph["root"])
            self.assertEqual(report["index"], graph["index"])
            self.assertEqual(report["root_json"].encode(), graph["files"][blob_path(graph["root"])])
            self.assertEqual(report["platform"], "linux/" + arch)
            blobs = {k: v for k, v in graph["files"].items() if k.startswith("blobs/")}
            self.assertEqual(report["blob_count"], len(blobs))
            self.assertEqual(report["total_bytes"], sum(map(len, blobs.values())))
            self.assertNotIn("SPDXRef-DOCUMENT", repr(report))
            self.assertNotIn("opaque uncompressed fixture", repr(report))
            reports.append(report)
            graphs.append(graph)
        original = copy.deepcopy(reports)
        assembled = json.loads(self.api.assemble_index(reports, {"fixture.native": "retained"}))
        self.assertEqual(assembled["manifests"], graphs[0]["index"]["manifests"] + graphs[1]["index"]["manifests"])
        self.assertEqual(assembled["annotations"], {"fixture.shared": "retained", "fixture.native": "retained"})
        self.assertEqual(reports, original)

    def test_native_named_exports_preserve_one_bound_statement_subject(self):
        for arch in ("amd64", "arm64"):
            bound = fixture(arch, native=True)["descriptors"]["runnable"]["digest"][7:]
            subject = [dict(name="pkg:docker/fixture@sha256:" + bound, digest={"sha256": bound})]
            edits = {name: lambda x: dict(x, subject=subject) for name in ("spdx", "provenance")}
            graph = fixture(arch, edits, native=True)
            path = self.archive(graph)
            before = path.read_bytes()
            report = self.api.inspect_archive(path, graph["root"]["digest"], "linux/" + arch)
            self.assertEqual(path.read_bytes(), before)
            for name in ("spdx", "provenance"):
                raw = graph["files"][blob_path(graph["descriptors"][name])]
                self.assertEqual(json.loads(raw)["subject"], subject)
            self.assertEqual(report["index"], graph["index"])

    def test_native_artifact_type_and_subject_are_both_required_and_exact(self):
        for field in ("artifactType", "subject"):
            with self.subTest(missing=field):
                self.rejected(fixture(edits={"attestation": lambda x: {k: v for k, v in x.items() if k != field}}, native=True))
        for value in (None, "", "application/vnd.oci.image.manifest.v1+json", ARTIFACT.upper(), [], {}):
            with self.subTest(artifactType=value):
                self.rejected(fixture(edits={"attestation": lambda x: dict(x, artifactType=value)}, native=True))
        # Without both keys the empty config / v1 statements must not become legacy.
        self.rejected(fixture(edits={"attestation": lambda x: {k: v for k, v in x.items() if k not in ("artifactType", "subject")}}, native=True))

    def test_native_manifest_subject_requires_exact_typed_runnable_descriptor(self):
        changes = [dict(digest=digest(b"wrong")), dict(mediaType=INDEX), dict(size=0),
                   dict(size=True), dict(size=False), dict(digest=None), dict(mediaType=None)]
        changes += [{field: value} for field, value in (
            ("urls", ["https://example.invalid/subject"]), ("data", "e30="),
            ("platform", {"os": "linux", "architecture": "amd64"}),
            ("annotations", {}), ("alias", "runnable"))]
        for change in changes:
            with self.subTest(change=change):
                self.rejected(fixture(edits={"attestation": lambda x: dict(x, subject=dict(x["subject"], **change))}, native=True))
        self.rejected(fixture(edits={"attestation": lambda x: dict(x, subject=dict(x["subject"], size=float(x["subject"]["size"])))}, native=True))
        for field in ("mediaType", "digest", "size"):
            with self.subTest(missing=field):
                self.rejected(fixture(edits={"attestation": lambda x: dict(x, subject={k: v for k, v in x["subject"].items() if k != field})}, native=True))
        for value in (None, [], "runnable"):
            with self.subTest(subject=value):
                self.rejected(fixture(edits={"attestation": lambda x: dict(x, subject=value)}, native=True))

    def test_native_empty_statement_subject_still_requires_docker_index_binding(self):
        changes = [dict(platform={"os": "linux", "architecture": "unknown"}),
                   dict(annotations={KIND: "attestation-manifest", REF: digest(b"wrong")}),
                   dict(annotations={KIND: "attestation-manifest"}),
                   dict(annotations={REF: "unused"}),
                   dict(annotations={KIND: "wrong", REF: "unused"})]
        for change in changes:
            with self.subTest(change=change):
                self.rejected(fixture(edits={"root": lambda x: dict(x, manifests=[x["manifests"][0], dict(x["manifests"][1], **change)])}, native=True))

    def test_native_empty_config_requires_exact_shape_inline_data_and_types(self):
        changes = [dict(mediaType=CONFIG), dict(digest=digest(b"wrong")), dict(size=1),
                   dict(size=2.0), dict(size=True), dict(data=""), dict(data="e30"),
                   dict(data="e30=\n"), dict(data="W10="), dict(data={}), dict(data=None),
                   dict(urls=["https://example.invalid/config"]), dict(platform={}), dict(annotations={})]
        for change in changes:
            with self.subTest(change=change):
                self.rejected(fixture(edits={"attestation": lambda x: dict(x, config=dict(x["config"], **change))}, native=True))
        for field in ("mediaType", "digest", "size", "data"):
            with self.subTest(missing=field):
                self.rejected(fixture(edits={"attestation": lambda x: dict(x, config={k: v for k, v in x["config"].items() if k != field})}, native=True))

    def test_native_empty_config_requires_stored_exact_empty_json_blob(self):
        for raw in (b"[]", b"{ }", b"{}\n", b'{"os":"unknown"}', b"null", b"{"):
            with self.subTest(raw=raw):
                self.rejected(fixture(edits={"att_config": lambda x: raw}, native=True))
        for corrupt in (False, True):
            graph = fixture(native=True)
            key = blob_path(graph["descriptors"]["att_config"])
            if corrupt:
                graph["files"][key] = b"[]"
            else:
                del graph["files"][key]
            with self.subTest(corrupt=corrupt):
                self.rejected(graph)

    def test_native_profile_is_never_allowed_on_runnable_manifest(self):
        self.rejected(fixture(edits={"runnable": lambda x: dict(x, artifactType=ARTIFACT,
                      subject=dict(mediaType=MANIFEST, digest=digest(b"wrong"), size=0))}, native=True))
        self.rejected(fixture(edits={"runnable": lambda x: dict(x, config=dict(x["config"], data="e30="))}, native=True))

    def test_native_attestation_layers_must_be_nonempty_and_local(self):
        for value in ([], None, {}, "layers"):
            with self.subTest(layers=value):
                self.rejected(fixture(edits={"attestation": lambda x: dict(x, layers=value)}, native=True))
        for change in (dict(urls=["https://example.invalid/statement"]), dict(data="e30="), dict(mediaType=LAYER)):
            with self.subTest(change=change):
                self.rejected(fixture(edits={"attestation": lambda x: dict(x, layers=[dict(d, **change) for d in x["layers"]])}, native=True))

    def test_native_statement_uri_predicate_object_and_annotation_are_strict(self):
        changes = [dict(_type="https://in-toto.io/Statement/v0.1"), dict(_type="https://in-toto.io/Statement/v2"),
                   dict(predicateType="https://example.invalid/predicate"), dict(predicateType=PROVENANCE),
                   dict(predicate=None), dict(predicate=[]), dict(predicate="opaque"), dict(extra="unsupported")]
        for name in ("spdx", "provenance"):
            for change in changes:
                with self.subTest(name=name, change=change):
                    self.rejected(fixture(edits={name: lambda x: dict(x, **change)}, native=True))
        for annotation in ({}, {"in-toto.io/predicate-type": PROVENANCE}, {"in-toto.io/predicate-type": SPDX}):
            with self.subTest(annotation=annotation):
                self.rejected(fixture(edits={"attestation": lambda x: dict(x, layers=[x["layers"][0], dict(x["layers"][1], annotations=annotation)])}, native=True))
        # Self-consistent layer annotation cannot authorize the legacy SLSA URI.
        self.rejected(fixture(edits={"provenance": lambda x: dict(x, predicateType=PROVENANCE),
                      "attestation": lambda x: dict(x, layers=[x["layers"][0], dict(x["layers"][1],
                      annotations={"in-toto.io/predicate-type": PROVENANCE})])}, native=True))

    def test_native_nonempty_statement_subject_is_never_ignored(self):
        bound = fixture(native=True)["descriptors"]["runnable"]["digest"][7:]
        valid = dict(name="fixture", digest={"sha256": bound})
        invalid = [None, {}, "subject", [dict(valid, name=1)], [dict(valid, digest={"sha256": "0" * 64})],
                   [dict(valid, digest={"sha256": bound, "sha512": "other"})],
                   [dict(valid, urls=["https://example.invalid/subject"])], [valid, valid],
                   [{"digest": valid["digest"]}], [{"name": "fixture"}]]
        for name in ("spdx", "provenance"):
            for subjects in invalid:
                with self.subTest(name=name, subject=subjects):
                    self.rejected(fixture(edits={name: lambda x: dict(x, subject=subjects)}, native=True))

    def test_legacy_statements_still_require_one_nonempty_subject_and_legacy_uris(self):
        for name in ("spdx", "provenance"):
            for change in (dict(subject=[]), dict(_type="https://in-toto.io/Statement/v1"),
                           dict(predicateType=NATIVE_PROVENANCE)):
                with self.subTest(name=name, change=change):
                    self.rejected(fixture(edits={name: lambda x: dict(x, **change)}))
        self.rejected(fixture(edits={"provenance": lambda x: dict(x, predicateType=NATIVE_PROVENANCE),
                      "attestation": lambda x: dict(x, layers=[x["layers"][0], dict(x["layers"][1],
                      annotations={"in-toto.io/predicate-type": NATIVE_PROVENANCE})])}))

    def test_native_profile_requires_both_spdx_and_v1_provenance(self):
        # Remove each statement and its blob so no-orphan checking cannot mask missing evidence.
        for position, name in ((0, "spdx"), (1, "provenance")):
            graph = fixture(edits={"attestation": lambda x: dict(x, layers=[x["layers"][1 - position]])}, native=True)
            del graph["files"][blob_path(graph["descriptors"][name])]
            with self.subTest(missing=name):
                self.rejected(graph)

    def test_mixed_native_and_legacy_attestations_in_one_index_are_refused(self):
        legacy = fixture()
        graph = fixture(edits={"root": lambda x: dict(x, manifests=x["manifests"] + [legacy["index"]["manifests"][1]])}, native=True)
        for name in ("spdx", "provenance", "att_config", "attestation"):
            path = blob_path(legacy["descriptors"][name])
            graph["files"][path] = legacy["files"][path]
        self.rejected(graph)

    def test_native_missing_corrupt_descendants_and_orphans_are_refused(self):
        for name in ("layer", "config", "runnable", "spdx", "provenance", "attestation", "root"):
            for corrupt in (False, True):
                graph = fixture(native=True)
                key = blob_path(graph["descriptors"][name])
                if corrupt:
                    graph["files"][key] = b"!" * len(graph["files"][key])
                else:
                    del graph["files"][key]
                with self.subTest(name=name, corrupt=corrupt):
                    self.rejected(graph)
        graph = fixture(native=True)
        graph["files"]["blobs/sha256/" + hashlib.sha256(b"orphan").hexdigest()] = b"orphan"
        self.rejected(graph)

    def test_native_member_byte_json_and_depth_limits_are_enforced(self):
        for name in ("MAX_MEMBERS", "MAX_TOTAL_BYTES", "MAX_JSON_BYTES", "MAX_DEPTH"):
            with self.subTest(limit=name), mock.patch.object(self.api, name, 1):
                self.rejected(fixture(native=True))

    def test_wrong_root_platform_and_config_architecture_are_refused(self):
        for kwargs in ({"root": digest(b"not the root")}, {"platform": "linux/arm64"}):
            with self.subTest(kwargs=kwargs):
                self.rejected(fixture(), **kwargs)
        for field, value in (("architecture", "arm64"), ("os", "windows")):
            with self.subTest(field=field):
                self.rejected(fixture(edits={"config": lambda x: dict(x, **{field: value})}))
    def test_missing_corrupt_and_wrong_size_descendants_are_refused(self):
        for name in ("layer", "config", "spdx", "provenance", "attestation", "att_config", "runnable", "root"):
            for corrupt in (False, True):
                with self.subTest(name=name, corrupt=corrupt):
                    graph = fixture()
                    key = blob_path(graph["descriptors"][name])
                    if corrupt:
                        graph["files"][key] = b"!" * len(graph["files"][key])
                    else:
                        del graph["files"][key]
                    self.rejected(graph)
        self.rejected(fixture(edits={"runnable": lambda x: dict(x, layers=[dict(x["layers"][0], size=1)])}))
    def test_invalid_and_duplicate_key_json_are_refused(self):
        for name in ("root", "config", "att_config", "runnable", "attestation", "spdx", "provenance"):
            for change in (lambda x: b"{", duplicate_json):
                with self.subTest(name=name, change=change):
                    self.rejected(fixture(edits={name: change}))
        for name in ("index.json", "oci-layout"):
            for change in (lambda x: b"{", duplicate_json):
                graph = fixture()
                graph["files"][name] = change(json.loads(graph["files"][name]))
                self.rejected(graph)
    def test_unknown_nodes_and_unbound_attestations_are_refused(self):
        changes = [lambda x: dict(x, manifests=[dict(x["manifests"][0], platform={"os": "unknown", "architecture": "unknown"}), x["manifests"][1]]),
                   lambda x: dict(x, manifests=[x["manifests"][0], dict(x["manifests"][1], annotations={})]),
                   lambda x: dict(x, manifests=[x["manifests"][0], dict(x["manifests"][1], annotations={**x["manifests"][1]["annotations"], KIND: "unsupported"})]),
                   lambda x: dict(x, manifests=[x["manifests"][0], dict(x["manifests"][1], annotations={KIND: "attestation-manifest", REF: digest(b"unbound")})]),
                   lambda x: dict(x, manifests=x["manifests"] + [dict(x["manifests"][0], mediaType="application/x-unsupported")])]
        for i, change in enumerate(changes):
            with self.subTest(case=i):
                self.rejected(fixture(edits={"root": change}))
        for name in ("spdx", "provenance"):
            self.rejected(fixture(edits={name: lambda x: dict(x, subject=[dict(name="wrong", digest=dict(sha256=hashlib.sha256(b"wrong").hexdigest()))])}))
    def test_external_urls_foreign_and_nondistributable_layers_are_refused(self):
        for change in ({"urls": ["https://example.invalid/blob"]},
                       {"mediaType": "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip"},
                       {"mediaType": "application/vnd.oci.image.layer.nondistributable.v1.tar"}):
            with self.subTest(change=change):
                self.rejected(fixture(edits={"runnable": lambda x: dict(x, layers=[dict(x["layers"][0], **change)])}))
        for node, field in (("root", "manifests"), ("attestation", "layers")):
            self.rejected(fixture(edits={node: lambda x: dict(x, **{field: [dict(d, urls=["https://example.invalid/blob"]) for d in x[field]]})}))
    def test_ambiguous_wrapper_and_unreferenced_blobs_are_refused(self):
        self.rejected(fixture(edits={"wrapper": lambda x: dict(x, manifests=x["manifests"] * 2)}))
        graph = fixture()
        graph["files"]["blobs/sha256/" + hashlib.sha256(b"orphan").hexdigest()] = b"orphan"
        self.rejected(graph)
    def test_duplicate_unsafe_and_normalized_alias_paths_are_refused(self):
        graph = fixture()
        layer_path = blob_path(graph["descriptors"]["layer"])
        for name in ("index.json", "/index.json", "../index.json", "blobs/../index.json", "./index.json", layer_path.replace("blobs/", "blobs//")):
            with self.subTest(name=name):
                data = graph["files"][layer_path if "//" in name else "index.json"]
                info = tarfile.TarInfo(name)
                info.size = len(data)
                with self.assertRaises((ValueError, OSError)):
                    self.api.inspect_archive(self.archive(graph, [(info, data)]), graph["root"]["digest"], "linux/amd64")
    def test_links_special_and_sparse_members_are_refused(self):
        for kind in (tarfile.SYMTYPE, tarfile.LNKTYPE, tarfile.FIFOTYPE, tarfile.CHRTYPE, tarfile.BLKTYPE, tarfile.GNUTYPE_SPARSE):
            with self.subTest(kind=kind):
                graph = fixture()
                info = tarfile.TarInfo("unexpected")
                info.type, info.linkname = kind, "index.json" if kind in (tarfile.SYMTYPE, tarfile.LNKTYPE) else ""
                path = self.archive(graph, [(info, b"")])
                with self.assertRaises((ValueError, OSError)):
                    self.api.inspect_archive(path, graph["root"]["digest"], "linux/amd64")
    def test_archive_must_be_regular_and_not_a_symlink(self):
        graph = fixture()
        link = self.tmp / "link.tar"
        link.symlink_to(self.archive(graph))
        for path in (link, self.tmp):
            with self.subTest(path=path), self.assertRaises((ValueError, OSError)):
                self.api.inspect_archive(path, graph["root"]["digest"], "linux/amd64")
    def test_compressed_archive_is_not_accepted_as_uncompressed_handoff_data(self):
        graph = fixture()
        path = self.archive(graph)
        path.write_bytes(gzip.compress(path.read_bytes(), mtime=0))
        with self.assertRaises((ValueError, OSError)):
            self.api.inspect_archive(path, graph["root"]["digest"], "linux/amd64")
    def test_member_byte_json_and_depth_limits_are_enforced(self):
        for name in ("MAX_MEMBERS", "MAX_TOTAL_BYTES", "MAX_JSON_BYTES", "MAX_DEPTH"):
            with self.subTest(limit=name), mock.patch.object(self.api, name, 1):
                self.rejected(fixture())
    def test_assembly_flattens_all_children_and_keeps_compatible_annotations(self):
        reports = self.reports({"root": lambda x: dict(x, annotations={**x["annotations"], "fixture.arm": "retained-too"})})
        annotations = {"org.opencontainers.image.version": "fixture", "fixture.shared": "retained"}
        before = copy.deepcopy(reports)
        data = self.api.assemble_index(reports, annotations)
        self.assertIsInstance(data, bytes)
        result = json.loads(data)
        self.assertEqual(result, dict(schemaVersion=2, mediaType=INDEX,
                         manifests=reports[0]["index"]["manifests"] + reports[1]["index"]["manifests"],
                         annotations={"fixture.shared": "retained", "fixture.arm": "retained-too", **annotations}))
        self.assertEqual(data, encoded(result))
        self.assertEqual(data, self.api.assemble_index(copy.deepcopy(reports), dict(annotations)))
        self.assertEqual(reports, before)
    def test_assembly_rejects_missing_duplicate_and_colliding_descriptors(self):
        reports = self.reports()
        for invalid in ([], reports[:1], [reports[0], reports[0]], reports + [reports[1]]):
            with self.subTest(count=len(invalid)), self.assertRaises((ValueError, OSError)):
                self.api.assemble_index(invalid, {})
        for change in (lambda r: r[1]["index"]["manifests"][0].update(digest=r[0]["index"]["manifests"][0]["digest"]),
                       lambda r: r[1]["index"]["manifests"][1]["annotations"].update({REF: r[0]["index"]["manifests"][0]["digest"]}),
                       lambda r: r[1]["index"]["manifests"][1].update(mediaType="application/x-unsupported")):
            altered = copy.deepcopy(reports)
            change(altered)
            with self.assertRaises((ValueError, OSError)):
                self.api.assemble_index(altered, {})
    def test_assembly_rejects_consistently_swapped_children_under_unchanged_root(self):
        reports = self.reports()
        alternate = self.inspect(fixture(edits={"layer": lambda b: b"X" * len(b)}))
        self.assertNotEqual(reports[0]["root"]["digest"], alternate["root"]["digest"])
        for counter in ("blob_count", "total_bytes"):
            self.assertEqual(reports[0][counter], alternate[counter])
        original = copy.deepcopy(reports[0])
        reports[0]["index"] = alternate["index"]
        self.assertEqual(reports[0]["root"], original["root"])
        with self.assertRaises(ValueError):
            self.api.assemble_index(reports, {})
    def test_assembly_requires_raw_root_json_string(self):
        reports = self.reports()
        for value in (None, b"{}", {}, 1):
            with self.subTest(value=value):
                altered = copy.deepcopy(reports)
                altered[0]["root_json"] = value
                with self.assertRaises(ValueError):
                    self.api.assemble_index(altered, {})
        reports[0].pop("root_json", None)
        with self.assertRaises(ValueError):
            self.api.assemble_index(reports, {})
    def test_assembly_checks_raw_root_bytes_not_reserialized_index(self):
        reports = self.reports()
        # Same parsed index and byte length, but a different order/whitespace digest.
        raw = encoded(reports[0]["index"])
        reordered = json.dumps(dict(reversed(list(reports[0]["index"].items()))), separators=(",", ":")).encode()
        self.assertEqual(len(raw), len(reordered))
        self.assertNotEqual(raw, reordered)
        reports[0]["root_json"] = reordered.decode()
        reports[0]["root"].update(digest=digest(raw), size=len(raw))
        with self.assertRaises(ValueError):
            self.api.assemble_index(reports, {})
    def test_assembly_rejects_raw_root_descriptor_size_and_digest_mismatches(self):
        reports = self.reports()
        for change in ({"size": reports[0]["root"]["size"] + 1}, {"digest": digest(b"wrong root")}):
            with self.subTest(change=change):
                altered = copy.deepcopy(reports)
                altered[0]["root"].update(change)
                with self.assertRaises(ValueError):
                    self.api.assemble_index(altered, {})
    def test_assembly_strictly_parses_digest_verified_raw_root_json(self):
        reports = self.reports()
        for raw in (b"{", duplicate_json(reports[0]["index"]),
                    encoded(reports[0]["index"]).replace(b'"schemaVersion":2', b'"schemaVersion":NaN')):
            with self.subTest(raw=raw):
                altered = copy.deepcopy(reports)
                altered[0]["root_json"] = raw.decode()
                altered[0]["root"].update(digest=digest(raw), size=len(raw))
                with self.assertRaises(ValueError):
                    self.api.assemble_index(altered, {})
    def test_assembly_rejects_raw_root_annotation_changes_hidden_from_index(self):
        reports = self.reports()
        root_index = copy.deepcopy(reports[0]["index"])
        root_index["annotations"]["fixture.shared"] = "different"
        raw = encoded(root_index)
        reports[0]["root_json"] = raw.decode()
        reports[0]["root"].update(digest=digest(raw), size=len(raw))
        with self.assertRaises(ValueError):
            self.api.assemble_index(reports, {})
    def test_assembly_preserves_aggregate_report_json_and_depth_limits(self):
        reports = self.reports()
        for name, limit in (("MAX_JSON_BYTES", len(encoded(reports)) - 1), ("MAX_DEPTH", 1)):
            with self.subTest(limit=name), mock.patch.object(self.api, name, limit):
                with self.assertRaises(ValueError):
                    self.api.assemble_index(reports, {})
    def test_assembly_never_silently_overrides_root_annotations(self):
        reports = self.reports({"root": lambda x: dict(x, annotations={"fixture.shared": "conflict"})})
        with self.assertRaises((ValueError, OSError)):
            self.api.assemble_index(reports, {})
        with self.assertRaises((ValueError, OSError)):
            self.api.assemble_index(self.reports(), {"fixture.shared": "override"})

if __name__ == "__main__":
    unittest.main()
