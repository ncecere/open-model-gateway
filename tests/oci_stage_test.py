"""Local fake-tool contracts only; not real ORAS/BuildKit/transport approval.
Run: python3 -m unittest discover -s tests -p oci_stage_test.py -v
"""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest import mock

from oci_layout_test import fixture, blob_path

ROOT = Path(__file__).resolve().parents[1]
IMAGE = "ghcr.io/ncecere/open-model-gateway"
COMMIT = "9b1fc815f6b7fba743f08b9b5c5846639cc11d3c"
TOKEN = "fixture-private-token"


class OciStageTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)
        scripts = self.base / ".github/scripts"
        scripts.mkdir(parents=True)
        for name in ("oci-stage.py", "oci-layout.py"):
            shutil.copyfile(ROOT / ".github/scripts" / name, scripts / name)
        self.script = scripts / "oci-stage.py"
        spec = importlib.util.spec_from_file_location("oci_stage_fixture", self.script)
        self.api = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.api)
        self.config = self.base / ".github/tools/oras.json"
        self.config.parent.mkdir()
        self.tool, self.log = self.base / "fake-oras", self.base / "calls.jsonl"
        self.install_tool()
        self.graph = fixture()
        self.archive = self.base / "archive.tar"
        self.write_archive()
        self.env = dict(os.environ, GITHUB_TOKEN=TOKEN, GITHUB_ACTOR="fixture-actor",
                        HTTPS_PROXY="https://credential.invalid", http_proxy="proxy-secret",
                        ALL_PROXY="proxy-secret", NO_PROXY="ambient", DOCKER_CONFIG="helper-secret",
                        DOCKER_AUTH_CONFIG="auth-secret", GH_TOKEN="gh-secret",
                        ACTIONS_ID_TOKEN_REQUEST_TOKEN="oidc-secret", PATH="/ambient/helpers",
                        HOME=str(self.base / "ambient-home"), TMPDIR=str(self.base / "ambient-tmp"))

    def install_tool(self, fail="", sleep=""):
        self.tool.write_text(f'''#!{sys.executable}
import json, os, pathlib, stat, sys, time
args = sys.argv[1:]
kind = args[0]
auth = pathlib.Path(args[args.index('--registry-config' if kind == 'login' else '--to-registry-config') + 1])
record = dict(argv=args, env=dict(os.environ), stdin=sys.stdin.read(), auth=str(auth),
              config=json.loads(auth.read_text()), mode=stat.S_IMODE(auth.stat().st_mode),
              parent_mode=stat.S_IMODE(auth.parent.stat().st_mode), cwd=os.getcwd())
with open({str(self.log)!r}, 'a') as log:
    log.write(json.dumps(record) + '\\n')
if kind == {sleep!r}:
    time.sleep(30)
print('untrusted-output fixture-private-token')
print('credentialed-error fixture-private-token', file=sys.stderr)
if kind == {fail!r}:
    sys.exit(9)
if kind == 'login':
    auth.write_text(json.dumps({{'auths': {{'ghcr.io': {{'auth': 'fixture-credential'}}}}}}))
''')
        self.tool.chmod(0o700)
        self.pin = dict(version="1.3.6", source_commit=COMMIT,
                        binary_sha256=hashlib.sha256(self.tool.read_bytes()).hexdigest())
        self.config.write_text(json.dumps(self.pin))

    def write_archive(self):
        with tarfile.open(self.archive, "w", format=tarfile.USTAR_FORMAT) as archive:
            for name, data in self.graph["files"].items():
                info = tarfile.TarInfo(name)
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))

    def args(self, **changes):
        values = dict(archive=str(self.archive), root=self.graph["root"]["digest"],
                      platform="linux/amd64", image=IMAGE, tool=str(self.tool))
        values.update(changes)
        return [part for key, value in values.items() for part in ("--" + key, value)]

    def invoke(self, args=None):
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.dict(os.environ, self.env, clear=True), contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = self.api.main(self.args() if args is None else args)
        self.assertNotIn(TOKEN, out.getvalue() + err.getvalue())
        return code, out.getvalue(), err.getvalue()

    def records(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def assert_cleanup(self):
        for record in self.records():
            for path in (record["auth"], record["env"]["HOME"], record["cwd"]):
                self.assertFalse(Path(path).exists(), path)

    def refused(self, args=None, calls=0):
        code, out, err = self.invoke(args)
        self.assertNotEqual(code, 0)
        self.assertEqual(out, "")
        self.assertTrue(err)
        self.assertEqual(len(self.records()), calls)
        self.assert_cleanup()

    def test_exact_digest_only_copy_for_both_platforms_and_repositories(self):
        before = self.archive.read_bytes()
        for arch, image in (("amd64", IMAGE), ("arm64", IMAGE + "-loadtest")):
            with self.subTest(arch=arch):
                self.graph = fixture(arch)
                self.write_archive()
                original = self.archive.read_bytes()
                code, out, err = self.invoke(self.args(platform="linux/" + arch, image=image))
                self.assertEqual((code, out, err), (0, self.graph["root"]["digest"] + "\n", ""))
                login, copy = self.records()[-2:]
                auth = login["auth"]
                self.assertEqual(login["argv"], ["login", "--registry-config", auth, "--username", "fixture-actor", "--password-stdin", "ghcr.io"])
                self.assertEqual(copy["argv"][:-2], ["cp", "--force", "--no-tty", "--from-oci-layout", "--to-registry-config", auth, "--to-distribution-spec", "v1.1-referrers-api"])
                self.assertEqual(copy["argv"][-1], image)
                source, root = copy["argv"][-2].rsplit("@", 1)
                self.assertEqual(root, self.graph["root"]["digest"])
                self.assertEqual(copy["stdin"], "")
                self.assertEqual(login["stdin"], TOKEN)
                self.assertEqual(login["config"], {"auths": {}})
                self.assertEqual(copy["config"], {"auths": {"ghcr.io": {"auth": "fixture-credential"}}})
                self.assertTrue(Path(source).is_absolute())
                self.assertEqual(self.archive.read_bytes(), original)
                for record in (login, copy):
                    self.assertEqual(record["mode"], 0o600)
                    self.assertEqual(record["parent_mode"], 0o700)
                    self.assertLessEqual(set(record["env"]), {"HOME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE"})
                    self.assertNotEqual(record["cwd"], str(self.base))
                self.assert_cleanup()
        self.assertTrue(before)

    def test_cli_success_is_only_the_root_digest(self):
        result = subprocess.run([sys.executable, str(self.script), *self.args()], env=self.env, capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, (self.graph["root"]["digest"] + "\n").encode())
        self.assertEqual(result.stderr, b"")
        self.assert_cleanup()

    def test_missing_production_pin_is_fail_closed(self):
        self.config.unlink()
        self.refused()

    def test_invalid_pin_fields_and_duplicate_keys_refuse_before_execution(self):
        for field, value in (("version", "1.3.5"), ("source_commit", "f" * 40),
                             ("binary_sha256", "f" * 64), ("binary_sha256", "XYZ"), ("extra", "override")):
            with self.subTest(field=field, value=value):
                self.config.write_text(json.dumps(dict(self.pin, **{field: value})))
                self.refused()
        self.config.write_text('{"version":"wrong",' + json.dumps(self.pin)[1:])
        self.refused()

    def test_tool_integrity_and_execute_permission_are_required(self):
        self.tool.write_text(self.tool.read_text() + "\n# changed")
        self.refused()
        self.install_tool()
        self.tool.chmod(0o600)
        self.refused()

    def test_symlink_directory_and_fifo_tool_and_config_are_refused(self):
        for path in (self.tool, self.config):
            saved = path.read_bytes()
            mode = path.stat().st_mode
            for kind in ("symlink", "directory", "fifo"):
                with self.subTest(path=path.name, kind=kind):
                    path.unlink()
                    target = self.base / "target"
                    target.write_bytes(saved)
                    if kind == "symlink":
                        path.symlink_to(target)
                    elif kind == "directory":
                        path.mkdir()
                    else:
                        os.mkfifo(path)
                    self.refused()
                    path.rmdir() if kind == "directory" else path.unlink()
                    path.write_bytes(saved)
                    path.chmod(mode)

    def test_invalid_archive_root_platform_and_evidence_never_authenticate(self):
        for change in ("tar", "blob", "evidence", "root", "platform"):
            with self.subTest(change=change):
                self.graph = fixture()
                if change == "blob":
                    self.graph["files"][blob_path(self.graph["descriptors"]["layer"])] = b"bad"
                if change == "evidence":
                    self.graph = fixture(edits={"spdx": lambda x: dict(x, subject=[])})
                self.write_archive()
                if change == "tar":
                    self.archive.write_bytes(b"not a tar")
                kwargs = {"root": "sha256:" + "0" * 64} if change == "root" else {"platform": "linux/arm64"} if change == "platform" else {}
                with mock.patch.dict(self.env, GITHUB_TOKEN="", GITHUB_ACTOR=""):
                    self.refused(self.args(**kwargs))

    def test_repositories_refs_urls_platforms_extra_args_and_abbreviations_are_rejected(self):
        for image in (IMAGE + ":tag", IMAGE + "@sha256:" + "f" * 64, "https://" + IMAGE, IMAGE + "/child", "ghcr.io/other/repo", "--help"):
            self.refused(self.args(image=image))
        self.refused(self.args(platform="linux/s390x"))
        self.refused(self.args(root="sha256:" + "A" * 64))
        self.refused(self.args() + ["--override", "anything"])
        self.refused(["--arch" if x == "--archive" else x for x in self.args()])

    def test_missing_credentials_never_execute_the_tool(self):
        for name in ("GITHUB_TOKEN", "GITHUB_ACTOR"):
            with mock.patch.dict(self.env, {name: ""}):
                self.refused()

    def test_login_and_copy_failures_are_redacted_and_cleaned(self):
        for kind, count in (("login", 1), ("cp", 2)):
            with self.subTest(kind=kind):
                self.log.unlink(missing_ok=True)
                self.install_tool(fail=kind)
                self.refused(calls=count)

    def test_login_and_copy_timeouts_are_bounded_redacted_and_cleaned(self):
        run = subprocess.run
        for kind, count in (("login", 1), ("cp", 2)):
            self.log.unlink(missing_ok=True)
            self.install_tool(sleep=kind)
            def bounded(*args, **kwargs):
                self.assertGreater(kwargs["timeout"], 0)
                self.assertLessEqual(kwargs["timeout"], 600)
                return run(*args, **dict(kwargs, timeout=0.2))
            with mock.patch.object(self.api.subprocess, "run", side_effect=bounded):
                self.refused(calls=count)


if __name__ == "__main__":
    unittest.main()
