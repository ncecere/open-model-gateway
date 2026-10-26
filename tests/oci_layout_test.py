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

def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()

def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()

def duplicate_json(value):
    key = next(iter(value))
    return b"{" + encoded(key) + b":" + encoded(value[key]) + b"," + encoded(value)[1:]

def blob_path(desc):
    return "blobs/sha256/" + desc["digest"].split(":")[1]

def fixture(arch="amd64", edits=None):
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
        ("provenance", PROVENANCE, dict(builder=dict(id="https://example.invalid/builder"),
         buildType="https://example.invalid/build", invocation={}, metadata={}, materials=[])),
    ]:
        statement = dict(_type="https://in-toto.io/Statement/v0.1", predicateType=predicate_type,
                         subject=[dict(name="fixture", digest=dict(sha256=runnable["digest"][7:]))], predicate=predicate)
        desc = put(name, statement, INTOTO)
        desc["annotations"] = {"in-toto.io/predicate-type": predicate_type}
        statements.append(desc)
    att_config = put("att_config", dict(os="unknown", architecture="unknown", rootfs=dict(type="layers", diff_ids=[])), CONFIG)
    attestation = put("attestation", dict(schemaVersion=2, mediaType=MANIFEST,
                      config=att_config, layers=statements), MANIFEST)
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
