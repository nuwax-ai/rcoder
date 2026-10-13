import copy
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


SOURCE = Path(__file__).resolve().parents[2] / "tests-e2e/tools/official_pingap_lifecycle.py"
spec = importlib.util.spec_from_file_location("official_lifecycle_guard", SOURCE)
lifecycle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lifecycle)


class InitializationGuardTests(unittest.TestCase):
    def setUp(self):
        self.snapshot = {"supervisor_id": "native-a", "phase": "ready", "error": None}
        self.recovery = {"success": True, "code": "OK", "data": {
            "runtime_instance_id": "runtime-a", "kernel_protected": False,
            "owner_protected": False}}

    def initialized(self, snapshot=None, status=200, recovery=None):
        return lifecycle.owner_initialization_complete(
            self.snapshot if snapshot is None else snapshot, status,
            self.recovery if recovery is None else recovery, "runtime-a", "native-a")

    def test_identity_and_native_ready_do_not_replace_finished_recovery(self):
        self.assertFalse(self.initialized(status=503, recovery={"code": "ERR_INITIALIZING"}))
        snapshot = dict(self.snapshot, phase="reconciling")
        self.assertFalse(self.initialized(snapshot=snapshot))
        self.assertTrue(self.initialized())

    def test_recovery_required_and_other_errors_are_not_retried(self):
        for status, code in [(503, "ERR_RECOVERY_REQUIRED"), (403, "ERR_FORBIDDEN"),
                             (503, "ERR_PROTOCOL_UNSUPPORTED"), (503, "UNKNOWN")]:
            with self.subTest(code=code), self.assertRaises(AssertionError):
                self.initialized(status=status, recovery={"success": False, "code": code})
        for snapshot in [dict(self.snapshot, phase="recovery_required"),
                         dict(self.snapshot, error="cleanup unknown"),
                         dict(self.snapshot, phase="new-unknown-phase")]:
            with self.subTest(snapshot=snapshot), self.assertRaises(AssertionError):
                self.initialized(snapshot=snapshot)

    def test_protection_and_missing_proof_fail_instead_of_reporting_ready(self):
        for field in ["kernel_protected", "owner_protected"]:
            for value in [True, None, 0, "false"]:
                recovery = copy.deepcopy(self.recovery)
                recovery["data"][field] = value
                with self.subTest(field=field, value=value), self.assertRaises(AssertionError):
                    self.initialized(recovery=recovery)
            recovery = copy.deepcopy(self.recovery)
            del recovery["data"][field]
            with self.assertRaises(AssertionError):
                self.initialized(recovery=recovery)

    def test_both_http_and_native_incarnations_must_match_the_capture(self):
        with self.assertRaises(AssertionError):
            self.initialized(snapshot=dict(self.snapshot, supervisor_id="native-b"))
        recovery = copy.deepcopy(self.recovery)
        recovery["data"]["runtime_instance_id"] = "runtime-b"
        with self.assertRaises(AssertionError):
            self.initialized(recovery=recovery)

    def test_bounded_wait_only_reads_and_does_not_post_or_replace_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runner = lifecycle.Acceptance("builtin", "controlled-test")
            observations = []
            runner.identity = lambda timeout=None: {"runtime_instance_id": "runtime-a"}
            runner.native_status = lambda timeout=3: dict(
                self.snapshot, binding={"component": "app-cli", "resource": str(root)})
            responses = iter([(503, {"code": "ERR_INITIALIZING"}), (200, self.recovery)])
            def request(path, method="GET", **kwargs):
                observations.append((path, method))
                return next(responses)
            runner.request = request
            runner.check = lambda *args, **kwargs: observations.append((args[0], args[1]))
            with patch.object(lifecycle, "WORK", root), patch.object(lifecycle.time, "sleep"):
                result = runner.wait_owner_initialized({"runtime_instance_id": "runtime-a"}, seconds=1)
            self.assertEqual(result["runtime_instance_id"], "runtime-a")
            self.assertEqual(observations[:2], [("/v1/runtime/recovery", "GET")] * 2)
            self.assertEqual(len(observations), 3)

    def test_late_success_does_not_extend_the_shared_deadline(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runner = lifecycle.Acceptance("builtin", "controlled-test")
            runner.identity = lambda timeout=None: {"runtime_instance_id": "runtime-a"}
            runner.native_status = lambda timeout=3: dict(
                self.snapshot, binding={"component": "app-cli", "resource": str(root)})
            runner.request = lambda *args, **kwargs: (200, self.recovery)
            with patch.object(lifecycle, "WORK", root), patch.object(
                    lifecycle.time, "monotonic", side_effect=[0, 0.1, 0.2, 0.3, 2]):
                with self.assertRaisesRegex(AssertionError, "deadline exhausted"):
                    runner.wait_owner_initialized({"runtime_instance_id": "runtime-a"}, seconds=1)


class AdmissionDiagnosticTests(unittest.TestCase):
    def test_startup_rejection_is_recorded_with_status_and_body_without_replay(self):
        runner = lifecycle.Acceptance("supervisord", "fixture-source")
        runner.identity = lambda: {"runtime_instance_id": "runtime-a", "workspace_id": "workspace-a"}
        runner.save = lambda: None
        calls = []
        rejection = {"success": False, "code": "ERR_INVALID_STATE",
                     "message": "startup recovery has not completed", "data": None}
        def request(path, method="GET", body=None, **kwargs):
            calls.append((path, method))
            if path == "/v1/runtime/status":
                return 200, {"success": True, "data": {"revision": 0}}
            if path == "/v1/runtime/operations":
                return 409, rejection
            raise AssertionError("a refused mutation must not be replayed")
        runner.request = request
        with self.assertRaisesRegex(AssertionError, "restart accepted"):
            runner.operate("restart")
        failed = runner.evidence["checks"][-1]
        self.assertEqual(failed["status"], 409)
        self.assertEqual(failed["response"], rejection)
        self.assertFalse(failed["passed"])
        self.assertEqual(calls.count(("/v1/runtime/operations", "POST")), 1)


if __name__ == "__main__":
    unittest.main()
