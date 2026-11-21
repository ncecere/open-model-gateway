#!/usr/bin/env python3
"""Unit tests for scripts/test-fast.py: the selection only, no cargo and no git."""
import importlib.util
import os
import pathlib
import unittest

for _name in [name for name in os.environ if name.startswith("GIT_")]:
    del os.environ[_name]

ROOT = pathlib.Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("test_fast", ROOT / "scripts" / "test-fast.py")
fast = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fast)

SRC = "apps/gateway/src/"
BIG = [f"management::tests::topic_{n}_tests::case" for n in range(fast.SMALL_MODULE + 5)]
TESTS = BIG + [
    "management::keys::tests::issues_a_key",
    "management::tests::key_rotation_tests::rotates",
    "management::tests::access_review_decisions_tests::decides",
    "management::tests::openapi_fixtures::setup::management_openapi_slice3_fixtures",
    "management::tests::openapi_fixtures::setup::management_openapi_slice3_fixtures_shard_2",
    "management::openapi::tests::management_openapi_artifact_matches_regeneration",
    "providers::anthropic::tests::encodes",
    "providers::bedrock::stream::tests::decodes",
    "governance::settlement::tests::settles",
    "governance::tests::db::admits",
    "source_rules::money_float::tests::no_float",
    "store::tests::migrates",
    "filestore::tests::format_v1_fixtures_from_earlier_releases_still_decrypt",
]


def select(paths, **kwargs):
    return fast.select(paths, TESTS, **kwargs)


class Selection(unittest.TestCase):
    def test_a_small_module_runs_whole_with_the_source_rules(self):
        chosen, _, _, scripts = select([SRC + "providers/anthropic.rs"])
        self.assertEqual(
            chosen,
            ["providers::anthropic::tests::encodes", "providers::bedrock::stream::tests::decodes",
             "source_rules::money_float::tests::no_float"],
        )
        self.assertEqual(scripts, ["test:structure"])

    def test_a_large_module_is_narrowed_to_the_file(self):
        chosen, _, _, _ = select([SRC + "management/keys.rs"])
        self.assertIn("management::keys::tests::issues_a_key", chosen)
        self.assertIn("management::tests::key_rotation_tests::rotates", chosen)
        self.assertNotIn(BIG[0], chosen)
        chosen, _, _, _ = select([SRC + "management/access_reviews.rs"])
        self.assertIn("management::tests::access_review_decisions_tests::decides", chosen)
        self.assertNotIn("management::keys::tests::issues_a_key", chosen)

    def test_a_large_module_file_with_no_tests_of_its_own_runs_the_module(self):
        chosen, reasons, _, _ = select([SRC + "management/mod.rs"])
        self.assertIn(BIG[0], chosen)
        self.assertTrue(any("nothing narrower" in reason for reason in reasons))

    def test_contract_fixtures_are_left_out_unless_the_contract_surface_changed(self):
        fixture = "management::tests::openapi_fixtures::setup::management_openapi_slice3_fixtures"
        chosen, reasons, _, _ = select([SRC + "management/mod.rs"])
        self.assertNotIn(fixture, chosen)
        self.assertNotIn(fixture + "_shard_2", chosen)
        self.assertTrue(any("left to CI" in reason for reason in reasons))
        for path in (SRC + "route_policy.rs", SRC + "management_openapi/responses.rs", "docs/openapi/management.openapi.json"):
            chosen, _, _, _ = select([path])
            self.assertIn(fixture, chosen, path)
            self.assertIn(fixture + "_shard_2", chosen, path)
            self.assertIn("management::openapi::tests::management_openapi_artifact_matches_regeneration", chosen, path)
        chosen, _, _, _ = select([SRC + "management/mod.rs"], contract=True)
        self.assertIn(fixture, chosen)

    def test_a_cross_cutting_file_selects_every_lib_test_but_the_fixtures(self):
        for path in (SRC + "store.rs", "Cargo.lock", "apps/gateway/enterprise_migrations/0082_x.sql", "crates/dbtest/src/lib.rs"):
            chosen, _, _, _ = select([path])
            self.assertEqual(len(chosen), len(TESTS) - 2, path)

    def test_the_mounted_openapi_module_is_found(self):
        chosen, _, _, _ = select([SRC + "management_openapi/validation.rs"], contract=False)
        self.assertNotEqual(len(chosen), len(TESTS))

    def test_other_areas_map_to_their_npm_scripts_and_integration_targets(self):
        chosen, _, targets, scripts = select(["apps/web/src/lib/api.ts", ".github/workflows/ci.yml",
                                              "deploy/helm/open-model-gateway/values.yaml",
                                              "apps/gateway/tests/startup.rs", "docs/roadmap.md"])
        self.assertEqual(chosen, [])
        self.assertEqual(targets, ["startup"])
        self.assertEqual(scripts, ["typecheck:web", "test:web", "test:supply-chain", "test:ci", "test:helm"])

    def test_all_selects_everything_but_the_fixtures(self):
        chosen, _, _, _ = select([], everything=True)
        self.assertEqual(len(chosen), len(TESTS) - 2)
        chosen, _, _, _ = select([], everything=True, contract=True)
        self.assertEqual(len(chosen), len(TESTS))

    def test_module_candidates_and_keywords(self):
        self.assertEqual(fast.module_candidates(SRC + "management/keys/issuance.rs")[:2],
                         ["management::keys::issuance", "management::keys"])
        self.assertIn("jobs::runner", fast.module_candidates(SRC + "jobs/runner/mod.rs"))
        self.assertIn("governance::settlement", fast.module_candidates(SRC + "governance/settlement_tests.rs"))
        self.assertEqual(fast.keyword(SRC + "management/access_reviews.rs"), "access_review")
        self.assertIsNone(fast.keyword(SRC + "management/mod.rs"))

    def test_only_openapi_fixtures_are_contract_fixtures(self):
        name = "filestore::tests::format_v1_fixtures_from_earlier_releases_still_decrypt"
        self.assertFalse(fast.is_contract_fixture(name))
        chosen, _, _, _ = select([SRC + "filestore.rs"])
        self.assertIn(name, chosen)
        chosen, _, _, _ = select([], everything=True)
        self.assertIn(name, chosen)

    def test_files_under_an_integration_target_select_it_and_shared_ones_are_named(self):
        _, reasons, targets, _ = select(["apps/gateway/tests/runtime_privileges/probes.rs",
                                         "apps/gateway/tests/common/mod.rs",
                                         "deploy/staging/runtime-grants.sql"])
        self.assertEqual(targets, ["runtime_privileges"])
        uncovered = [reason for reason in reasons if reason.startswith(fast.NOT_COVERED)]
        self.assertEqual(len(uncovered), 2)
        self.assertTrue(any("tests/common/mod.rs" in reason for reason in uncovered))
        self.assertTrue(any("runtime-grants.sql" in reason for reason in uncovered))

    def test_main_lists_the_tests_for_a_contract_document_or_build_script_change(self):
        import contextlib
        import io
        for path, expected in (("docs/openapi/management/operations.json", 3),
                               ("apps/gateway/build.rs", len(TESTS) - 2)):
            fast.changed_files = lambda base, path=path: [path]
            fast.list_tests = lambda jobs: TESTS
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(fast.main(["--plan"]), 0)
            self.assertIn(f"selected: {expected} of {len(TESTS)} lib tests", out.getvalue())

    def test_a_run_that_covers_nothing_does_not_say_passed(self):
        import contextlib
        import io
        fast.changed_files = lambda base: ["apps/gateway/tests/common/mod.rs"]
        fast.list_tests = lambda jobs: TESTS
        fast.run = lambda command: 0
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(fast.main([]), 0)
        self.assertIn("no test for this change", out.getvalue())
        self.assertNotIn("passed", out.getvalue())


if __name__ == "__main__":
    unittest.main()
