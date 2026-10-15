#!/usr/bin/env python3
"""Real supervisord faults inside a dedicated acceptance container.

Every response comes from the real supervisor. A transparent Unix transport
holds one completed app-pingap Stop/Remove response past the caller deadline.
No synthetic process state, success response, or manual journal repair is used.
"""
import argparse
import hashlib
import json
from pathlib import Path
import socket
import socketserver
import subprocess
import threading
import time
import uuid
import xmlrpc.client

from official_pingap_lifecycle import Acceptance, CLI, STATE, WORK


def read_http(reader):
    lines = []
    while True:
        line = reader.readline(65537)
        if not line or len(line) > 65536:
            raise AssertionError("incomplete supervisor HTTP header")
        lines.append(line)
        if line == b"\r\n":
            break
    headers = b"".join(lines)
    lengths = [line.partition(b":")[2].strip() for line in lines
               if line.lower().startswith(b"content-length:")]
    if len(lengths) != 1:
        raise AssertionError("supervisor must provide an exact Content-Length")
    size = int(lengths[0])
    if not 0 <= size <= 16 * 1024 * 1024:
        raise AssertionError("supervisor response exceeds fixture budget")
    body = reader.read(size)
    if len(body) != size:
        raise AssertionError("incomplete supervisor HTTP body")
    return headers + body, body


def native_stop_cleanup_complete(captured, request_id, reply, snapshot, groups):
    """Original StopWork terminal and current physical emptiness are distinct.

    The owner may already have started a new management-only worker by the time
    status is read. Its current Ready phase cannot invalidate the retained Stop
    reply, but the exact reply and real supervisor business absence are required.
    """
    return (reply.get("phase") == "stopped" and reply.get("intent") == "stopped"
            and reply.get("operation_id") == request_id and reply.get("generation", "missing") is None
            and reply.get("error", "missing") is None and reply.get("problem") is None
            and reply.get("supervisor_id") == captured["supervisor_id"]
            and reply.get("binding") == captured["binding"]
            and snapshot.get("supervisor_id") == captured["supervisor_id"]
            and snapshot.get("binding") == captured["binding"]
            and snapshot.get("intent") == "stopped"
            and snapshot.get("error", "missing") is None and snapshot.get("problem") is None
            and not any(row["group"] == "app-pingap" or row["group"].startswith("app-svc-")
                        for row in groups))


def historical_operation_preserved(before, after, cleanup_confirmed):
    """Only confirmed RecoveryRequired-to-Failed convergence changes history.

    The original identity, request digest, error and failure detail are retained.
    Later successful cleanup is evidence for the new operation, not permission
    to rewrite the original failure as success or replace its diagnosis.
    """
    if not isinstance(before, dict) or not isinstance(after, dict):
        return False
    old, new = dict(before), dict(after)
    old_state, new_state = old.pop("state", None), new.pop("state", None)
    state_retained = old_state == new_state and old_state in {"recovery_required", "failed"}
    confirmed_terminal = cleanup_confirmed is True and (old_state, new_state) == ("recovery_required", "failed")
    return bool(before.get("error_code") and before.get("error_message")
                and old == new and (state_retained or confirmed_terminal))


def historical_receipt_preserved(before, after, cleanup_confirmed):
    if not isinstance(before, dict) or not isinstance(after, dict):
        return False
    old, new = dict(before), dict(after)
    old_view, new_view = old.pop("view", None), new.pop("view", None)
    return old == new and historical_operation_preserved(old_view, new_view, cleanup_confirmed)


class DelayedSupervisor:
    """Delay a real reply only after the upstream supervisor completed it."""
    def __init__(self, front, upstream):
        self.upstream = str(upstream)
        self.lock = threading.Lock()
        self.condition = threading.Condition(self.lock)
        self.active_handlers = 0
        self.closing = False
        self.close_result = None
        self.events = []
        self.failures = []
        self.fault = None
        controller = self

        class Handler(socketserver.StreamRequestHandler):
            def handle(self):
                method = None
                stage = "read_request"
                try:
                    request, body = read_http(self.rfile)
                    params, method = xmlrpc.client.loads(body)
                    stage = "forward_upstream_request"
                    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as real:
                        real.settimeout(45)
                        real.connect(controller.upstream)
                        real.sendall(request)
                        stage = "read_complete_upstream_response"
                        with real.makefile("rb") as reader:
                            response, returned = read_http(reader)
                    fault_code = None
                    try:
                        xmlrpc.client.loads(returned)
                    except xmlrpc.client.Fault as fault:
                        fault_code = fault.faultCode
                    with controller.lock:
                        event = {"method": method, "group": params[0] if params else None,
                                 "upstream_completed": True, "upstream_fault": fault_code,
                                 "response_released": False, "at": time.monotonic(),
                                 "request_bytes": len(request), "response_bytes": len(response),
                                 "upstream_response_sha256": hashlib.sha256(response).hexdigest()}
                        controller.events.append(event)
                        fault = controller.fault
                        delayed = bool(fault and not fault["captured"]
                                       and method == fault["method"]
                                       and params and params[0] == "app-pingap")
                        if delayed:
                            fault["captured"] = True
                            fault["event"] = event
                            fault["held"].set()
                    stage = "hold_completed_upstream_reply" if delayed else "forward_exact_reply"
                    if delayed and not fault["release"].wait(100):
                        raise AssertionError("fault response release deadline exhausted")
                    stage = "forward_exact_reply"
                    self.wfile.write(response)
                    self.wfile.flush()
                    with controller.lock:
                        event["response_released"] = True
                        event["forwarded_response_sha256"] = hashlib.sha256(response).hexdigest()
                except Exception as error:
                    with controller.lock:
                        controller.failures.append({"method": method, "stage": stage,
                            "during_teardown": controller.closing,
                            "error": type(error).__name__ + ": " + str(error)})
                finally:
                    with controller.condition:
                        controller.active_handlers -= 1
                        controller.condition.notify_all()

        class Server(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
            daemon_threads = True
            def process_request(self, request, client_address):
                with controller.condition:
                    controller.active_handlers += 1
                try:
                    super().process_request(request, client_address)
                except Exception:
                    with controller.condition:
                        controller.active_handlers -= 1
                        controller.condition.notify_all()
                    raise
        self.server = Server(str(front), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def arm(self, method):
        with self.lock:
            if self.fault and not self.fault["release"].is_set():
                raise AssertionError("the previous fault response is still pending")
            self.fault = {"method": "supervisor." + method, "captured": False,
                          "held": threading.Event(), "release": threading.Event()}
            return self.fault

    def snapshot(self):
        with self.lock:
            return {"events": [dict(event) for event in self.events],
                    "failures": list(self.failures), "active_handlers": self.active_handlers,
                    "during_teardown": self.closing, "close_result": self.close_result}

    def close(self):
        with self.lock:
            self.closing = True
            if self.fault:
                self.fault["release"].set()
        self.server.shutdown()
        # Preserve already accepted complete RPCs during failure teardown. The
        # wait stays bounded, and never replays a request or invents its result.
        deadline = time.monotonic() + 10
        with self.condition:
            while self.active_handlers and time.monotonic() < deadline:
                self.condition.wait(max(0, deadline - time.monotonic()))
            self.close_result = {"accepted_handlers_drained": self.active_handlers == 0,
                                 "pending_handlers": self.active_handlers}
        self.server.server_close()
        return self.close_result


class RecoveryAcceptance(Acceptance):
    def __init__(self, source_digest, binary_sha256):
        super().__init__("supervisord", source_digest)
        self.owner_supervisor_socket = "/var/run/supervisor.sock"
        self.real_supervisor_socket = "/var/run/supervisor-real.sock"
        self.binary_sha256 = binary_sha256
        self.forwarder = None

    def real_rpc(self, method, *params):
        upstream = self.real_supervisor_socket
        class UnixTransport(xmlrpc.client.Transport):
            def make_connection(self, host):
                import http.client
                class Connection(http.client.HTTPConnection):
                    def connect(self):
                        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                        self.sock.settimeout(10)
                        self.sock.connect(upstream)
                return Connection("localhost", timeout=10)
        with xmlrpc.client.ServerProxy("http://localhost", transport=UnixTransport()) as client:
            return getattr(client.supervisor, method)(*params)

    def native_stop(self):
        """Use the original native StopWork entrance and prove its terminal."""
        captured = self.native_status()
        original_owner = self.identity()["runtime_instance_id"]
        request_id = "reconcile-" + uuid.uuid4().hex
        deadline = time.monotonic() + 60
        args = [str(CLI), "owner", "stop", "--workspace", str(WORK),
                "--request-id", request_id, "--generation", captured["generation"] or ""]
        last = None
        while time.monotonic() < deadline:
            result = subprocess.run(args, env=self.owner_environment(), capture_output=True,
                                    text=True, timeout=10)
            if result.returncode:
                raise AssertionError("original native StopWork refused reconciliation: " + result.stderr[-1000:])
            reply = json.loads(result.stdout)
            snapshot = self.native_status()
            last = {"reply": reply, "snapshot": snapshot}
            if reply.get("phase") == "stopped" and reply.get("operation_id") == request_id:
                groups = self.real_rpc("getAllProcessInfo")
                last["process_info"] = groups
                self.check("original native StopWork confirms physical cleanup",
                           native_stop_cleanup_complete(captured, request_id, reply, snapshot, groups),
                           request_id=request_id, terminal_reply=reply,
                           current_snapshot=snapshot, process_info=groups)
                owners = [row for row in groups if row["group"] == "app-cli"]
                self.check("original native StopWork restores management after business exit",
                           self.identity()["runtime_instance_id"] == original_owner
                           and len(owners) == 1 and owners[0]["statename"] == "RUNNING"
                           and owners[0]["pid"] > 0,
                           runtime_instance_id=original_owner, process_info=owners)
                return last
            time.sleep(0.1)
        raise AssertionError("original native StopWork did not settle: " + repr(last))

    def fault_case(self, method):
        original_identity = self.identity()
        original_owner = original_identity["runtime_instance_id"]
        previous = self.ready("B")
        held = self.forwarder.arm(method)
        failed = self.operate("restart", succeeds=False)
        self.check(method + " fault actually held a completed real supervisor RPC",
                   held["held"].is_set() and held["event"]["upstream_completed"]
                   and held["event"]["upstream_fault"] is None
                   and not held["event"]["response_released"], event=dict(held.get("event") or {}))
        # Bypass only the test transport to inspect real physical state. All
        # application operations still go through the original management API.
        groups = self.real_rpc("getAllProcessInfo")
        proxy = [row for row in groups if row["group"] == "app-pingap"]
        fragments = {path.name: path.read_text() for path in Path("/etc/supervisor/conf.d").glob("5*.conf")}
        self.check(method + " expired caller leaves proxy configuration withdrawn",
                   all("[program:app-pingap]" not in text for text in fragments.values()),
                   fragments=fragments)
        if method == "stopProcessGroup":
            self.check("late Stop leaves the real proxy STOPPED with PID zero and Remove pending",
                       len(proxy) == 1 and proxy[0]["statename"] == "STOPPED" and proxy[0]["pid"] == 0,
                       process_info=proxy)
        else:
            self.check("late Remove already removed the real group before its reply", not proxy,
                       process_info=proxy)
        receipt_path = STATE / "control/operations" / (failed["operation_id"] + ".json")
        receipt_before_bytes = receipt_path.read_bytes()
        receipt_before = json.loads(receipt_before_bytes)
        self.check(method + " original failed operation has a durable receipt",
                   all(receipt_before["view"].get(key) == value for key, value in failed.items()),
                   original_view=failed, original_receipt=receipt_before)
        self.evidence[method + "_before_recovery"] = {
            "native": self.native_status(), "proxy_rows": proxy,
            "fragments": fragments, "failed_operation": failed,
            "original_receipt": receipt_before,
            "original_receipt_sha256": hashlib.sha256(receipt_before_bytes).hexdigest(),
            "rpc": self.forwarder.snapshot()}
        self.save()
        held["release"].set()
        self.wait(lambda: held["event"]["response_released"], bool, 15)
        stopped = self.native_stop()
        # StopWork restores a management-only worker asynchronously. Identity
        # availability and native Ready alone cannot authorize new business;
        # wait for the existing read-only HTTP recovery/initialization barrier.
        self.wait_owner_initialized(original_identity)
        self.check(method + " management initialization completes before original Restart",
                   self.native_status()["supervisor_id"] == stopped["snapshot"]["supervisor_id"])
        self.check(method + " recovery retains the original management owner",
                   self.identity()["runtime_instance_id"] == original_owner)
        self.operate("restart")
        recovered = self.ready("B")
        self.check(method + " original Restart restores one current business process",
                   recovered["pid"] != previous["pid"]
                   and self.identity()["runtime_instance_id"] == original_owner,
                   old_pid=previous["pid"], new_pid=recovered["pid"])
        groups = self.real_rpc("getAllProcessInfo")
        owned = [row for row in groups if row["group"] in {"app-cli", "app-pingap", "app-svc-web"}]
        self.check(method + " recovered real supervisor has one owner, proxy, and business",
                   len(owned) == 3 and {row["group"] for row in owned} == {"app-cli", "app-pingap", "app-svc-web"}
                   and all(row["statename"] == "RUNNING" and row["pid"] > 0 for row in owned),
                   process_info=owned)
        status, original = self.request("/v1/runtime/operations/" + failed["operation_id"])
        receipt_after_bytes = receipt_path.read_bytes()
        receipt_after = json.loads(receipt_after_bytes)
        cleanup_confirmed = (stopped["reply"].get("phase") == "stopped"
                             and stopped["reply"].get("generation", "missing") is None
                             and stopped["reply"].get("error", "missing") is None
                             and not any(row["group"] == "app-pingap" or row["group"].startswith("app-svc-")
                                         for row in stopped["process_info"]))
        self.evidence[method + "_after_recovery"] = {
            "groups": owned, "rpc": self.forwarder.snapshot(),
            "original_after_recovery": original, "original_before_recovery": failed,
            "original_receipt_before": receipt_before, "original_receipt_after": receipt_after,
            "original_receipt_after_sha256": hashlib.sha256(receipt_after_bytes).hexdigest(),
            "native_cleanup": stopped, "physical_cleanup_confirmed": cleanup_confirmed}
        self.save()
        self.check(method + " recovery preserves the original failed operation",
                   status == 200 and original["success"]
                   and historical_operation_preserved(failed, original["data"], cleanup_confirmed),
                   original_before=failed, original_after=original, status=status,
                   physical_cleanup_confirmed=cleanup_confirmed)
        self.check(method + " recovery preserves the original private receipt and request",
                   historical_receipt_preserved(receipt_before, receipt_after, cleanup_confirmed),
                   receipt_before=receipt_before, receipt_after=receipt_after)

    def run(self):
        STATE.mkdir(exist_ok=True)
        self.check("fresh app-cli binary matches the build source receipt",
                   hashlib.sha256(CLI.read_bytes()).hexdigest() == self.binary_sha256,
                   binary_sha256=self.binary_sha256)
        sentinel = self.prepare_fixture()
        (WORK / "marker.txt").write_text("B")
        self.forwarder = DelayedSupervisor(self.owner_supervisor_socket, self.real_supervisor_socket)
        try:
            self.start_owner()
            self.operate("start")
            self.ready("B")
            self.fault_case("stopProcessGroup")
            self.fault_case("removeProcessGroup")
            self.operate("restart")
            self.ready("B")
            self.stop_once()
            self.operate("start")
            self.ready("B")
            self.check("both delayed faults preserve owned workspace data",
                       hashlib.sha256((STATE / "sentinel.txt").read_bytes()).hexdigest() == sentinel)
            self.check("real supervisor transport has no injection failures",
                       not self.forwarder.snapshot()["failures"], rpc=self.forwarder.snapshot())
            self.evidence["passed"] = True
            self.save()
        finally:
            self.evidence["rpc_before_transport_shutdown"] = self.forwarder.snapshot()
            closed = self.forwarder.close()
            self.evidence["rpc_after_transport_shutdown"] = self.forwarder.snapshot()
            self.evidence["checks"].append({"name": "transport shutdown drains accepted RPC handlers",
                "passed": closed["accepted_handlers_drained"], **closed})
            if not closed["accepted_handlers_drained"]:
                self.evidence["passed"] = False
            self.save()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-digest", required=True)
    parser.add_argument("--binary-sha256", required=True)
    args = parser.parse_args()
    runner = RecoveryAcceptance(args.source_digest, args.binary_sha256)
    try:
        runner.run()
    except Exception as error:
        runner.evidence["error"] = str(error)
        runner.save()
        raise


if __name__ == "__main__":
    main()
