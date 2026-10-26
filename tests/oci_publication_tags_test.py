"""Pure subprocess tag-policy tests; no registry, Docker or source image execution."""
from pathlib import Path
import subprocess
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / ".github/scripts/publication-tags.py"
IMAGE = "ghcr.io/ncecere/open-model-gateway"
SHA = "71e8cf24ac4378e5827a5106b2e46cc4a24abc64"


class PublicationTagsTests(unittest.TestCase):
    def invoke(self, tags, ref="refs/heads/main", sha=SHA, args=(), extra=None):
        env = dict(TAGS="\n".join(tags), GITHUB_REF=ref, GITHUB_SHA=sha)
        env.update(extra or {})
        return subprocess.run([sys.executable, "-I", str(HELPER), *args],
                              env=env, capture_output=True, text=True, check=False)

    def accepted(self, tags, **kwargs):
        result = self.invoke(tags, **kwargs)
        self.assertEqual(result.returncode, 0, result.stderr)

    def refused(self, tags, **kwargs):
        result = self.invoke(tags, **kwargs)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("publication-tags:", result.stderr)

    def test_main_exact_sha7(self):
        self.accepted([f"{IMAGE}:sha-{SHA[:7]}"])

    def test_stable_exact_three_tags_in_any_order(self):
        tags = [f"{IMAGE}:{tag}" for tag in ("v1.2.3", "v1.2", "latest-release")]
        self.accepted(tags, ref="refs/tags/v1.2.3")
        self.accepted(list(reversed(tags)), ref="refs/tags/v1.2.3")

    def test_prerelease_only_full_tag(self):
        for version in ("v1.2.3-rc.1", "v1.2.3-beta-2.0", "v1.2.3-0", "v1.2.3-RC.1"):
            with self.subTest(version=version):
                self.accepted([f"{IMAGE}:{version}"], ref=f"refs/tags/{version}")

    def test_current_numeric_version_components_are_preserved(self):
        self.accepted([f"{IMAGE}:v01.02.003", f"{IMAGE}:v01.02", f"{IMAGE}:latest-release"],
                      ref="refs/tags/v01.02.003")

    def test_wrong_repository(self):
        self.refused([f"{IMAGE}-other:sha-{SHA[:7]}"])
        self.refused([f"evil.invalid/ncecere/open-model-gateway:sha-{SHA[:7]}"])

    def test_main_cannot_request_latest_or_user_release(self):
        for tag in ("latest", "latest-release", "v1.0.0"):
            with self.subTest(tag=tag):
                self.refused([f"{IMAGE}:{tag}"])
                self.refused([f"{IMAGE}:sha-{SHA[:7]}", f"{IMAGE}:{tag}"])

    def test_prerelease_cannot_request_aliases(self):
        for alias in ("v1.2", "latest-release", "latest", "v1.2.3"):
            with self.subTest(alias=alias):
                self.refused([f"{IMAGE}:v1.2.3-rc.1", f"{IMAGE}:{alias}"], ref="refs/tags/v1.2.3-rc.1")

    def test_stable_requires_every_tag_and_no_extras(self):
        tags = [f"{IMAGE}:{tag}" for tag in ("v1.2.3", "v1.2", "latest-release")]
        for i in range(3):
            self.refused(tags[:i] + tags[i + 1:], ref="refs/tags/v1.2.3")
        for extra in ("v1", "v1.2.4", "latest"):
            self.refused(tags + [f"{IMAGE}:{extra}"], ref="refs/tags/v1.2.3")

    def test_duplicate_and_empty_tags(self):
        tag = f"{IMAGE}:sha-{SHA[:7]}"
        for tags in ([], [""], [tag, tag], [tag, ""], ["", tag], [tag, " "]):
            with self.subTest(tags=tags):
                self.refused(tags)

    def test_wrong_ref(self):
        for ref in ("", "main", "refs/heads/other", "refs/pull/1/merge", "refs/tags/v1.2.3", "refs/heads/main\n"):
            with self.subTest(ref=ref):
                self.refused([f"{IMAGE}:sha-{SHA[:7]}"], ref=ref)

    def test_invalid_version_refs(self):
        for version in ("v1.2", "v1.2.3-", "v1.2.3-rc..1", "v1.2.3-.rc", "v1.2.3-01", "v1.2.3-rc.01", "v1.2.3+build", "v1.2.3-rc/1", "v1.2.3\n", "v1.2.3-" + "a" * 128):
            with self.subTest(version=version):
                self.refused([f"{IMAGE}:{version}"], ref=f"refs/tags/{version}")

    def test_bad_sha(self):
        for sha in ("", SHA[:7], SHA + "0", "g" * 40, SHA.upper(), SHA + "\n"):
            with self.subTest(sha=sha):
                self.refused([f"{IMAGE}:sha-{SHA[:7]}"], sha=sha)

    def test_uppercase_or_changed_tags_are_not_normalized(self):
        for tag in (f"{IMAGE.upper()}:sha-{SHA[:7]}", f"{IMAGE}:SHA-{SHA[:7]}", f"{IMAGE}:sha-{SHA[:7].upper()}", f" {IMAGE}:sha-{SHA[:7]}"):
            with self.subTest(tag=tag):
                self.refused([tag])
        self.refused([f"{IMAGE}:V1.2.3"], ref="refs/tags/V1.2.3")

    def test_native_argument_and_shell_injection_refused(self):
        for value in ("--tag=evil.invalid/image:latest", f"{IMAGE}:sha-{SHA[:7]} --tag evil/image:latest", "$(touch /not-executed)", f"{IMAGE}:sha-{SHA[:7]};true"):
            with self.subTest(value=value):
                self.refused([value])
                self.refused([f"{IMAGE}:sha-{SHA[:7]}"], ref=f"refs/tags/{value}")

    def test_no_cli_or_ambient_source_overrides(self):
        tags = [f"{IMAGE}:sha-{SHA[:7]}"]
        for args in (("--image", "evil.invalid/image"), ("--ref", "refs/tags/v1.0.0"), ("--source", "other.py")):
            self.refused(tags, args=args)
        self.refused([f"evil.invalid/image:sha-{SHA[:7]}"], extra={"IMAGE": "evil.invalid/image", "REF": "refs/heads/main", "SOURCE_REVISION": SHA})
        self.accepted(tags, extra={"IMAGE": "evil.invalid/image", "REF": "refs/heads/other", "SOURCE_REVISION": "bad"})

    def test_missing_environment_is_refused(self):
        for key in ("TAGS", "GITHUB_REF", "GITHUB_SHA"):
            env = dict(TAGS=f"{IMAGE}:sha-{SHA[:7]}", GITHUB_REF="refs/heads/main", GITHUB_SHA=SHA)
            del env[key]
            result = subprocess.run([sys.executable, "-I", str(HELPER)], env=env,
                                    capture_output=True, text=True, check=False)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("publication-tags:", result.stderr)


if __name__ == "__main__":
    unittest.main()
