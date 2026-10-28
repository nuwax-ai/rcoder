import copy
import json
import unittest
from unittest.mock import patch
from pathlib import Path
import isolated_ci
from isolated_ci import immutable_image, successful_case


class StrictCiReportTests(unittest.TestCase):
    def test_host_build_cache_stays_outside_uploaded_evidence(self):
        reports = Path("/owned/evidence")
        with patch.dict(isolated_ci.os.environ, {}, clear=True):
            self.assertEqual(isolated_ci.client_target_path(reports, "run"),
                             Path("/owned/rcoder-ci-client-target-run"))
        with patch.dict(isolated_ci.os.environ, {"CARGO_TARGET_DIR": "/owned/existing-target"}, clear=True):
            self.assertEqual(isolated_ci.client_target_path(reports, "run"),
                             Path("/owned/existing-target"))
        with patch.dict(isolated_ci.os.environ, {"CARGO_TARGET_DIR": "/owned/evidence/source/target"}, clear=True):
            with self.assertRaises(ValueError):
                isolated_ci.client_target_path(reports, "run")

    def test_zero_exit_never_hides_skips_missing_cases_or_cleanup_failure(self):
        good = {"results": [{"test": "recovery", "verdict": "pass", "errors": []}]}
        self.assertTrue(successful_case(0, good, "recovery"))
        bad = []
        for verdict in ["skip", "skipped", "aborted", "fail"]:
            row = copy.deepcopy(good)
            row["results"][0]["verdict"] = verdict
            bad.append(row)
        for rows in [[], good["results"] * 2]:
            bad.append({"results": rows})
        for field, value in [("test", "different-case"), ("errors", ["cleanup unconfirmed"])]:
            row = copy.deepcopy(good)
            row["results"][0][field] = value
            bad.append(row)
        for report in bad:
            with self.subTest(report=report):
                self.assertFalse(successful_case(0, report, "recovery"))
        self.assertFalse(successful_case(1, good, "recovery"))

    def test_only_exact_immutable_base_inputs_are_accepted(self):
        digest = "sha256:" + "a" * 64
        for image in [digest, "registry.example.test:5000/tooling/controller@" + digest]:
            self.assertEqual(immutable_image(image), image)
        for image in ["image:latest", "image:tag", "sha256:short", "sha256:" + "A" * 64,
                      "image@sha256:" + "a" * 63, "image;touch-file@" + digest,
                      "https://user:password@registry.example.test/image@" + digest]:
            with self.subTest(image=image), self.assertRaises(ValueError):
                immutable_image(image)


class FinalizationTests(unittest.TestCase):
    def finish_without_captured_controller(self, rows, *, before=None, discovery_error=None,
                                           baseline_confirmed=True, inspected_rows=None):
        source = {"origin_head": "test", "worktree_sha256": "frozen"}
        record = {"project": "owned-test", "run_id": "run", "case_id": "case",
                  "controller_creation_started": True, "baseline_confirmed": baseline_confirmed,
                  "error": "compose-up failed after Docker accepted Create"}
        before = {} if before is None else before
        calls, saved = [], []
        def command(*argv):
            if "inspect" in argv:
                return json.dumps(rows if inspected_rows is None else inspected_rows)
            if discovery_error:
                raise discovery_error
            if "compose-down" in calls:
                return ""
            self.assertIn("label=com.docker.compose.project=owned-test", argv)
            self.assertIn("--no-trunc", argv)
            return "\n".join(row["Id"] for row in rows)
        def execute(argv, name):
            self.assertNotIn("--volumes", argv)
            self.assertNotIn("-v", argv)
            calls.append(name)
        with patch.object(isolated_ci, "command", side_effect=command), \
             patch.object(isolated_ci, "private_children", return_value=[]), \
             patch.object(isolated_ci, "containers", return_value=before), \
             patch.object(isolated_ci, "source_identity", return_value=source):
            isolated_ci.finish_owned_run(record, Path("/owned"), ["docker", "compose"],
                                        None, before, source,
                                        execute,
                                        lambda: saved.append(copy.deepcopy(record)))
        self.assertEqual(record["error"], "compose-up failed after Docker accepted Create")
        self.assertFalse(record["success"])
        self.assertEqual(record["preexisting_containers_preserved"], baseline_confirmed)
        self.assertEqual(saved[-1], record)
        return record, calls

    @staticmethod
    def controller_row(cid="a" * 64):
        return {"Id": cid, "Config": {"Labels": {
            "com.docker.compose.project": "owned-test", "com.docker.compose.service": "rcoder",
            "rcoder.e2e.run": "run", "rcoder.e2e.case": "case"}},
            "State": {"Running": False, "Restarting": False, "Pid": 0}}

    def test_up_or_ps_failure_discovers_exact_owned_controller_and_finishes_cleanup(self):
        record, calls = self.finish_without_captured_controller([self.controller_row()])
        self.assertEqual(record.get("controller_id"), "a" * 64)
        self.assertIn("controller-stdout", calls)
        self.assertIn("compose-stop", calls)
        self.assertIn("compose-down", calls)
        self.assertTrue(record["cleanup_confirmed"])

    def test_discovery_needs_original_baseline_and_exact_inspected_id(self):
        for options in ({"baseline_confirmed": False},
                        {"inspected_rows": [self.controller_row("b" * 64)]}):
            with self.subTest(options=options):
                record, calls = self.finish_without_captured_controller([self.controller_row()], **options)
                self.assertEqual(calls, [])
                self.assertFalse(record["cleanup_confirmed"])
                self.assertTrue(record["cleanup_errors"])

    def test_discovery_never_stops_foreign_baseline_or_ambiguous_controllers(self):
        for label in ("com.docker.compose.project", "com.docker.compose.service",
                      "rcoder.e2e.run", "rcoder.e2e.case"):
            row = self.controller_row()
            row["Config"]["Labels"][label] = "foreign"
            with self.subTest(label=label):
                record, calls = self.finish_without_captured_controller([row])
                self.assertEqual(calls, [])
                self.assertFalse(record["cleanup_confirmed"])
                self.assertTrue(record["cleanup_errors"])
        for rows, before in (([self.controller_row()], {"a" * 64: {"running": False}}),
                             ([self.controller_row(), self.controller_row("b" * 64)], {})):
            with self.subTest(rows=len(rows), before=before):
                record, calls = self.finish_without_captured_controller(rows, before=before)
                self.assertEqual(calls, [])
                self.assertFalse(record["cleanup_confirmed"])
                self.assertTrue(record["cleanup_errors"])

    def test_absent_controller_or_failed_discovery_never_authorizes_blind_down(self):
        for error in (None, RuntimeError("Docker observations unavailable")):
            with self.subTest(error=error):
                record, calls = self.finish_without_captured_controller([], discovery_error=error)
                self.assertEqual(calls, [])
                self.assertFalse(record["cleanup_confirmed"])
                self.assertTrue(record["cleanup_errors"])

    def run_finalization(self, failure, primary_failure="original business failure"):
        source = {"origin_head": "test", "worktree_sha256": "frozen"}
        record = {"project": "owned-test", "cases_passed": True,
                  "baseline_confirmed": True, "error": primary_failure}
        calls, saved = [], []
        def execute(argv, name):
            calls.append(name)
            if name == failure:
                raise RuntimeError(name + " injected failure")
        def command(*argv):
            return '[{"State":{"Running":false,"Restarting":false,"Pid":0}}]' if "inspect" in argv else ""
        with patch.object(isolated_ci, "command", side_effect=command), \
             patch.object(isolated_ci, "validate_owned"), \
             patch.object(isolated_ci, "private_children", return_value=[]), \
             patch.object(isolated_ci, "containers", return_value={"existing": {"running": True}}), \
             patch.object(isolated_ci, "source_identity", return_value=source):
            isolated_ci.finish_owned_run(record, Path("/owned"), ["docker", "compose"],
                                        "captured-controller", {"existing": {"running": True}},
                                        source, execute, lambda: saved.append(copy.deepcopy(record)))
        return record, calls, saved

    def test_diagnostics_failure_still_attempts_owned_cleanup_and_preserves_primary_failure(self):
        record, calls, saved = self.run_finalization("controller-stdout")
        self.assertIn("compose-stop", calls)
        self.assertIn("compose-down", calls)
        self.assertTrue(record["cleanup_confirmed"])
        self.assertFalse(record["success"])
        self.assertEqual(record["error"], "original business failure")
        self.assertTrue(record["cleanup_errors"])
        self.assertEqual(saved[-1], record)

    def test_diagnostic_failure_with_all_business_cases_passed_is_still_failure(self):
        record, calls, saved = self.run_finalization("controller-stdout", primary_failure=None)
        self.assertTrue(record["cases_passed"])
        self.assertTrue(record["cleanup_confirmed"])
        self.assertTrue(record["preexisting_containers_preserved"])
        self.assertFalse(record["success"])
        self.assertEqual(saved[-1], record)

    def test_stop_failure_never_removes_unconfirmed_controller_or_reports_success(self):
        record, calls, saved = self.run_finalization("compose-stop")
        self.assertNotIn("compose-down", calls)
        self.assertFalse(record["cleanup_confirmed"])
        self.assertFalse(record["success"])
        self.assertTrue(record["preexisting_containers_preserved"])
        self.assertEqual(saved[-1], record)

    def test_all_case_passes_cannot_hide_changed_existing_resources(self):
        source = {"origin_head": "test", "worktree_sha256": "frozen"}
        record = {"project": "owned-test", "cases_passed": True, "baseline_confirmed": True}
        with patch.object(isolated_ci, "command") as command, \
             patch.object(isolated_ci, "validate_owned"), \
             patch.object(isolated_ci, "private_children", return_value=[]), \
             patch.object(isolated_ci, "containers", return_value={"existing": {"running": False}}), \
             patch.object(isolated_ci, "source_identity", return_value=source):
            command.side_effect = lambda *argv: '[{"State":{"Running":false,"Restarting":false,"Pid":0}}]' if "inspect" in argv else ""
            isolated_ci.finish_owned_run(record, Path("/owned"), ["docker", "compose"],
                                        "controller", {"existing": {"running": True}},
                                        source, lambda argv, name: None, lambda: None)
        self.assertTrue(record["cleanup_confirmed"])
        self.assertFalse(record["preexisting_containers_preserved"])
        self.assertFalse(record["success"])

    def test_stop_acknowledgement_without_actual_exit_does_not_authorize_down(self):
        source = {"origin_head": "test", "worktree_sha256": "frozen"}
        record = {"project": "owned-test", "cases_passed": True, "baseline_confirmed": True}
        calls = []
        with patch.object(isolated_ci, "command", return_value='[{"State":{"Running":true,"Restarting":false,"Pid":42}}]'), \
             patch.object(isolated_ci, "validate_owned"), \
             patch.object(isolated_ci, "private_children", return_value=[]), \
             patch.object(isolated_ci, "containers", return_value={}), \
             patch.object(isolated_ci, "source_identity", return_value=source):
            isolated_ci.finish_owned_run(record, Path("/owned"), ["docker", "compose"],
                                        "controller", {}, source,
                                        lambda argv, name: calls.append(name), lambda: None)
        self.assertIn("compose-stop", calls)
        self.assertNotIn("compose-down", calls)
        self.assertFalse(record["cleanup_confirmed"])
        self.assertFalse(record["success"])

    def test_late_owned_builder_at_stop_or_down_is_never_reported_as_clean(self):
        for window in ("stop", "down"):
            with self.subTest(window=window):
                source = {"origin_head": "test", "worktree_sha256": "frozen"}
                record = {"project": "owned-test", "cases_passed": True, "baseline_confirmed": True,
                          "error": "original business failure"}
                observed = [[], ["late-builder"]] if window == "stop" else [[], [], ["late-builder"]]
                calls = []
                with patch.object(isolated_ci, "command", side_effect=lambda *argv: '[{"State":{"Running":false,"Restarting":false,"Pid":0}}]' if "inspect" in argv else ""), \
                     patch.object(isolated_ci, "validate_owned"), \
                     patch.object(isolated_ci, "private_children", side_effect=observed), \
                     patch.object(isolated_ci, "containers", return_value={}), \
                     patch.object(isolated_ci, "source_identity", return_value=source):
                    isolated_ci.finish_owned_run(record, Path("/owned"), ["docker", "compose"],
                                                "controller", {}, source,
                                                lambda argv, name: calls.append(name), lambda: None)
                self.assertFalse(record["cleanup_confirmed"])
                self.assertFalse(record["success"])
                self.assertEqual(record["error"], "original business failure")
                self.assertTrue(record["cleanup_errors"])
                self.assertEqual("compose-down" in calls, window == "down")


if __name__ == "__main__":
    unittest.main()
