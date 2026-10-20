#!/usr/bin/env python3
"""Real official Pingap lifecycle acceptance inside an isolated Linux container.

This runner creates its own workspace and supervisord configuration. It never
accepts arbitrary PIDs, deletes owner state, or changes cluster resources.
Invoke only in the dedicated acceptance container with a freshly built app-cli.
"""
from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import signal
import shutil
import socket
import subprocess
import time
import tomllib
import urllib.error
import urllib.request
import uuid
import xmlrpc.client

WORK = Path("/workspace")
STATE = Path("/state")
CLI = Path("/usr/local/bin/app-cli")
PINGAP = Path("/usr/local/bin/pingap")
ADMIN = "http://127.0.0.1:3010"
PROXY = "http://127.0.0.1:9080"
TERMINAL = {"succeeded", "failed", "cancelled", "recovery_required"}


def owner_initialization_complete(snapshot, recovery_status, recovery, runtime_instance, supervisor):
    """Pure admission guard: identity availability is not startup completion."""
    if not runtime_instance or not supervisor or snapshot.get("supervisor_id") != supervisor:
        raise AssertionError("native owner changed during initialization")
    phase = snapshot.get("phase")
    if phase not in {"reconciling", "starting", "ready", "stopping", "cleanup_pending",
                     "recovery_required", "stopped"}:
        raise AssertionError("native owner initialization has an unknown phase")
    if phase == "recovery_required" or snapshot.get("error") is not None:
        raise AssertionError("native owner initialization requires recovery")
    if recovery_status == 503 and recovery.get("code") == "ERR_INITIALIZING":
        return False
    if recovery_status != 200 or recovery.get("success") is not True:
        raise AssertionError(f"owner initialization recovery query failed: status={recovery_status}, "
                             f"code={recovery.get('code')}")
    view = recovery.get("data")
    if not isinstance(view, dict) or view.get("runtime_instance_id") != runtime_instance:
        raise AssertionError("startup recovery belongs to a different owner instance")
    if view.get("kernel_protected") is not False or view.get("owner_protected") is not False:
        raise AssertionError("fresh isolated owner initialization remains recovery-protected")
    return phase == "ready"


def require_business_process(captured, python_binary: Path, script: Path):
    """Pure fault-injection guard; an HTTP PID alone never authorizes a signal."""
    if (type(captured.get("pid")) is not int or captured["pid"] <= 1
            or not str(captured.get("start_ticks", "")).isdigit()
            or int(captured["start_ticks"]) <= 0
            or captured.get("exe") != str(python_binary.resolve(strict=True))
            or len(captured.get("argv", [])) != 2
            or captured["argv"][1] != os.fsencode(script.resolve(strict=True))
            or captured.get("cwd") != str(script.parent.resolve(strict=True))):
        raise AssertionError("business fault target is not the exact owned workspace Python service")


class Acceptance:
    def __init__(self, engine: str, source_digest: str):
        self.engine = engine
        self.source_digest = source_digest
        self.token = uuid.uuid4().hex
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        self.owner = None
        self.supervisor = None
        self.evidence = {"engine": engine, "source_digest": source_digest,
                         "checks": [], "operations": [], "passed": False}

    def request(self, path: str, method="GET", body=None, request_id=None, base=ADMIN, timeout=None):
        headers = {"X-Deploy-Token": self.token, "Connection": "close"}
        if request_id:
            headers["X-Request-ID"] = request_id
        data = None if body is None else json.dumps(body).encode()
        if method == "POST" and data is None:
            data = b""
        if body is not None:
            headers["Content-Type"] = "application/json"
        req = urllib.request.Request(base + path, data=data, headers=headers, method=method)
        try:
            with self.opener.open(req, timeout=timeout if timeout is not None else
                                  (35 if method == "POST" else 5)) as response:
                status, raw = response.status, response.read()
        except urllib.error.HTTPError as error:
            status, raw = error.code, error.read()
        return status, json.loads(raw) if raw else None

    def check(self, name: str, condition: bool, **facts):
        self.evidence["checks"].append({"name": name, "passed": bool(condition), **facts})
        self.save()
        if not condition:
            raise AssertionError(name)

    def save(self):
        path = STATE / "evidence.json"
        temporary = path.with_suffix(".tmp")
        temporary.write_text(json.dumps(self.evidence, indent=2), encoding="utf-8")
        temporary.replace(path)

    def wait(self, observe, predicate, seconds=90):
        deadline = time.monotonic() + seconds
        last = None
        while time.monotonic() < deadline:
            try:
                last = observe()
                if predicate(last):
                    return last
            except (OSError, ValueError, urllib.error.URLError):
                pass
            time.sleep(0.2)
        raise AssertionError(f"observation deadline exhausted; last={last!r}")

    def identity(self, timeout=None):
        status, envelope = self.request("/v1/runtime/identity", timeout=timeout)
        if status==503 and envelope.get("code")=="ERR_INITIALIZING":
            self.evidence["identity_initializing_observations"]=self.evidence.get("identity_initializing_observations",0)+1
            self.save()
            return None
        if status != 200 or not envelope.get("success"):
            raise AssertionError(f"management identity unavailable: status={status}, code={envelope.get('code')}")
        return envelope["data"]

    def wait_owner_initialized(self, captured_identity, seconds=45):
        """Bounded read-only wait; only explicit ERR_INITIALIZING is transient."""
        runtime_instance = captured_identity["runtime_instance_id"]
        deadline = time.monotonic() + seconds
        supervisor = None
        last = None

        def remaining(limit):
            budget = deadline - time.monotonic()
            if budget <= 0:
                raise AssertionError(f"owner initialization deadline exhausted; last={last!r}")
            return min(limit, budget)

        while True:
            current = self.identity(timeout=remaining(5))
            if not isinstance(current, dict) or current.get("runtime_instance_id") != runtime_instance:
                raise AssertionError("management owner changed after initialization identity capture")
            snapshot = self.native_status(timeout=remaining(3))
            if supervisor is None:
                supervisor = snapshot["supervisor_id"]
            binding = snapshot.get("binding") or {}
            if (binding.get("component") != "app-cli"
                    or Path(binding.get("resource", "")).resolve(strict=True) != WORK.resolve(strict=True)):
                raise AssertionError("initializing native owner belongs to another workspace")
            status, recovery = self.request("/v1/runtime/recovery", timeout=remaining(5))
            last = {"supervisor_id": snapshot["supervisor_id"], "phase": snapshot["phase"],
                    "recovery_status": status, "recovery_code": recovery.get("code"),
                    "snapshot": snapshot, "recovery": recovery}
            try:
                initialized = owner_initialization_complete(snapshot, status, recovery,
                                                            runtime_instance, supervisor)
            except AssertionError:
                self.evidence["owner_initialization_failure"] = last
                self.save()
                raise
            remaining(5)  # A late successful read cannot exceed the original budget.
            if initialized:
                self.check("captured owner finished initialization before business admission", True,
                           runtime_instance_id=runtime_instance, supervisor_id=supervisor,
                           native_phase=snapshot["phase"], recovery=recovery["data"])
                return captured_identity
            time.sleep(min(0.2, remaining(0.2)))

    def owner_environment(self):
        env = os.environ.copy()
        for key in list(env):
            if key.startswith(("APP_DEPLOY_", "PINGAP_")):
                env.pop(key)
        env.update(APP_CLI_WORKSPACE=str(WORK), APP_CLI_STATE_ROOT=str(STATE / "control"),
                   APP_CLI_SPEC_DIR="/run/official-pingap-specs", APP_CLI_LOG_DIR=str(STATE / "logs"),
                   APP_CLI_PINGAP_RUNTIME_DIR=str(STATE / "logs/pingap"),
                   APP_CLI_DEPLOY_TOKEN=self.token, APP_CLI_REQUIRE_PG="0",
                   APP_CLI_SUPERVISOR_SOCKET=(getattr(self, "owner_supervisor_socket", "/var/run/supervisor.sock") if self.engine == "supervisord"
                                              else str(STATE / "not-a-supervisor.sock")))
        env.pop("SERVICE_TYPE", None)
        env.pop("PROJECT_ID", None)
        env.pop("APP_ID", None)
        return env

    def start_owner(self):
        deadline = time.monotonic() + 45
        env = self.owner_environment()
        args = [str(CLI), "serve", "--workspace", str(WORK), "--control-only",
                "--log-dir", str(STATE / "logs"), "--admin-addr", "0.0.0.0:3010"]
        if self.engine == "builtin":
            self.owner = subprocess.Popen(args, env=env, stdout=open(STATE / "owner.log", "ab"),
                                          stderr=subprocess.STDOUT, start_new_session=True)
        else:
            conf = STATE / "supervisord.conf"
            real_socket = getattr(self, "real_supervisor_socket", "/var/run/supervisor.sock")
            conf.write_text("[unix_http_server]\nfile=" + real_socket + "\nchmod=0700\n"
                            "[supervisord]\nnodaemon=true\nlogfile=/state/supervisord.log\n"
                            "pidfile=/state/supervisord.pid\n"
                            "[rpcinterface:supervisor]\nsupervisor.rpcinterface_factory=supervisor.rpcinterface:make_main_rpcinterface\n"
                            "[supervisorctl]\nserverurl=unix://" + real_socket + "\n"
                            "[program:app-cli]\ncommand=" + " ".join(args) + "\n"
                            "autostart=true\nautorestart=true\nstartsecs=0\n"
                            "stopasgroup=true\nkillasgroup=true\nstopwaitsecs=3\n"
                            "stdout_logfile=/state/owner.log\nredirect_stderr=true\n"
                            "[include]\nfiles=/etc/supervisor/conf.d/5*.conf\n", encoding="utf-8")
            Path("/etc/supervisor/conf.d").mkdir(parents=True, exist_ok=True)
            self.supervisor = subprocess.Popen(["supervisord", "-n", "-c", str(conf)], env=env,
                                               stdout=open(STATE / "supervisor-console.log", "ab"),
                                               stderr=subprocess.STDOUT, start_new_session=True)
        captured = self.wait(self.identity, lambda view: isinstance(view,dict) and bool(view.get("runtime_instance_id")),
                             max(0, deadline - time.monotonic()))
        self.wait_owner_initialized(captured, seconds=max(0, deadline - time.monotonic()))

    def operate(self, kind: str, succeeds=True, midwindow_readiness=False):
        identity = self.identity()
        status, envelope = self.request("/v1/runtime/status")
        self.check("runtime status is readable", status == 200, status=status, response=envelope)
        operation_id = str(uuid.uuid4())
        request = {"operation_id": operation_id,
                   "expected_runtime_instance_id": identity["runtime_instance_id"],
                   "expected_revision": envelope["data"]["revision"],
                   "workspace_id": identity["workspace_id"], "kind": kind,
                   "profile": {"profile": "source", "input": {"workspace_id": identity["workspace_id"]}}}
        status, accepted = self.request("/v1/runtime/operations", "POST", request)
        self.check(f"{kind} accepted with exact operation", status == 202 and accepted.get("success"),
                   operation_id=operation_id, status=status, response=accepted)
        if midwindow_readiness:
            def open_stop_snapshot():
                readiness = self.request("/v1/app/readiness")
                operation = self.request(f"/v1/runtime/operations/{operation_id}")
                return readiness, operation
            def stopping_while_open(observed):
                (readiness_status, readiness), (operation_status, operation) = observed
                data = (readiness or {}).get("data") or {}
                return (readiness_status == 200 and data.get("status") == "stopping"
                        and operation_status == 200
                        and (operation or {}).get("data", {}).get("state") not in TERMINAL)
            (readiness, operation) = self.wait(open_stop_snapshot, stopping_while_open)
            (readiness_status, readiness) = readiness
            (operation_status, operation) = operation
            data = readiness["data"]
            self.check("stop window reports stopping while the operation is unsettled",
                       readiness_status == 200 and operation_status == 200
                       and data["status"] == "stopping" and data["reason_code"] == "STOP_ACCEPTED"
                       and data["ready"] is False and data.get("operation_id") == operation_id,
                       snapshot=data, operation=operation["data"])
        _, result = self.wait(lambda: self.request(f"/v1/runtime/operations/{operation_id}"),
                              lambda response: response[0] == 200 and response[1]["data"]["state"] in TERMINAL)
        view = result["data"]
        self.evidence["operations"].append(view)
        self.check(f"{kind} operation truthful terminal", view["operation_id"] == operation_id and
                   (view["state"] == "succeeded" if succeeds else view["state"] != "succeeded"))
        return view

    def ready(self, marker="A"):
        _, readiness = self.wait(lambda: self.request("/v1/app/readiness"),
                                 lambda value: value[0] == 200 and value[1]["data"]["ready"])
        _, business = self.wait(lambda: self.request("/", base=PROXY),
                               lambda value: value[0] == 200 and value[1]["marker"] == marker)
        self.check("actual proxy and business HTTP", business["marker"] == marker,
                   business_pid=business["pid"], port=business["port"])
        confirmation = readiness["data"]["proxy"].get("confirmation")
        self.check("official confirmation instead of private Applied", bool(confirmation) and
                   confirmation["confirmed"] and confirmation["contract"] == "official_restart_http_v1")
        return business

    def verify_secondary_listener(self,port):
        status,envelope=self.request("/v1/proxy/status")
        self.check("secondary listener has current official confirmation",status==200 and envelope["data"]["confirmation"]["confirmed"])
        publication=envelope["data"]["publication_id"]
        with self.opener.open(f"http://127.0.0.1:{port}/_pub/{publication}",timeout=5) as response:
            raw=response.read().decode()
            self.check("changed listener executes current publication UUID",response.status==200 and
                       response.headers.get("X-Rcoder-Publication")==publication and raw==publication,
                       listener_port=port,publication_id=publication)

    def custom_serving(self, marker):
        """Custom routes remain conservatively unsupported in app readiness."""
        status,readiness=self.request("/v1/app/readiness")
        self.check("Custom preserves conservative readiness contract",status==200 and
                   not readiness["data"]["ready"] and readiness["data"]["status"]=="unsupported" and
                   readiness["data"]["reason_code"]=="CUSTOM_ROUTE_UNVERIFIED",response=readiness)
        _,proxy=self.wait(lambda:self.request("/v1/proxy/status"),lambda response:response[0]==200 and
                          bool(response[1]["data"].get("confirmation")) and response[1]["data"]["confirmation"]["confirmed"])
        self.check("Custom has actual official proxy confirmation",proxy["data"]["confirmation"]["contract"]=="official_restart_http_v1")
        _,business=self.wait(lambda:self.request("/",base=PROXY),lambda value:value[0]==200 and value[1]["marker"]==marker)
        self.check("Custom serves real business HTTP",business["marker"]==marker,business_pid=business["pid"])

    def reject_custom_preflight(self, name, candidate, error_fragments, failure_stage):
        previous = self.proxy_receipt()
        process = self.process(previous["process_id"])
        status, current = self.request("/v1/proxy/status")
        confirmation = current["data"].get("confirmation") if status == 200 else None
        self.check(f"{name}: predecessor is confirmed before invalid input",
                   bool(confirmation) and confirmation["confirmed"] and confirmation["phase"] == "serving")
        (WORK / "proxy.custom.toml").write_text(candidate)
        request_id = str(uuid.uuid4())
        status, rejected = self.request("/v1/proxy/reload", "POST", request_id=request_id)
        operation = rejected.get("data") or {}
        message = rejected.get("message", "")
        self.check(f"{name}: preflight returns a concrete Failed operation",
                   status == 400 and rejected.get("success") is False
                   and rejected.get("code") == "PINGAP_CONFIG_INVALID"
                   and operation.get("operation_id") == request_id
                   and operation.get("state") == "failed" and operation.get("phase") == "failed"
                   and operation.get("error_code") == "PINGAP_CONFIG_INVALID"
                   and operation.get("error_message") == message
                   and all(fragment in message for fragment in error_fragments),
                   failure_stage=failure_stage, http_status=status, response=rejected)
        self.check(f"{name}: old proxy receipt and OS incarnation remain unchanged",
                   self.proxy_receipt() == previous and self.process(previous["process_id"]) == process,
                   process_id=previous["process_id"], proxy_instance_id=previous["instance_id"])
        status, current = self.request("/v1/proxy/status", request_id=request_id)
        self.check(f"{name}: formal old confirmation is preserved alongside the failed request",
                   status == 200 and current["success"] and current["data"]["confirmation"] == confirmation
                   and current["data"]["operation"]["operation_id"] == request_id
                   and current["data"]["operation"]["state"] == "failed")
        self.custom_serving("B")

    @staticmethod
    def process(pid: int):
        stat = Path(f"/proc/{pid}/stat").read_text()
        # comm may contain spaces or ')'; fields after its final ')' start at #3.
        start = stat.rsplit(")", 1)[1].split()[19]
        return {"pid": pid, "start_ticks": start, "exe": str(Path(f"/proc/{pid}/exe").resolve()),
                "argv": Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")[:-1]}

    def exact_kill(self, pid: int, binary: Path, reason: str):
        captured = self.process(pid)
        self.check(f"{reason}: exact executable captured", captured["exe"] == str(binary.resolve()))
        self.check(f"{reason}: same OS incarnation before signal", self.process(pid) == captured)
        os.kill(pid, signal.SIGKILL)
        self.evidence["checks"].append({"name": f"{reason}: exact SIGKILL", "passed": True,
                                         "pid": pid, "start_ticks": captured["start_ticks"]})
        self.save()

    def native_status(self, timeout=3):
        discovery = json.loads((STATE / "control/supervisor.json").read_text())
        if discovery["version"] != 2:
            raise AssertionError("native status protocol changed before business fault injection")
        address, port = discovery["address"].rsplit(":", 1)
        frame = {"version": 2, "instance": discovery["instance"], "token": discovery["token"],
                 "request": {"request_id": str(uuid.uuid4()), "action": "status",
                             "expected_generation": discovery["snapshot"]["generation"]}}
        with socket.create_connection((address, int(port)), timeout=timeout) as connection:
            connection.sendall(json.dumps(frame).encode() + b"\n")
            with connection.makefile("rb") as reader:
                raw = reader.readline(32769)
        if len(raw) > 32768 or not raw.endswith(b"\n"):
            raise AssertionError("native status returned an invalid bounded frame")
        reply = json.loads(raw)
        snapshot = reply["snapshot"]
        if (reply["instance"] != discovery["instance"] or reply.get("error") is not None
                or snapshot["supervisor_id"] != discovery["instance"]):
            raise AssertionError("native owner identity changed during business fault observation")
        # Return only public identity; never persist the control credential.
        return snapshot

    def business_authority(self, pid: int):
        identity = self.identity()
        snapshot = self.native_status()
        generation = snapshot["generation"]
        uuid.UUID(generation)
        scope = STATE / "control/work" / generation
        record = json.loads((scope / "generation.json").read_text())
        if (snapshot["binding"]["component"] != "app-cli"
                or Path(snapshot["binding"]["resource"]).resolve(strict=True) != WORK.resolve(strict=True)
                or Path(identity["source_root"]).resolve(strict=True) != WORK.resolve(strict=True)
                or record["id"] != generation or record["supervisor"] != snapshot["supervisor_id"]
                or record["phase"] != "Running"):
            raise AssertionError("business scope does not belong to the captured workspace owner")
        if self.engine == "builtin":
            matches = []
            for path in (scope / "guardians").glob("*/receipt.json"):
                receipt = json.loads(path.read_text())
                if receipt.get("diagnostic_pid") == pid:
                    if (receipt["instance_id"] != generation or receipt["id"] != path.parent.name
                            or receipt["phase"] != "Running" or receipt.get("root_status") is not None):
                        raise AssertionError("business command guardian is not confirmed running in its original scope")
                    matches.append({"guardian_id": receipt["id"], "command_digest": receipt["command_digest"],
                                    "instance_id": receipt["instance_id"], "diagnostic_pid": pid})
            if len(matches) != 1:
                raise AssertionError("business PID is not uniquely registered in the captured generation")
            engine = matches[0]
        else:
            engine_receipt = json.loads((scope / "supervisord-engine.json").read_text())
            if (engine_receipt["generation"] != generation
                    or engine_receipt["supervisor_id"] != snapshot["supervisor_id"]
                    or engine_receipt["socket"] != "/var/run/supervisor.sock"):
                raise AssertionError("supervisord business engine belongs to a different owner scope")

            class UnixConnection(http.client.HTTPConnection):
                def connect(self):
                    self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                    self.sock.settimeout(3)
                    self.sock.connect(engine_receipt["socket"])

            class UnixTransport(xmlrpc.client.Transport):
                def make_connection(self, host):
                    return UnixConnection(host)

            with xmlrpc.client.ServerProxy("http://localhost/RPC2", transport=UnixTransport()) as client:
                supervisor_pid = client.supervisor.getPID()
                info = client.supervisor.getProcessInfo("app-svc-web")
                if (supervisor_pid != self.supervisor.pid or client.supervisor.getPID() != supervisor_pid
                        or info["name"] != "app-svc-web" or info["group"] != "app-svc-web"
                        or info["state"] != 20 or info["statename"] != "RUNNING" or info["pid"] != pid):
                    raise AssertionError("supervisord does not confirm the exact business process")
            # run-service execs Python in place. Bind that real process to its
            # original service spec without exposing unrelated environment keys.
            selected = {}
            for item in Path(f"/proc/{pid}/environ").read_bytes().split(b"\0"):
                key, separator, value = item.partition(b"=")
                if separator and key in (b"APP_RELEASE_ID", b"APP_SERVICE_ID"):
                    selected[key.decode()] = value.decode()
            release = selected.get("APP_RELEASE_ID", "")
            if not release or Path(release).name != release or selected.get("APP_SERVICE_ID") != "web":
                raise AssertionError("business process has no exact release/service identity")
            spec_path = Path("/run/official-pingap-specs") / release / "web.toml"
            spec_bytes = spec_path.read_bytes()
            spec = tomllib.loads(spec_bytes.decode())
            if (spec["release_id"] != release or spec["service_id"] != "web"
                    or spec["argv"] != ["python3", str(WORK / "web/server.py")]
                    or Path(spec["cwd"]).resolve(strict=True) != (WORK / "web").resolve(strict=True)):
                raise AssertionError("supervisord service spec differs from the captured business command")
            engine = {"supervisor_pid": supervisor_pid, "release_id": release,
                      "spec_sha256": hashlib.sha256(spec_bytes).hexdigest(),
                      **{key: info[key] for key in ("name", "group", "pid", "start")}}
        return {"runtime_instance_id": identity["runtime_instance_id"],
                "workspace_id": identity["workspace_id"], "generation": generation,
                "supervisor_id": snapshot["supervisor_id"], "engine": engine}

    def recover_business_crash(self, business, successful_operation, marker):
        pid = business["pid"]
        script = WORK / "web/server.py"
        python = shutil.which("python3")
        if not python:
            raise AssertionError("trusted workspace Python executable is unavailable")
        captured = self.process(pid)
        captured["cwd"] = str(Path(f"/proc/{pid}/cwd").resolve(strict=True))
        require_business_process(captured, Path(python), script)
        authority = self.business_authority(pid)
        self.check("business SIGKILL is bound to the current original owner scope", True,
                   pid=pid, start_ticks=captured["start_ticks"], **authority)
        # Keep the kernel process handle across the final scope and /proc
        # checks, so PID reuse cannot redirect this test's fault to a new task.
        if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
            raise AssertionError("Linux business fault injection requires exact pidfd signalling")
        process_handle = os.pidfd_open(pid)
        try:
            authority_stable = self.business_authority(pid) == authority
            actual = self.process(pid)
            actual["cwd"] = str(Path(f"/proc/{pid}/cwd").resolve(strict=True))
            self.check("business scope and OS incarnation stable before SIGKILL",
                       authority_stable and actual == captured)
            signal.pidfd_send_signal(process_handle, signal.SIGKILL)
        finally:
            os.close(process_handle)
        self.check("exact running business received SIGKILL", True, pid=pid,
                   start_ticks=captured["start_ticks"])

        def original_exited():
            try:
                return self.process(pid)["start_ticks"] != captured["start_ticks"]
            except FileNotFoundError:
                return True

        self.wait(original_exited, lambda exited: exited, 15)
        status, health = self.request("/health")
        self.check("original owner management survives business SIGKILL",
                   status == 200 and health["status"] == "alive"
                   and self.identity()["runtime_instance_id"] == authority["runtime_instance_id"])
        status, sources = self.request("/v1/logs/sources/query", "POST", {"selectors": [], "tail": 100})
        self.check("log sources remain readable after business SIGKILL",
                   status == 200 and sources["success"]
                   and any(source["service_id"] == "web" and source["matched_files"]
                           and not source.get("diagnostic") for source in sources["data"]))
        status, logs = self.request("/v1/logs/query", "POST", {"selectors": [{"service_id": "web"}], "tail": 100})
        self.check("actual business logs remain readable after SIGKILL",
                   status == 200 and logs["success"] and not logs["data"]["source_errors"]
                   and any(line["service_id"] == "web" for line in logs["data"]["logs"]))
        status, original = self.request(f"/v1/runtime/operations/{successful_operation['operation_id']}")
        self.check("business crash preserves the original successful operation",
                   status == 200 and original["success"] and original["data"] == successful_operation)
        replacement = self.operate("restart")
        recovered = self.ready(marker)
        self.check("explicit Restart restores real HTTP under the original owner",
                   replacement["operation_id"] != successful_operation["operation_id"]
                   and self.identity()["runtime_instance_id"] == authority["runtime_instance_id"]
                   and recovered["pid"] != pid, previous_pid=pid, current_pid=recovered["pid"])
        status, original = self.request(f"/v1/runtime/operations/{successful_operation['operation_id']}")
        self.check("new Restart does not rewrite the old operation terminal",
                   status == 200 and original["success"] and original["data"] == successful_operation)

    def proxy_receipt(self):
        return json.loads((STATE / "logs/pingap/official-proxy-confirmation.json").read_text())["confirmation"]

    def stop_once(self):
        """Verify the complete HTTP Stop path publishes standby exactly once."""
        log = STATE / "supervisord.log"
        starts_before = (log.read_text().count("spawned: 'app-pingap' with pid")
                         if self.engine == "supervisord" else None)
        previous = self.proxy_receipt()
        stopped = self.operate("stop", midwindow_readiness=True)
        current = self.proxy_receipt()
        self.check("Stop standby belongs to its original operation", current["operation_id"] == stopped["operation_id"] and
                   current["instance_id"] != previous["instance_id"])
        status, proxy = self.request("/v1/proxy/status")
        confirmation = proxy["data"].get("confirmation") if status == 200 else None
        self.check("Stop leaves a confirmed standby", bool(confirmation) and confirmation["confirmed"] and
                   confirmation["phase"] == "standby" and confirmation["operation_id"] == stopped["operation_id"])
        if starts_before is not None:
            starts_after = log.read_text().count("spawned: 'app-pingap' with pid")
            self.check("HTTP Stop performs exactly one supervisord standby spawn", starts_after - starts_before == 1,
                       starts_before=starts_before, starts_after=starts_after)
        # The durable terminal can precede Idle publication, and the observer
        # can retain its stopping snapshot for one second. Require convergence.
        status, readiness = self.wait(lambda: self.request("/v1/app/readiness"),
            lambda response: response[0] == 200
            and (response[1] or {}).get("data", {}).get("status") == "stopped"
            and response[1]["data"].get("ready") is False, seconds=5)
        self.check("settled Stop reports stopped", status == 200 and
                   readiness["data"]["status"] == "stopped" and readiness["data"]["ready"] is False,
                   response=readiness)
        return stopped

    def prepare_fixture(self):
        """Create the owned real service used by normal and fault acceptance."""
        WORK.mkdir(exist_ok=True)
        STATE.mkdir(exist_ok=True)
        (WORK / "web").mkdir(exist_ok=True)
        (STATE / "sentinel.txt").write_text("preserve-owned-container-data\n")
        sentinel = hashlib.sha256((STATE / "sentinel.txt").read_bytes()).hexdigest()
        (WORK / "marker.txt").write_text("A")
        proxy_config=("[upstreams.web]\naddrs=['rcoder://web']\n"
                      "[locations.web]\npath='/'\nupstream='web'\n"
                      "[servers.app]\naddr='0.0.0.0:9080'\nlocations=['web']\n")
        workspace_manifest="schema_version=1\n[workspace]\nname='official-acceptance'\n[pingap]\nmode='extend'\nconfig='proxy.extend.toml'\n"
        (WORK / "proxy.extend.toml").write_text("")
        (WORK / "workspace.manifest.toml").write_text(workspace_manifest)
        (WORK / "web/project.manifest.toml").write_text(
            "schema_version=1\n[project]\nservice_id='web'\nname='Web'\ntype='rust'\n"
            "[build]\ncommand=['true']\nartifact='unused.zip'\n"
            "[run]\ncommand=['python3','/workspace/web/server.py']\nshutdown_timeout_seconds=3\n"
            "[devrun]\ncommand=['python3','/workspace/web/server.py']\n"
            "[health]\nstartup_timeout_seconds=20\nreadiness_path='/health'\n"
            "[proxy]\npath='/'\n")
        (WORK / "web/server.py").write_text(
            "from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer\n"
            "import json,os\nfrom pathlib import Path\n"
            "class Handler(BaseHTTPRequestHandler):\n"
            " def do_GET(self):\n"
            "  body=json.dumps({'marker':Path('/workspace/marker.txt').read_text().strip(),'pid':os.getpid(),'port':int(os.environ['PORT'])}).encode()\n"
            "  self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)\n"
            "ThreadingHTTPServer(('0.0.0.0',int(os.environ['PORT'])),Handler).serve_forever()\n")
        version = subprocess.run([str(PINGAP), "--version"], capture_output=True, text=True, check=True)
        expected_version="pingap 0.15.0 (8270a1ebb7a238ea86fa220215714613410378bb, tls=openssl)"
        private = subprocess.run([str(PINGAP), "--apply-protocol-version"], capture_output=True, text=True)
        self.check("unmodified official 0.15.0 protocol", version.stdout.strip()==expected_version and private.returncode != 0,
                   version=(version.stdout + version.stderr).strip(),
                   binary_sha256=hashlib.sha256(PINGAP.read_bytes()).hexdigest())
        return sentinel

    def run(self):
        sentinel = self.prepare_fixture()
        self.start_owner()
        original_identity = self.identity()["runtime_instance_id"]
        self.operate("start"); self.ready()
        self.operate("start"); before_restart=self.ready()
        (WORK / "marker.txt").write_text("B")
        self.operate("restart"); after_restart=self.ready("B")
        self.check("Restart replaces the actual business process",after_restart["pid"]!=before_restart["pid"],
                   previous_pid=before_restart["pid"],current_pid=after_restart["pid"])
        self.stop_once()
        status, health = self.request("/health")
        self.check("management survives business Stop", status == 200 and health["status"] == "alive")
        self.operate("start"); self.ready("B")
        reload_id = str(uuid.uuid4())
        status, reload = self.request("/v1/proxy/reload", "POST", request_id=reload_id)
        self.check("proxy reload succeeds with exact request identity", status == 200 and reload["success"] and
                   reload["data"]["operation_id"] == reload_id)
        status, replay = self.request("/v1/proxy/reload", "POST", request_id=reload_id)
        self.check("same reload does not create a new proxy", status == 200 and replay["data"]["replayed"])
        self.ready("B")
        # Only the topology subcase uses Custom, whose app readiness deliberately
        # remains unsupported. Never turn that conservative result into Ready.
        (WORK / "proxy.custom.toml").write_text(proxy_config)
        (WORK / "workspace.manifest.toml").write_text(workspace_manifest.replace("mode='extend'","mode='custom'").replace("proxy.extend.toml","proxy.custom.toml"))
        self.operate("restart"); self.custom_serving("B")
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1",0))
            secondary_port=reservation.getsockname()[1]
        extension=proxy_config+f"[servers.additional]\naddr='127.0.0.1:{secondary_port}'\n"
        (WORK / "proxy.custom.toml").write_text(extension)
        changed_id=str(uuid.uuid4())
        status,changed=self.request("/v1/proxy/reload","POST",request_id=changed_id)
        self.check("official restart accepts changed listener topology",status==200 and changed["success"],
                   http_status=status,response=changed)
        self.verify_secondary_listener(secondary_port)
        self.custom_serving("B")
        old_proxy = self.proxy_receipt()
        (WORK / "proxy.custom.toml").write_text(extension+"[plugins.unsupported]\ncategory='not_in_official_binary'\n")
        status, rejected = self.request("/v1/proxy/reload", "POST", request_id=str(uuid.uuid4()))
        self.check("unsupported plugin rejected before stopping old proxy", status >= 400 and not rejected["success"] and
                   self.proxy_receipt()["process_id"] == old_proxy["process_id"],http_status=status,response=rejected)
        self.custom_serving("B")
        (WORK / "proxy.custom.toml").write_text(extension)
        self.reject_custom_preflight(
            "known basic_auth constructor failure",
            extension + "[plugins.invalid_auth]\ncategory='basic_auth'\n",
            ["pingap -t rejected candidate", "basic authorizations can't be empty"],
            "official_native_config_test.plugin_construction")
        (WORK / "proxy.custom.toml").write_text(extension)
        self.reject_custom_preflight(
            "malformed custom TOML",
            extension + "[plugins.broken\n",
            ["invalid Pingap source syntax, field, or value"],
            "candidate_source_parse")
        (WORK / "proxy.custom.toml").write_text(extension)
        custom_receipt=self.proxy_receipt()
        self.exact_kill(custom_receipt["process_id"],PINGAP,"custom proxy crash")
        self.wait(lambda:self.proxy_receipt(),lambda value:value["instance_id"]!=custom_receipt["instance_id"])
        self.custom_serving("B"); self.verify_secondary_listener(secondary_port)
        (WORK / "workspace.manifest.toml").write_text(workspace_manifest)
        self.operate("restart"); self.ready("B")
        original_server=(WORK / "web/server.py").read_text()
        (WORK / "web/server.py").write_text("import sys\nprint('controlled startup failure',flush=True)\nsys.exit(17)\n")
        self.operate("restart",succeeds=False)
        status,health=self.request("/health")
        self.check("management remains alive after real command exit 17",status==200 and health["status"]=="alive")
        (WORK / "web/server.py").write_text(original_server)
        successful = self.operate("restart"); business = self.ready("B")
        self.recover_business_crash(business, successful, "B")
        receipt = self.proxy_receipt()
        self.exact_kill(receipt["process_id"], PINGAP, "proxy crash")
        self.wait(lambda: self.proxy_receipt(), lambda value: value["instance_id"] != receipt["instance_id"])
        self.ready("B")
        if self.engine == "builtin":
            owner_pid = self.owner.pid
        else:
            owner_pid = int(subprocess.check_output(["supervisorctl", "-c", str(STATE / "supervisord.conf"), "pid", "app-cli"], text=True).strip())
        self.exact_kill(owner_pid, CLI, "owner crash")
        if self.engine == "builtin":
            self.owner.wait(timeout=10)
            self.start_owner()
        recovery_deadline = time.monotonic() + 90
        recovered_identity = self.wait(self.identity, lambda view: isinstance(view,dict) and view["runtime_instance_id"] != original_identity,
                                       max(0, recovery_deadline - time.monotonic()))
        self.wait_owner_initialized(recovered_identity, seconds=max(0, recovery_deadline - time.monotonic()))
        self.operate("restart"); self.ready("B")
        self.check("same state and sentinel after both crashes", hashlib.sha256((STATE / "sentinel.txt").read_bytes()).hexdigest() == sentinel)
        self.stop_once()
        self.evidence["passed"] = True
        self.save()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--engine", choices=["builtin", "supervisord"], required=True)
    parser.add_argument("--source-digest", required=True)
    args = parser.parse_args()
    runner = Acceptance(args.engine, args.source_digest)
    try:
        runner.run()
    except Exception as error:
        runner.evidence["error"] = str(error)
        runner.save()
        raise


if __name__ == "__main__":
    main()
