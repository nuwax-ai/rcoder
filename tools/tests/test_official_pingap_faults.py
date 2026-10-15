"""Transport integrity checks; these are not real-supervisord E2E evidence."""
import importlib.util
import io
from pathlib import Path
import sys
import unittest
import xmlrpc.client

TOOLS = Path(__file__).resolve().parents[2] / "tests-e2e/tools"
sys.path.insert(0, str(TOOLS))
try:
    spec = importlib.util.spec_from_file_location("official_pingap_faults_guard", TOOLS / "official_pingap_faults.py")
    faults = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(faults)
finally:
    sys.path.pop(0)


class ForwardedHttpIntegrityTests(unittest.TestCase):
    def test_real_request_bytes_and_target_are_preserved(self):
        body = xmlrpc.client.dumps(("app-pingap",), methodname="supervisor.stopProcessGroup").encode()
        original = b"POST /RPC2 HTTP/1.1\r\nContent-Length: " + str(len(body)).encode() + b"\r\nConnection: close\r\n\r\n" + body
        forwarded, parsed = faults.read_http(io.BytesIO(original))
        self.assertEqual(forwarded, original)
        self.assertEqual(xmlrpc.client.loads(parsed), (("app-pingap",), "supervisor.stopProcessGroup"))

    def test_upstream_fault_is_forwarded_without_fabricating_success(self):
        body = xmlrpc.client.dumps(xmlrpc.client.Fault(70, "NOT_RUNNING")).encode()
        original = b"HTTP/1.1 200 OK\r\nContent-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body
        forwarded, parsed = faults.read_http(io.BytesIO(original))
        self.assertEqual(forwarded, original)
        with self.assertRaises(xmlrpc.client.Fault) as raised:
            xmlrpc.client.loads(parsed)
        self.assertEqual(raised.exception.faultCode, 70)

    def test_partial_response_is_rejected(self):
        with self.assertRaisesRegex(AssertionError, "incomplete.*body"):
            faults.read_http(io.BytesIO(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\npartial"))

    def test_missing_or_duplicate_length_cannot_invent_reply_boundaries(self):
        for wire in (b"HTTP/1.1 200 OK\r\n\r\n",
                     b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n"):
            with self.subTest(wire=wire), self.assertRaisesRegex(AssertionError, "Content-Length"):
                faults.read_http(io.BytesIO(wire))

class NativeStopTerminalAndPhysicalEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.captured = {"supervisor_id": "native-owner", "binding": {"component": "app-cli", "resource": "/workspace"}}
        self.reply = dict(self.captured, phase="stopped", intent="stopped", generation=None,
                          operation_id="original-stop", error=None)
        self.current = dict(self.captured, phase="ready", intent="stopped",
                            generation="new-management-worker", operation_id=None, error=None)
        self.groups = [{"group": "app-cli", "statename": "RUNNING", "pid": 42}]

    def complete(self, reply=None, current=None, groups=None):
        return faults.native_stop_cleanup_complete(self.captured, "original-stop",
            self.reply if reply is None else reply,
            self.current if current is None else current,
            self.groups if groups is None else groups)

    def test_completed_stop_allows_the_same_owner_new_management_worker(self):
        self.assertTrue(self.complete())

    def test_wrong_request_or_nonterminal_reply_cannot_prove_original_cleanup(self):
        for changed in ({"operation_id": "another-stop"}, {"phase": "ready"},
                        {"generation": "old-live-business"}, {"error": "cleanup failed"}):
            with self.subTest(changed=changed):
                self.assertFalse(self.complete(reply=dict(self.reply, **changed)))

    def test_terminal_metadata_requires_actual_business_and_proxy_group_absence(self):
        for group in ("app-pingap", "app-svc-web", "app-svc-background-worker"):
            with self.subTest(group=group):
                self.assertFalse(self.complete(groups=self.groups + [{"group": group, "statename": "STOPPED", "pid": 0}]))

    def test_changed_or_failed_current_owner_cannot_prove_restored_management(self):
        for changed in ({"supervisor_id": "successor"}, {"intent": "running"},
                        {"error": "cleanup pending"}, {"problem": {"code": "CleanupUnconfirmed"}}):
            with self.subTest(changed=changed):
                self.assertFalse(self.complete(current=dict(self.current, **changed)))

    def test_missing_terminal_fields_are_not_explicit_null_exit_evidence(self):
        for field in ("generation", "error"):
            incomplete = dict(self.reply)
            del incomplete[field]
            with self.subTest(field=field):
                self.assertFalse(self.complete(reply=incomplete))

class HistoricalOperationRetentionTests(unittest.TestCase):
    def setUp(self):
        self.before = {"operation_id": "original", "kind": "restart", "state": "recovery_required",
                       "request_digest": "immutable-input", "revision": 7, "runtime_instance_id": "same-owner",
                       "error_code": "entry_preflight_failed", "error_message": "real stop RPC deadline exceeded",
                       "failure_detail": {"stage": "entry_preflight_failed", "cleanup_confirmed": False}}
        self.after = dict(self.before, state="failed")

    def test_confirmed_recovery_can_finish_failed_without_rewriting_diagnosis(self):
        self.assertTrue(faults.historical_operation_preserved(self.before, self.after, True))
        self.assertTrue(faults.historical_operation_preserved(self.before, self.before, False))

    def test_state_convergence_requires_physical_cleanup_evidence(self):
        self.assertFalse(faults.historical_operation_preserved(self.before, self.after, False))

    def test_code_message_or_failure_detail_changes_are_not_history_retention(self):
        for change in ({"error_code": "ERR_INTERRUPTED_OWNER_EXIT"},
                       {"error_message": "generic management recovery"},
                       {"failure_detail": {"stage": "startup_reconciled", "cleanup_confirmed": True}},
                       {"failure_detail": None}):
            with self.subTest(change=change):
                self.assertFalse(faults.historical_operation_preserved(self.before, dict(self.after, **change), True))
        without_detail = {key: value for key, value in self.before.items() if key != "failure_detail"}
        self.assertFalse(faults.historical_operation_preserved(without_detail, self.after, True))

    def test_confirmed_cleanup_cannot_rewrite_failure_as_success_or_cancellation(self):
        for state in ("succeeded", "cancelled", "running", "accepted"):
            with self.subTest(state=state):
                self.assertFalse(faults.historical_operation_preserved(self.before, dict(self.after, state=state), True))

    def test_original_identity_and_input_remain_bound_to_original_request(self):
        for key, value in (("operation_id", "new-request"), ("kind", "start"),
                           ("request_digest", "another-input"), ("revision", 8),
                           ("runtime_instance_id", "successor")):
            with self.subTest(key=key):
                self.assertFalse(faults.historical_operation_preserved(self.before, dict(self.after, **{key: value}), True))

    def test_private_request_is_retained_even_if_public_failure_view_matches(self):
        before = {"view": self.before, "request": {"operation_id": "original", "profile": {"workspace_id": "workspace"}}}
        after = {"view": self.after, "request": before["request"]}
        self.assertTrue(faults.historical_receipt_preserved(before, after, True))
        changed = dict(after, request={"operation_id": "original", "profile": {"workspace_id": "another-workspace"}})
        self.assertFalse(faults.historical_receipt_preserved(before, changed, True))


if __name__ == "__main__":
    unittest.main()
