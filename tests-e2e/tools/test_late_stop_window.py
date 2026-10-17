"""Offline mocks/local Unix protocol; not real Docker or 120-second acceptance.

The paired Compose fixture remains responsible for a real AutoRemove container,
unchanged 120s client timeout, actual old-CID 404, and rebuilt-source HTTP.
This file never executes Docker/Cargo and mocks every upstream network call.
"""
import copy
import io
import json
from pathlib import Path
import socket
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

import compute_failure_contract as contract
import docker_fault_proxy as proxy


OLD, NEW, APP, LIFE = "a" * 64, "b" * 64, "phase2app", "original-life"


class Response:
    def __init__(self, status, body=None):
        self.status = status
        self.body = io.BytesIO(json.dumps(body or {}).encode())

    def read(self, amount=None):
        return self.body.read() if amount is None else self.body.read(amount)

    def getheaders(self):
        return [("Content-Type", "application/json")]


class Connection:
    def __init__(self):
        self.closed = False

    def close(self):
        self.closed = True


def inspect_body(auto_remove=True):
    return {
        "Id": OLD,
        "Config": {"Labels": {
            "rcoder.io/application-id": APP, "rcoder.io/lifecycle-id": LIFE,
            "service-type": "user-app-builder",
        }},
        "HostConfig": {"AutoRemove": auto_remove},
        "State": {"Running": False},
    }


class ExactProbeTests(unittest.TestCase):
    def test_exact_404_is_absence_and_errors_or_wrong_200_are_not(self):
        for status, body, expected in [
            (404, {}, "absent"), (200, inspect_body(), "present"),
            (403, {}, "error"), (500, {}, "error"),
            (200, {**inspect_body(), "Id": NEW}, "error"),
        ]:
            connection = Connection()
            with self.subTest(status=status, expected=expected), patch.object(
                proxy, "exchange", return_value=(connection, Response(status, body))
            ) as exchange:
                if expected == "error":
                    with self.assertRaises(ValueError):
                        proxy.observe_exact_old_cid_absence(OLD, seconds=0)
                else:
                    observed = proxy.observe_exact_old_cid_absence(OLD, seconds=0)
                    self.assertEqual(observed["container_id"], OLD)
                    self.assertEqual(observed["status"], 404 if expected == "absent" else 200)
                exchange.assert_called_once_with("GET", "/containers/" + OLD + "/json")
                self.assertTrue(connection.closed)

    def test_actual_inspect_auto_remove_must_be_boolean_true(self):
        for flag in (True, False, None, "true", 1):
            with tempfile.TemporaryDirectory() as temporary, self.subTest(flag=flag):
                root = Path(temporary)
                (root / "compute-arm.json").write_text(json.dumps({
                    "container_id": OLD, "lifecycle_id": LIFE, "stop_window": "before_write",
                }))
                with patch.multiple(proxy, ROOT=root, APP=APP, MODE="compute_stop_timeout",
                                    OWNED_CONTAINERS={OLD: LIFE}, OWNED_ALIASES={},
                                    OWNED_AUTO_REMOVE={}, GATE_LOCK=threading.Lock()), \
                        patch.object(proxy, "exchange",
                                     return_value=(Connection(), Response(200, inspect_body(flag)))):
                    self.assertTrue(proxy.container_owned(OLD))
                    self.assertEqual(proxy.OWNED_AUTO_REMOVE[OLD], flag)
                    self.assertIs(type(proxy.OWNED_AUTO_REMOVE[OLD]), type(flag))
                    if flag is not True:
                        with self.assertRaises(ValueError):
                            proxy.hold_original_stop_request("POST", "/containers/" + OLD + "/stop")
                        self.assertFalse((root / "compute-stop-held.json").exists())


@unittest.skipUnless(hasattr(socket, "AF_UNIX"), "local Unix protocol fixture")
class BeforeWriteProtocolTests(unittest.TestCase):
    def test_original_request_is_not_forwarded_until_release_and_then_gets_old_cid_404(self):
        # The fake daemon is deliberately stateful: held request != held reply.
        # No command or connection to a real Docker daemon is used.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            operation = {"app_id": APP, "lifecycle_id": LIFE, "operation_id": "original-op",
                         "stage": "stopping", "state": "running"}
            (root / "compute-arm.json").write_text(json.dumps({
                "container_id": OLD, "lifecycle_id": LIFE, "stop_window": "before_write",
            }))
            (root / "compute-operation.json").write_text(json.dumps({"operation_id": "original-op"}))
            state = {"old_exists": True, "new_running": False}
            calls, wire, errors = [], [], []
            mutex = threading.Lock()

            def exchange(method, path, body=None, headers=None):
                with mutex:
                    calls.append((method, path))
                    if method == "GET" and path == "/containers/" + OLD + "/json":
                        status = 200 if state["old_exists"] else 404
                        return Connection(), Response(status, inspect_body() if status == 200 else {})
                    if method == "POST" and path.split("?")[0].endswith("/containers/" + OLD + "/stop"):
                        self.assertFalse(state["old_exists"], "test releases only after exact old-CID absence")
                        return Connection(), Response(404, {"message": "No such captured Container"})
                    self.fail("unexpected upstream protocol call: " + method + " " + path)

            def urlopen(url, timeout):
                self.assertIn("/computer/pod/operations/" + APP + "/original-op", url)
                return io.BytesIO(json.dumps({"code": "0000", "data": operation}).encode())

            endpoint = root / "proxy.sock"
            with patch.multiple(proxy, ROOT=root, APP=APP, MODE="compute_stop_timeout",
                                OWNED_CONTAINERS={OLD: LIFE}, OWNED_ALIASES={},
                                OWNED_AUTO_REMOVE={}, STOPPED_CONTAINERS=set(),
                                GATE_LOCK=threading.Lock()), \
                    patch.object(proxy, "exchange", side_effect=exchange), \
                    patch.object(proxy.urllib.request, "urlopen", side_effect=urlopen), \
                    proxy.Server(str(endpoint), proxy.Handler) as server:
                serving = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": .01}, daemon=True)
                serving.start()

                def request():
                    try:
                        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                            client.settimeout(5)
                            client.connect(str(endpoint))
                            client.sendall(("POST /v1.47/containers/" + OLD
                                + "/stop?t=30 HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").encode())
                            while True:
                                chunk = client.recv(8192)
                                if not chunk:
                                    break
                                wire.append(chunk)
                    except BaseException as error:
                        errors.append(error)

                client = threading.Thread(target=request, daemon=True)
                client.start()
                release = root / "compute-stop-release.json"
                try:
                    held = root / "compute-stop-held.json"
                    deadline = time.monotonic() + 3
                    captured = None
                    while captured is None and time.monotonic() < deadline:
                        try:
                            captured = json.loads(held.read_text())
                        except (FileNotFoundError, json.JSONDecodeError):
                            time.sleep(.01)
                    self.assertIsNotNone(captured, "production hold function must publish its complete barrier")
                    self.assertIs(captured["actual_auto_remove"], True)
                    self.assertIs(captured["request_held_before_upstream"], True)
                    self.assertIs(captured["upstream_submitted"], False)
                    self.assertFalse(any(method == "POST" for method, _ in calls))
                    self.assertTrue(client.is_alive(), "original request is still held")

                    # Protocol-state simulation of the separate fault actor and
                    # current-source recovery; this is not a real remove/Create.
                    with mutex:
                        state["old_exists"] = False
                        state["new_running"] = True
                    absent = proxy.observe_exact_old_cid_absence(OLD, seconds=0)
                    self.assertEqual(absent["status"], 404)
                    contract.publish_fault_config(release, {"action": "continue"})
                    client.join(timeout=3)
                    self.assertFalse(client.is_alive())
                    self.assertEqual(errors, [])
                    self.assertTrue(b"".join(wire).startswith(b"HTTP/1.1 404"))
                    settled = json.loads((root / "compute-stop-settled.json").read_text())
                    self.assertEqual(settled["container_id"], OLD)
                    self.assertEqual(settled["upstream_status"], 404)
                    self.assertIs(settled["upstream_submitted"], True)
                    self.assertTrue(state["new_running"], "late old-CID Stop cannot address the new CID")
                    stops = [(method, path) for method, path in calls if method == "POST"]
                    self.assertEqual(stops, [("POST", "/v1.47/containers/" + OLD + "/stop?t=30")])
                    self.assertFalse(any(method == "DELETE" or NEW in path for method, path in calls))
                finally:
                    if not release.exists():
                        contract.publish_fault_config(release, {"action": "discard"})
                    client.join(timeout=3)
                    server.shutdown()
                    serving.join(timeout=3)


class WholeWindowAuditTests(unittest.TestCase):
    def trace(self, stop_status=404):
        writes = [
            {"request_id": "stop", "method": "POST", "path": "/containers/" + OLD + "/stop", "query": {}},
            {"request_id": "create", "method": "POST", "path": "/containers/create",
             "query": {"name": ["rcoder-app-builder-" + APP]}},
            {"request_id": "start", "method": "POST", "path": "/containers/" + NEW + "/start", "query": {}},
        ]
        # The delayed Stop response is recorded last, after Create and Start.
        results = [
            {"request_id": "create", "outcome": "upstream", "status": 201,
             "created_container_id": NEW, "creation": {
                 "rcoder.io/application-id": APP, "rcoder.io/lifecycle-id": LIFE,
                 "service-type": "user-app-builder"}},
            {"request_id": "start", "outcome": "upstream", "status": 204, "container_id": NEW},
            {"request_id": "stop", "outcome": "upstream", "status": stop_status, "container_id": OLD},
        ]
        return writes, results

    def audit(self, writes, results):
        return contract.validate_timeout_write_window(writes, results, old_id=OLD, new_id=NEW,
            app=APP, lifecycle=LIFE, original_stop_statuses=(404,))

    def test_one_original_stop_one_create_one_start_no_delete(self):
        audit = self.audit(*self.trace())
        self.assertEqual(audit["counts"], {"stop": 1, "create": 1, "start": 1, "remove": 0,
                                          "exec_create": 0, "exec_start": 0})
        self.assertFalse(audit["old_cid_remove_fence"])
        self.assertEqual(audit["sdk_retry_allowances"], [])

    def test_wrong_target_duplicate_attempt_denied_result_and_volume_delete_fail(self):
        for change in ("stop-new", "start-old", "create-foreign", "duplicate-stop",
                       "denied-start", "delete-volumes", "late-stop-204"):
            writes, results = copy.deepcopy(self.trace())
            if change == "stop-new":
                writes[0]["path"] = "/containers/" + NEW + "/stop"
            elif change == "start-old":
                writes[2]["path"] = "/containers/" + OLD + "/start"
            elif change == "create-foreign":
                results[0]["creation"]["rcoder.io/application-id"] = "foreign-app"
            elif change == "duplicate-stop":
                replay = copy.deepcopy(writes[0])
                replay["request_id"] = "second-stop"
                writes.append(replay)
                results.append({"request_id": "second-stop", "outcome": "upstream",
                                "status": 404, "container_id": OLD})
            elif change == "denied-start":
                results[1]["outcome"] = "denied"
            elif change == "delete-volumes":
                writes.append({"request_id": "remove", "method": "DELETE",
                               "path": "/containers/" + OLD, "query": {"v": ["true"]}})
                results.append({"request_id": "remove", "outcome": "upstream",
                                "status": 204, "container_id": OLD})
            elif change == "late-stop-204":
                results[-1]["status"] = 204
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.audit(writes, results)


class TimeoutObservationTests(unittest.TestCase):
    def operation(self, state="recovery_required", stage="stopping"):
        return {"app_id": APP, "lifecycle_id": LIFE, "operation_id": "original-op",
                "state": state, "stage": stage, "error_code": "ERR_BACKEND_ERROR",
                "error_message": "original Docker request timed out"}

    def arm(self, root):
        (root / "compute-arm.json").write_text(json.dumps({
            "container_id": OLD, "lifecycle_id": LIFE, "stop_window": "before_write",
            "observe_timeout_checkpoint": True}))
        (root / "compute-operation.json").write_text('{"operation_id":"original-op"}')

    def test_only_original_recovery_get_is_paused_and_fresh_reads_are_not_regated(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); self.arm(root); errors = []
            operation = self.operation()
            def read_operation(url, timeout):
                self.assertTrue(url.endswith("/" + APP + "/original-op"))
                return io.BytesIO(json.dumps({"code": "0000", "data": operation}).encode())
            def hold():
                try:
                    proxy.hold_timeout_checkpoint_read("GET", "/v1.47/containers/" + OLD + "/json")
                except BaseException as error:
                    errors.append(error)
            with patch.multiple(proxy, ROOT=root, APP=APP, MODE="compute_stop_timeout",
                                OWNED_CONTAINERS={OLD: LIFE}, GATE_LOCK=threading.Lock()), \
                    patch.object(proxy.urllib.request, "urlopen", side_effect=read_operation) as query, \
                    patch.object(proxy, "exchange") as docker:
                for method, path in [("GET", "/containers/" + NEW + "/json"),
                                     ("POST", "/containers/" + OLD + "/stop")]:
                    proxy.hold_timeout_checkpoint_read(method, path)
                query.assert_not_called()
                operation["state"] = "running"
                proxy.hold_timeout_checkpoint_read("GET", "/containers/" + OLD + "/json")
                self.assertFalse((root / "compute-timeout-observation.json").exists())
                operation["state"] = "recovery_required"
                worker = threading.Thread(target=hold, daemon=True); worker.start()
                try:
                    observed = contract.wait_control_json(root / "compute-timeout-observation.json",
                                                         contract.PhaseDeadline(3, "test read barrier"))
                    self.assertTrue(worker.is_alive())
                    self.assertEqual(observed["operation"], operation)
                    self.assertTrue(observed["read_held_before_upstream"])
                    docker.assert_not_called()
                    contract.release_timeout_observation(root, "continue")
                    worker.join(timeout=3)
                    self.assertFalse(worker.is_alive())
                    self.assertEqual(errors, [])
                    before = query.call_count
                    proxy.hold_timeout_checkpoint_read("GET", "/containers/" + OLD + "/json")
                    self.assertEqual(query.call_count, before, "new controller reads are no longer gated")
                finally:
                    contract.release_timeout_observation(root, "discard")
                    worker.join(timeout=3)

    def test_foreign_operation_cannot_publish_a_checkpoint_barrier(self):
        for field in ("app_id", "lifecycle_id", "operation_id"):
            with tempfile.TemporaryDirectory() as temporary, self.subTest(field=field):
                root = Path(temporary); self.arm(root); operation = self.operation(); operation[field] = "foreign"
                with patch.multiple(proxy, ROOT=root, APP=APP, MODE="compute_stop_timeout",
                                    OWNED_CONTAINERS={OLD: LIFE}, GATE_LOCK=threading.Lock()), \
                        patch.object(proxy.urllib.request, "urlopen", return_value=io.BytesIO(
                            json.dumps({"code": "0000", "data": operation}).encode())):
                    with self.assertRaises(ValueError):
                        proxy.hold_timeout_checkpoint_read("GET", "/containers/" + OLD + "/json")
                    self.assertFalse((root / "compute-timeout-observation.json").exists())

    def test_diagnostic_capture_does_not_require_a_background_get_or_permanent_state(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); original = self.operation()
            self.assertEqual(contract.wait_timeout_diagnostic(lambda *_args, **_kwargs: original,
                "/operation", root, contract.PhaseDeadline(1, "baseline diagnostic")), original)
            (root / "compute-timeout-observation.json").write_text(json.dumps({"operation": original}))
            def advanced(*_args, **_kwargs):
                self.fail("the already-captured real diagnostic must not require a second RR observation")
            self.assertEqual(contract.wait_timeout_diagnostic(advanced, "/operation", root,
                contract.PhaseDeadline(1, "captured transient diagnostic")), original)


class IndependentFaultActorTests(unittest.TestCase):
    def prepare(self, root):
        (root / "compute-arm.json").write_text(json.dumps({"container_id": OLD,
            "lifecycle_id": LIFE, "stop_window": "before_write"}))
        row = inspect_body(); row["State"]["Running"] = True
        return row

    def test_one_real_fault_actor_stop_is_separate_from_the_platform_audit(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); row = self.prepare(root)
            with patch.multiple(proxy, ROOT=root, APP=APP, MODE="compute_stop_timeout"), \
                    patch.object(proxy, "exchange", side_effect=[(Connection(), Response(200, row)),
                        (Connection(), Response(204)), (Connection(), Response(404))]) as upstream:
                result = proxy.stop_captured_fault_actor(OLD)
                self.assertEqual(result["fault_actor_stop_request_count"], 1)
                self.assertFalse(result["sdk_retry_layer"])
                self.assertEqual(result["upstream_status"], 204)
                self.assertEqual(result["actual_old_cid_inspection"]["status"], 404)
                self.assertEqual([(call.args[0], call.args[1]) for call in upstream.call_args_list], [
                    ("GET", "/containers/" + OLD + "/json"),
                    ("POST", "/containers/" + OLD + "/stop?t=1"),
                    ("GET", "/containers/" + OLD + "/json")])
                self.assertFalse((root / "compute-writes.jsonl").exists(), "fault actor is not a platform Stop")

    def test_unknown_fault_actor_result_keeps_attempt_and_never_replays_stop(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); row = self.prepare(root)
            with patch.multiple(proxy, ROOT=root, APP=APP, MODE="compute_stop_timeout"), \
                    patch.object(proxy, "exchange", side_effect=[(Connection(), Response(200, row)),
                        OSError("response lost"), (Connection(), Response(200, row))]) as upstream:
                with self.assertRaises(OSError): proxy.stop_captured_fault_actor(OLD)
                self.assertTrue((root / "compute-fault-actor-attempt.json").exists())
                with self.assertRaises(FileExistsError): proxy.stop_captured_fault_actor(OLD)
                self.assertEqual(sum(call.args[0] == "POST" for call in upstream.call_args_list), 1)

    def test_foreign_or_non_auto_remove_target_never_receives_fault_actor_stop(self):
        for change in ("foreign", "not-auto-remove"):
            with tempfile.TemporaryDirectory() as temporary, self.subTest(change=change):
                root = Path(temporary); row = self.prepare(root)
                if change == "foreign": row["Config"]["Labels"]["rcoder.io/lifecycle-id"] = "foreign"
                else: row["HostConfig"]["AutoRemove"] = False
                with patch.multiple(proxy, ROOT=root, APP=APP, MODE="compute_stop_timeout"), \
                        patch.object(proxy, "exchange", return_value=(Connection(), Response(200, row))) as upstream:
                    with self.assertRaises(ValueError): proxy.stop_captured_fault_actor(OLD)
                    self.assertEqual([call.args[0] for call in upstream.call_args_list], ["GET"])
                    self.assertFalse((root / "compute-fault-actor-attempt.json").exists())


if __name__ == "__main__":
    unittest.main()
