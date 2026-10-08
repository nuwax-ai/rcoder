#!/usr/bin/env python3
"""自建真实 owner、HTTP 业务及 Stop 的提交窗口反例（Python 3.11+ / Unix）。

仅 Pingap 为受控替身，并显式启用 APP_CLI_SKIP_PINGAP_CONFIRM；这不是完整
Pingap 或容器 E2E。binary 和新的 --destination 必须显式提供；不继承平台、
数据库、代理凭据，不碰已有 owner。正常与失败路径均通过捕获的 native
实例、业务代次 Shutdown，确认同一个稳定 owner.lock 释放；不按裸 PID 杀进程。
"""

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import queue
import shutil
import socket
import subprocess
import sys
import threading
import time
import tomllib
import urllib.request
import uuid


REPOSITORY = Path(__file__).resolve().parents[5]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", help="当前源码已构建的 app-cli 可执行文件")
    parser.add_argument("--destination", required=True, help="不存在的自建工作盘目录")
    args = parser.parse_args()
    binary_source = Path(args.binary).expanduser().resolve(strict=True)
    if not binary_source.is_file() or not os.access(binary_source, os.X_OK):
        parser.error("binary 必须是可执行文件")
    destination_arg = Path(args.destination).expanduser()
    if destination_arg.exists() or destination_arg.is_symlink():
        parser.error("destination 已存在；请指定新的自建目录")
    root = destination_arg.resolve()
    os.umask(0o077)
    root.mkdir(parents=True, mode=0o700, exist_ok=False)
    binary = root / "app-cli"
    shutil.copy2(binary_source, binary)
    identity = dict(
        repository=str(REPOSITORY), supplied_binary=str(binary_source),
        frozen_binary=str(binary),
        binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
        build_source_note="需由调用者确认 binary 对应的源码提交；本脚本不构建",
    )
    (root / "binary-identity.json").write_text(json.dumps(identity, indent=2), encoding="utf-8")
    workspace = root / "workspace"
    project = workspace / "web"
    state = root / "state"
    project.mkdir(parents=True)
    (workspace / "workspace.manifest.toml").write_text(
        "schema_version=1\n[workspace]\nname='event-stop-review'\n"
        "[health]\nbridge_service='review-web'\n", encoding="utf-8",
    )
    (project / "project.manifest.toml").write_text(
        "schema_version=1\n[project]\nservice_id='review-web'\n"
        "name='Review HTTP'\ntype='python'\n[build]\ncommand=['true']\n"
        "artifact='artifact.zip'\n[run]\ncommand=["
        + json.dumps(sys.executable)
        + ",'main.py']\nshutdown_timeout_seconds=3\n[health]\n"
        "startup_path='/health'\nreadiness_path='/ready'\nliveness_path='/health'\n"
        "[proxy]\npath='/'\nstrip_prefix=false\n", encoding="utf-8",
    )
    # 首次 ready 供 legacy startup probe 通过，后续不 ready 留出真实 bridge 窗口。
    (project / "main.py").write_text(
        "import http.server,os\n"
        "class H(http.server.BaseHTTPRequestHandler):\n"
        " ready_checks=0\n"
        " def do_GET(self):\n"
        "  status=200\n"
        "  if self.path == '/ready':\n"
        "   H.ready_checks+=1\n"
        "   status=200 if H.ready_checks == 1 else 503\n"
        "  self.send_response(status);self.end_headers();self.wfile.write(b'review-http')\n"
        " def log_message(self,*args): pass\n"
        "http.server.HTTPServer(('127.0.0.1',int(os.environ['PORT'])),H).serve_forever()\n",
        encoding="utf-8",
    )
    pingap = root / "fake-pingap"
    pingap.write_text(
        "#!/bin/sh\ncase \" $* \" in *\" -t \"*) exit 0;; esac\nexec sleep 300\n",
        encoding="utf-8",
    )
    pingap.chmod(0o755)
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        admin_port = reservation.getsockname()[1]
    address = "127.0.0.1:" + str(admin_port)
    home = root / "home"
    temporary = root / "tmp"
    home.mkdir(mode=0o700)
    temporary.mkdir(mode=0o700)
    env = {
        "PATH": os.environ.get("PATH", os.defpath),
        "HOME": str(home),
        "TMPDIR": str(temporary), "TMP": str(temporary), "TEMP": str(temporary),
        "LANG": "C.UTF-8",
        "PROJECT_ID": "event-stop-review", "APP_CLI_STATE_ROOT": str(state),
        "APP_CLI_SKIP_PG_WAIT": "1", "APP_CLI_REQUIRE_PG": "0",
        "APP_CLI_PINGAP_RUNTIME_DIR": str(root / "pingap-runtime"),
        "APP_CLI_SKIP_PINGAP_CONFIRM": "1",
    }
    lock_result = subprocess.run(
        [str(binary), "gen-lock", "--workspace", str(workspace)],
        env=env, capture_output=True, text=True, timeout=20,
    )
    (root / "gen-lock.log").write_text(lock_result.stdout + lock_result.stderr, encoding="utf-8")
    if lock_result.returncode != 0:
        raise RuntimeError("gen-lock failed; inspect this fixture's gen-lock.log")
    release = tomllib.loads((workspace / "release.lock.toml").read_text(encoding="utf-8"))
    service_port = release["services"][0]["port"]
    # 预检发现真实端口占用就拒绝；不按端口处理已有服务。
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", service_port))
    stdout_events = queue.Queue()
    started = time.monotonic()
    lines = []
    process = None
    proof = dict(
        binary=str(binary), destination=str(root),
        fixture="real app-cli owner/business HTTP; fake Pingap and explicit skip-confirm",
    )
    # 即使宿主配置了 HTTP_PROXY，也不把本 fixture 管理 token 发给代理。
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def request(path, body=None):
        token = (state / "token").read_text(encoding="utf-8").strip()
        query = urllib.request.Request(
            "http://" + address + path,
            data=None if body is None else json.dumps(body).encode(),
            headers={"Content-Type": "application/json", "X-Deploy-Token": token},
        )
        with opener.open(query, timeout=4) as response:
            return json.load(response)

    def read_stdout():
        for line in process.stdout:
            lines.append(dict(elapsed=time.monotonic() - started, line=line.rstrip()))
            if line.startswith("APP-CLI-EVT "):
                stdout_events.put(json.loads(line.removeprefix("APP-CLI-EVT ")))

    def poll_original(operation_id, timeout=15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            result = request("/v1/runtime/operations/" + operation_id)["data"]
            if result["state"] in ["succeeded", "failed", "cancelled", "recovery_required"]:
                return result
            time.sleep(0.05)
        raise RuntimeError("original operation did not become terminal")

    def native_shutdown():
        record = json.loads((state / "supervisor.json").read_text(encoding="utf-8"))
        host, port = record["address"].rsplit(":", 1)
        if host != "127.0.0.1":
            raise RuntimeError("fixture native control is not on loopback; refusing cleanup")
        message = dict(
            version=2, instance=record["instance"], token=record["token"],
            request=dict(request_id=str(uuid.uuid4()), action="status", expected_generation=None),
        )

        def call(message):
            with socket.create_connection((host, int(port)), timeout=4) as connection:
                connection.settimeout(25)
                connection.sendall(json.dumps(message).encode() + b"\n")
                with connection.makefile("rb") as reader:
                    return json.loads(reader.readline())

        before = call(message)
        snapshot = before["snapshot"]
        if before["instance"] != record["instance"]:
            raise RuntimeError("native owner instance changed; refusing cleanup")
        binding = snapshot["binding"]
        if binding["component"] != "app-cli" or Path(binding["resource"]).resolve() != workspace.resolve():
            raise RuntimeError("native owner binding changed; refusing cleanup")
        generation = snapshot.get("generation")
        proof["shutdown_captured_generation"] = generation
        message["request"].update(
            request_id=str(uuid.uuid4()), action="shutdown",
            # 空值精确表示无业务代次，不能使用 None（旧协议的无限制请求）。
            expected_generation=generation or "",
        )
        result = call(message)
        if result["instance"] != record["instance"] or result.get("error") is not None:
            raise RuntimeError("captured native shutdown was not confirmed; preserve fixture state")
        proof["shutdown_captured_instance"] = result["instance"]
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            with (state / "owner.lock").open("rb") as lock:
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    proof["owner_lock_released"] = True
                    return
                except BlockingIOError:
                    pass
            time.sleep(0.05)
        raise RuntimeError("captured fixture owner did not exit; preserve fixture state")

    try:
        with (root / "stderr.log").open("w", encoding="utf-8") as error_file:
            process = subprocess.Popen([
                str(binary), "run", "--workspace", str(workspace),
                "--log-dir", str(root / "logs"), "--admin-addr", address,
                "--pingap-bin", str(pingap),
            ], env=env, stdout=subprocess.PIPE, stderr=error_file, text=True)
            reader = threading.Thread(target=read_stdout, daemon=True)
            reader.start()
            observed = []
            deadline = time.monotonic() + 25
            while time.monotonic() < deadline:
                try:
                    event = stdout_events.get(timeout=max(0.01, deadline - time.monotonic()))
                except queue.Empty as error:
                    raise RuntimeError(
                        "no Done before bridge readiness; inspect fixture logs and current code"
                    ) from error
                observed.append(event)
                if event.get("event") == "orchestration_done":
                    break
            else:
                raise RuntimeError("no Done before bridge readiness")
            status = request("/v1/runtime/status")["data"]
            operation_id = status.get("active_operation_id")
            if not operation_id:
                raise RuntimeError("Done arrived with no active operation; re-evaluate this counterexample")
            proof["events_before_stop"] = observed
            proof["original_before_stop"] = request("/v1/runtime/operations/" + operation_id)["data"]
            with opener.open("http://127.0.0.1:" + str(service_port) + "/health", timeout=3) as response:
                proof["real_http_before_stop"] = dict(status=response.status, body=response.read().decode())
            runtime_identity = request("/v1/runtime/identity")["data"]
            stop_id = "review-stop-" + uuid.uuid4().hex
            accepted = request("/v1/runtime/operations", dict(
                operation_id=stop_id,
                expected_runtime_instance_id=runtime_identity["runtime_instance_id"],
                expected_revision=status["revision"],
                workspace_id=runtime_identity["workspace_id"], kind="stop",
                profile=dict(profile="source", input=dict(workspace_id=runtime_identity["workspace_id"])),
            ))
            proof["stop_receipt"] = accepted["data"]
            proof["original_final"] = poll_original(operation_id)
            proof["stop_final"] = poll_original(stop_id)
            process.wait(timeout=10)
            reader.join(timeout=3)
            proof["client_exit"] = process.returncode
            with socket.socket() as probe:
                probe.settimeout(1)
                proof["business_port_closed"] = probe.connect_ex(("127.0.0.1", service_port)) != 0
    finally:
        try:
            if (state / "supervisor.json").is_file():
                native_shutdown()
            if process and process.poll() is None:
                process.wait(timeout=10)
        finally:
            proof["stdout_lines"] = lines
            (root / "proof.json").write_text(json.dumps(proof, indent=2), encoding="utf-8")
            print(json.dumps(proof, indent=2))


if __name__ == "__main__":
    main()
