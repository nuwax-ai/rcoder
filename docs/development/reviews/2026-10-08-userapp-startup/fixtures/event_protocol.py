#!/usr/bin/env python3
"""真实 CLI、内核 owner 锁及受控 HTTP owner 的协议反例（仅 Unix）。

无真实业务进程，也不证明容器 E2E。binary 必须显式提供当前源码的构建产物；
--destination 必填，必须是不存在的自建目录，建议位于空间充足的工作盘。
脚本仅输出自建操作和事件；不继承数据库、平台或代理凭据。
"""

import argparse
import fcntl
import hashlib
import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import threading
import time


REPOSITORY = Path(__file__).resolve().parents[5]


def fixture_environment(root):
    home = root / "home"
    temporary = root / "tmp"
    home.mkdir(mode=0o700)
    temporary.mkdir(mode=0o700)
    # 只继承查找系统工具所需的 PATH；排除数据库/平台/代理配置及宿主 .pgpass。
    return {
        "PATH": os.environ.get("PATH", os.defpath),
        "HOME": str(home),
        "TMPDIR": str(temporary),
        "TMP": str(temporary),
        "TEMP": str(temporary),
        "LANG": "C.UTF-8",
        "PROJECT_ID": "event-review",
        "APP_CLI_STATE_ROOT": str(root / "state"),
        "APP_CLI_SKIP_PG_WAIT": "1",
        "APP_CLI_REQUIRE_PG": "0",
        "APP_CLI_PINGAP_RUNTIME_DIR": str(root / "pingap-runtime"),
    }


def exercise(binary, final_state, destination):
    root = destination / final_state
    root.mkdir(mode=0o700)
    source = root / "workspace"
    state = root / "state"
    source.mkdir()
    state.mkdir(mode=0o700)
    (source / "workspace.manifest.toml").write_text(
        "schema_version=1\n[workspace]\nname='event-review'\n", encoding="utf-8"
    )
    project = source / "web"
    project.mkdir()
    (project / "project.manifest.toml").write_text(
        "schema_version=1\n[project]\nservice_id='review-web'\n"
        "name='Review Web'\ntype='python'\n[build]\ncommand=['true']\n"
        "artifact='artifact.zip'\n[run]\ncommand=['python3','main.py']\n",
        encoding="utf-8",
    )
    # 此凭据只属于本协议替身，不是平台凭据；不记录 HTTP 请求头。
    (state / "token").write_text("isolated-event-fixture-token", encoding="utf-8")
    operation = {"id": None, "polls": 0}
    requests = []
    started = time.monotonic()
    identity = dict(
        application_id="event-review",
        service_family="userapp-dev",
        workspace_id="event-review-workspace",
        source_root=str(source.resolve()),
        runtime_instance_id="event-review-instance",
        deployment_generation_id="event-review-generation",
        protocol_version=5,
        capabilities=[],
    )

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def answer(self, data, code=200, envelope=True):
            body = json.dumps(
                dict(success=True, code="OK", message="ok", data=data)
                if envelope else data
            ).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            requests.append(dict(path=self.path, elapsed=time.monotonic() - started))
            if self.path == "/v1/runtime/identity":
                return self.answer(identity)
            if self.path == "/ready":
                return self.answer(
                    dict(status="not_ready", phase="Orchestrating"), 503, False
                )
            if self.path == "/v1/runtime/status":
                return self.answer(dict(
                    desired="running", observed="unknown", revision=3,
                    recovery_protection=False,
                    runtime_instance_id=identity["runtime_instance_id"],
                ))
            if "/events?" in self.path:
                common = dict(
                    operation_id=operation["id"],
                    runtime_instance_id=identity["runtime_instance_id"],
                )
                if self.path.endswith("after_seq=0"):
                    event = dict(
                        **common, sequence=1, stage="orchestration",
                        event_name="orchestration_done", payload=dict(failed=[]),
                    )
                else:
                    succeeded = final_state == "succeeded"
                    event = dict(
                        **common, sequence=2, stage="terminal",
                        event_name="Completed" if succeeded else "Failed",
                        payload=None if succeeded else dict(
                            code="ERR_CANCELLED",
                            error="Stop admitted before startup commit",
                        ),
                    )
                return self.answer(dict(operation_id=operation["id"], events=[event]))
            if self.path.startswith("/v1/runtime/operations/"):
                operation["polls"] += 1
                status = "starting" if operation["polls"] == 1 else final_state
                return self.answer(dict(
                    operation_id=operation["id"], kind="start", state=status,
                    request_digest="d" * 64, revision=4,
                    runtime_instance_id=identity["runtime_instance_id"],
                    error_code="ERR_CANCELLED" if status == "cancelled" else None,
                    error_message=("Stop admitted before startup commit"
                                   if status == "cancelled" else None),
                ))
            return self.answer(dict(unexpected=self.path), 404)

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            # 干净环境下不会产生 run_config；避免误把凭据写进证据。
            if body.get("run_config") is not None:
                return self.answer(dict(unexpected="fixture received credentials"), 400)
            requests.append(dict(path=self.path, body=body, elapsed=time.monotonic() - started))
            operation["id"] = body["operation_id"]
            return self.answer(dict(
                operation_id=operation["id"], state="accepted",
                poll="/v1/runtime/operations/" + operation["id"],
            ), 202)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with (state / "owner.lock").open("xb") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = subprocess.run([
                str(binary), "serve", "--workspace", str(source),
                "--log-dir", str(root / "logs"), "--admin-addr",
                "127.0.0.1:" + str(server.server_port),
                "--pingap-bin", "/bin/false",
            ], env=fixture_environment(root), capture_output=True, text=True, timeout=20)
        # 这里只持有协议替身自己的锁；没有真实 owner 或业务需 Shutdown。
        with (state / "owner.lock").open("rb") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        (root / "stdout.log").write_text(result.stdout, encoding="utf-8")
        (root / "stderr.log").write_text(result.stderr, encoding="utf-8")
        events = [
            json.loads(line.removeprefix("APP-CLI-EVT "))
            for line in result.stdout.splitlines() if line.startswith("APP-CLI-EVT ")
        ]
        proof = dict(
            binary=str(binary), case=final_state, exit=result.returncode,
            events=events, requests=requests, fixture_owner_lock_released=True,
        )
        (root / "proof.json").write_text(json.dumps(proof, indent=2), encoding="utf-8")
        return proof
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=3)


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
    destination = destination_arg.resolve()
    os.umask(0o077)
    destination.mkdir(parents=True, mode=0o700, exist_ok=False)
    binary = destination / "app-cli"
    shutil.copy2(binary_source, binary)
    identity = dict(
        repository=str(REPOSITORY), supplied_binary=str(binary_source),
        frozen_binary=str(binary),
        binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
        build_source_note="需由调用者确认 binary 对应的源码提交；本脚本不构建",
    )
    (destination / "binary-identity.json").write_text(json.dumps(identity, indent=2), encoding="utf-8")
    proofs = [exercise(binary, final, destination) for final in ["succeeded", "cancelled"]]
    print(json.dumps(dict(destination=str(destination), proofs=proofs), indent=2))


if __name__ == "__main__":
    main()
