"""Final-index data contracts only: no registry, Docker, layers or signing proof."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from oci_layout_test import fixture, blob_path, digest, encoded, duplicate_json, INDEX, REF, KIND

SCRIPT = Path(__file__).resolve().parents[1] / ".github/scripts/oci-index.py"


class OciIndexTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name).resolve()
        spec = importlib.util.spec_from_file_location("oci_index_fixture", SCRIPT)
        self.api = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.api)
        self.reset()

    def reset(self):
        self.parents = [fixture(arch) for arch in ("amd64", "arm64")]
        self.amd, self.arm, self.index, self.meta = [self.base / n for n in ("amd", "arm", "index", "meta")]
        for path, graph in zip((self.amd, self.arm), self.parents):
            path.write_bytes(graph["files"][blob_path(graph["root"])])
        self.value = dict(schemaVersion=2, mediaType=INDEX,
                          manifests=copy.deepcopy([c for g in self.parents for c in g["index"]["manifests"]]),
                          annotations={"org.opencontainers.image.title": "Unicode é 🦀"})
        self.publish()

    def publish(self, raw=None):
        self.index.write_bytes(encoded(self.value) if raw is None else raw)
        self.root = digest(self.index.read_bytes())
        self.metadata = {"containerimage.descriptor": dict(mediaType=INDEX, digest=self.root,
                          size=self.index.stat().st_size), "image.name": "fixture:latest-release,fixture:0.9.0"}
        self.meta.write_bytes(encoded(self.metadata))

    def args(self):
        return [str(self.meta), str(self.amd), self.parents[0]["root"]["digest"],
                str(self.arm), self.parents[1]["root"]["digest"], str(self.index), self.root]

    def verify(self, args=None):
        # There is no authority to open layers, execute tools or contact services.
        with mock.patch("socket.socket", side_effect=AssertionError("network")), \
             mock.patch("subprocess.Popen", side_effect=AssertionError("process")), \
             mock.patch("tarfile.TarFile.extractall", side_effect=AssertionError("extraction")):
            self.api.verify(*(self.args() if args is None else args))

    def refused(self, args=None):
        with self.assertRaises((ValueError, OSError)):
            self.verify(args)

    def test_exact_union_succeeds_without_layer_reads(self):
        self.verify()

    def test_noncanonical_unicode_raw_json_is_not_reserialized_for_hashing(self):
        for path in (self.amd, self.arm):
            value = json.loads(path.read_bytes())
            value["annotations"]["unicode"] = "é 🦀"
            path.write_text(json.dumps(value, ensure_ascii=False, indent=3) + "\n")
        for path, graph in zip((self.amd, self.arm), self.parents):
            graph["root"]["digest"] = digest(path.read_bytes())
        self.publish(json.dumps(self.value, ensure_ascii=False, indent=4).encode() + b"\n")
        self.verify()

    def test_descriptor_and_object_key_order_do_not_change_union(self):
        self.value["manifests"].reverse()
        self.value["manifests"] = [dict(reversed(list(d.items()))) for d in self.value["manifests"]]
        self.publish()
        self.verify()

    def test_each_child_replacement_drop_duplicate_and_extra_is_refused(self):
        for position in range(4):
            for operation in ("replace", "drop", "duplicate", "extra"):
                with self.subTest(position=position, operation=operation):
                    self.reset()
                    children = self.value["manifests"]
                    if operation == "drop":
                        children.pop(position)
                    elif operation == "duplicate":
                        children.append(copy.deepcopy(children[position]))
                    elif operation == "extra":
                        child = copy.deepcopy(children[position])
                        child["digest"] = "sha256:" + "f" * 64
                        children.append(child)
                    else:
                        children[position]["digest"] = "sha256:" + "f" * 64
                    self.publish()
                    self.refused()

    def test_every_child_size_type_platform_annotation_and_media_change_is_refused(self):
        for position in range(4):
            for field, value in (("size", 1), ("size", 123.0), ("size", True),
                                 ("mediaType", INDEX), ("digest", "sha256:" + "f" * 64),
                                 ("platform", {"os": "linux", "architecture": "arm64", "variant": "v8"}),
                                 ("annotations", {}), ("annotations", {"new": "é"}),
                                 ("urls", ["https://invalid"])):
                with self.subTest(position=position, field=field, value=value):
                    self.reset()
                    self.value["manifests"][position][field] = value
                    self.publish()
                    self.refused()

    def test_same_numeric_value_float_and_boolean_sizes_are_not_equivalent(self):
        for value in (1.0, True):
            self.reset()
            parent = json.loads(self.amd.read_bytes())
            parent["manifests"][0]["size"] = 1
            self.amd.write_bytes(encoded(parent))
            self.parents[0]["root"]["digest"] = digest(self.amd.read_bytes())
            self.value["manifests"][0]["size"] = value
            self.publish()
            self.refused()

    def test_modified_evidence_association_cannot_be_authorized_even_if_union_matches(self):
        for field, value in ((REF, "sha256:" + "f" * 64), (KIND, "other")):
            self.reset()
            parent = json.loads(self.amd.read_bytes())
            parent["manifests"][1]["annotations"][field] = value
            self.amd.write_bytes(encoded(parent))
            self.parents[0]["root"]["digest"] = digest(self.amd.read_bytes())
            self.value["manifests"][:2] = parent["manifests"]
            self.publish()
            self.refused()

    def test_incorrect_parent_architecture_unknown_platform_and_missing_evidence_are_refused(self):
        for mutate in (lambda p: p["manifests"][0]["platform"].update(architecture="arm64"),
                       lambda p: p["manifests"][1]["platform"].update(os="linux"),
                       lambda p: p["manifests"].pop(),
                       lambda p: p["manifests"].append(copy.deepcopy(p["manifests"][0]))):
            self.reset()
            parent = json.loads(self.amd.read_bytes())
            mutate(parent)
            self.amd.write_bytes(encoded(parent))
            self.parents[0]["root"]["digest"] = digest(self.amd.read_bytes())
            self.value["manifests"] = parent["manifests"] + self.parents[1]["index"]["manifests"]
            self.publish()
            self.refused()

    def test_parent_path_or_approved_digest_swaps_are_refused(self):
        args = self.args()
        args[1], args[3] = args[3], args[1]
        self.refused(args)
        args[2], args[4] = args[4], args[2]
        self.refused(args)  # hashes match, architectures do not

    def test_raw_digest_binding_for_all_three_documents_is_mandatory(self):
        for field in (1, 3, 5):
            self.reset()
            path = Path(self.args()[field])
            path.write_bytes(path.read_bytes() + b"\n")
            self.refused()

    def test_all_expected_digest_arguments_require_strict_sha256_syntax(self):
        for position in (2, 4, 6):
            for bad in ("sha256:" + "A" * 64, "sha256:" + "a" * 64 + "\n", "sha512:" + "a" * 64, "", True):
                args = self.args()
                args[position] = bad
                self.refused(args)

    def test_metadata_missing_descriptor_and_wrong_native_key_are_refused(self):
        for value in ({}, {"containerimage.digest": self.root},
                      {"containerimage.descriptor": self.root}, {"containerimage.descriptor": None}):
            self.meta.write_bytes(encoded(value))
            self.refused()

    def test_metadata_descriptor_digest_media_size_and_type_are_exact(self):
        for field, value in (("digest", "sha256:" + "f" * 64), ("digest", True),
                             ("mediaType", "application/vnd.docker.distribution.manifest.list.v2+json"),
                             ("size", 0), ("size", -1), ("size", True),
                             ("size", float(self.index.stat().st_size))):
            metadata = copy.deepcopy(self.metadata)
            metadata["containerimage.descriptor"][field] = value
            self.meta.write_bytes(encoded(metadata))
            self.refused()
        for field in ("digest", "mediaType", "size"):
            metadata = copy.deepcopy(self.metadata)
            del metadata["containerimage.descriptor"][field]
            self.meta.write_bytes(encoded(metadata))
            self.refused()

    def test_bad_published_document_shape_is_refused(self):
        for field, value in (("schemaVersion", True), ("schemaVersion", 2.0),
                             ("mediaType", "wrong"), ("manifests", {}), ("annotations", {"bad": 1})):
            self.reset()
            self.value[field] = value
            self.publish()
            self.refused()

    def test_duplicate_keys_nonfinite_depth_and_malformed_json_are_refused_in_every_file(self):
        for role in ("meta", "amd", "arm", "index"):
            for bad in (b'{"x":NaN}', b'{"x":Infinity}', b'{"x":1e999}', b'{"x":1,"x":2}',
                        b"[" * 33 + b"0" + b"]" * 33, b"\xff", b"{} garbage"):
                with self.subTest(role=role, bad=bad[:40]):
                    self.reset()
                    path = getattr(self, role)
                    path.write_bytes(bad)
                    args = self.args()
                    if role in ("amd", "arm"):
                        args[2 if role == "amd" else 4] = digest(bad)
                    elif role == "index":
                        self.publish(bad)
                        args = self.args()
                    self.refused(args)
        # A duplicate valid OCI field must not be masked by a normal parser.
        for role in ("meta", "amd", "arm", "index"):
            self.reset()
            bad = duplicate_json(json.loads(getattr(self, role).read_bytes()))
            getattr(self, role).write_bytes(bad)
            args = self.args()
            if role in ("amd", "arm"):
                args[2 if role == "amd" else 4] = digest(bad)
            elif role == "index":
                self.publish(bad)
                args = self.args()
            self.refused(args)

    def test_size_limit_is_enforced_before_reading_each_file(self):
        for role in ("meta", "amd", "arm", "index"):
            self.reset()
            with getattr(self, role).open("r+b") as stream:
                stream.truncate(self.api.layout.MAX_JSON_BYTES + 1)
            self.refused()

    def test_missing_symlink_directory_fifo_and_symlink_parent_paths_are_refused(self):
        for position in (0, 1, 3, 5):
            for kind in ("missing", "symlink", "directory", "fifo", "symlink-parent"):
                with self.subTest(position=position, kind=kind):
                    bad = self.base / "bad"
                    if bad.is_symlink() or bad.is_file() or (bad.exists() and not bad.is_dir()):
                        bad.unlink()
                    elif bad.is_dir():
                        bad.rmdir()
                    args = self.args()
                    if kind == "symlink":
                        bad.symlink_to(args[position])
                    elif kind == "directory":
                        bad.mkdir()
                    elif kind == "fifo":
                        os.mkfifo(bad)
                    elif kind == "symlink-parent":
                        bad.symlink_to(self.base, target_is_directory=True)
                    args[position] = str(bad / Path(args[position]).name) if kind == "symlink-parent" else str(bad)
                    self.refused(args)

    def test_cli_is_silent_on_success_and_refuses_invalid_data_without_output(self):
        result = subprocess.run([sys.executable, "-I", str(SCRIPT), *self.args()], capture_output=True, timeout=5)
        self.assertEqual((result.returncode, result.stdout, result.stderr), (0, b"", b""))
        self.meta.write_bytes(b"{}")
        result = subprocess.run([sys.executable, "-I", str(SCRIPT), *self.args()], capture_output=True, timeout=5)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, b"")
        self.assertEqual(result.stderr, b"oci-index: validation failed\n")


if __name__ == "__main__":
    unittest.main()
