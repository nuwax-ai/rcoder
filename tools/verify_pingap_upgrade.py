#!/usr/bin/env python3
"""真实 app-cli/Pingap 配对协议验证；隔离 Docker loopback，不调用 AI。

宿主 app-cli 生成 release.lock 和三模式配置；经指定 SHA256 校验的官方
Linux full 资产在隔离容器内执行 -t、admin、HTTP、WebSocket/HMR、SSE。
--linux-app-cli 可追加旧 metadata 的真实 serve/Stop/Start 恢复验证。
结果记录每个实际请求和断言；协议测试不是 Compose/真实 AI E2E。
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import secrets
import signal
import shutil
import socket
import struct
import subprocess
import sys
import tarfile
import threading
import time
import tomllib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

VERSION = "0.15.0"
COMMIT = "8270a1ebb7a238ea86fa220215714613410378bb"
OLD_VERSION = "0.14.3"
OLD_COMMIT = "cd74a461a3e778ae83f7c4dd7fd03ea483f3e3e8"
PREFIX_START = "──── pingap.toml ────\n"
PREFIX_END = "──── end ────"


def run(command, *, env=None, timeout=120, cwd=None):
    result = subprocess.run(command, cwd=cwd, env=env, text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            timeout=timeout, check=False)
    if result.returncode:
        raise RuntimeError(f"command failed ({result.returncode}): {command}\n{result.stdout}")
    return result.stdout


def checked_binary(asset: Path, checksum: str, destination: Path, binary_member="pingap"):
    with asset.open("rb") as source:
        actual = hashlib.file_digest(source, "sha256").hexdigest()
    if not re.fullmatch(r"[a-f0-9]{64}", checksum) or actual != checksum:
        raise ValueError(f"official asset SHA256 mismatch: expected={checksum}, observed={actual}")
    with tarfile.open(asset) as archive:
        members = [member for member in archive.getmembers()
                   if member.isfile() and Path(member.name).name == binary_member]
        if len(members) != 1:
            raise ValueError("official asset must contain exactly one regular Pingap binary")
        source = archive.extractfile(members[0])
        if source is None:
            raise ValueError("Pingap binary is not readable")
        destination.write_bytes(source.read())
    destination.chmod(0o755)
    return actual


def compiled_config(stdout: str):
    match = re.search(r"expected hash = ([A-Fa-f0-9]+)\)", stdout)
    if not match or stdout.count(PREFIX_START) != 1 or stdout.count(PREFIX_END) != 1:
        raise ValueError("app-cli gen-lock did not return one config and expected hash")
    return stdout.split(PREFIX_START, 1)[1].split(PREFIX_END, 1)[0], match.group(1)


def write_workspace(root: Path, mode: str):
    root.mkdir(parents=True)
    config = 'config = "custom.toml"\n' if mode != "managed" else ""
    (root / "workspace.manifest.toml").write_text(
        f'schema_version = 1\n[workspace]\nname = "pingap-upgrade-{mode}"\n'
        f'[pingap]\nmode = "{mode}"\n{config}', encoding="utf-8")
    for service, path, strip in [("root", "/", False), ("api", "/api", True),
                                  ("kept", "/keep", False)]:
        directory = root / service
        directory.mkdir()
        plugins = '["upgradeHeaders"]' if mode == "extend" else "[]"
        (directory / "project.manifest.toml").write_text(
            f'schema_version = 1\n[project]\nservice_id = "{service}"\n'
            f'name = "{service}"\ntype = "python"\n'
            '[build]\ncommand = ["python3", "--version"]\nartifact = "."\n'
            '[run]\ncommand = ["python3", "server.py"]\n'
            f'[proxy]\npath = "{path}"\nstrip_prefix = {str(strip).lower()}\n'
            f'plugins = {plugins}\n', encoding="utf-8")
        # A real owned service for the optional app-cli lifecycle path.
        (directory / "server.py").write_text(
            'import os,runpy\n'
            'runpy.run_path("/verification/verify_pingap_upgrade.py",run_name="service")\n',
            encoding="utf-8")
    if mode == "extend":
        (root / "custom.toml").write_text(
            '[plugins.upgradeHeaders]\ncategory = "response_headers"\n'
            'set_headers = ["X-Upgrade-Fixture:extend"]\n', encoding="utf-8")
    elif mode == "custom":
        (root / "custom.toml").write_text('''[servers.app]
addr = "0.0.0.0:9080"
locations = ["rootLocation", "apiLocation", "keptLocation"]
[upstreams.root]
addrs = ["rcoder://root"]
health_check = "http://root/health"
[upstreams.api]
addrs = ["rcoder://api"]
health_check = "http://api/health"
[upstreams.kept]
addrs = ["rcoder://kept"]
health_check = "http://kept/health"
[locations.rootLocation]
upstream = "root"
plugins = ["stripOrigin"]
[locations.apiLocation]
upstream = "api"
path = "/api"
rewrite = "^/api(?:/|$)(.*) /$1"
plugins = ["stripOrigin"]
[locations.keptLocation]
upstream = "kept"
path = "/keep"
plugins = ["stripOrigin"]
[plugins.stripOrigin]
category = "response_headers"
remove_headers = ["X-Pingap-EType"]
''', encoding="utf-8")


def prepare(args):
    catalog_path = Path(__file__).resolve().parent / "build/pingap-assets.json"
    catalog = json.loads(catalog_path.read_text())
    release = catalog["releases"][VERSION]
    if catalog["repository"] != "vicanso/pingap" or release["commit"] != COMMIT:
        raise ValueError("trusted official catalog disagrees with the planned source identity")
    trusted_asset = release["assets"][args.arch]
    if args.asset_sha256 != trusted_asset["sha256"]:
        raise ValueError("--asset-sha256 must match the checked-in official release catalog")
    image_info = json.loads(run(["docker", "image", "inspect", args.image]))[0]
    if image_info["Architecture"] != args.arch:
        raise ValueError("Docker image architecture disagrees with the selected asset")
    image_id = image_info["Id"]
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    actual = checked_binary(args.asset, args.asset_sha256, root / "pingap",
                            trusted_asset["name"].removesuffix(".tar.gz"))
    generated = []
    clean_env = {key: value for key, value in os.environ.items()
                 if not key.startswith(("APP_CLI_", "APP_DEPLOY_", "RCODER_PINGAP_"))}
    for mode in ("managed", "extend", "custom"):
        workspace = root / mode
        write_workspace(workspace, mode)
        command = [str(args.app_cli.resolve()), "gen-lock", "--workspace", str(workspace)]
        output = run(command, env=clean_env)
        (workspace / "gen-lock.log").write_text(output, encoding="utf-8")
        config, expected_hash = compiled_config(output)
        (workspace / "pingap.toml").write_text(config, encoding="utf-8")
        lock = tomllib.loads((workspace / "release.lock.toml").read_text())
        if (lock["pingap"]["version"], lock["pingap"]["commit"]) != (VERSION, COMMIT):
            raise ValueError(f"new app-cli identity disagrees with official asset: {lock['pingap']}")
        generated.append({"mode": mode, "hash": expected_hash,
                          "services": [{"service": service["service_id"], "port": service["port"]}
                                       for service in lock["services"]]})
    manifest = {"asset": args.asset.name, "asset_sha256": actual,
                "trusted_official_asset": trusted_asset,
                "app_cli_sha256": hashlib.file_digest(args.app_cli.open("rb"), "sha256").hexdigest(),
                "pingap_binary_sha256": hashlib.sha256((root / "pingap").read_bytes()).hexdigest(),
                "expected_version": VERSION, "expected_commit": COMMIT,
                "arch": args.arch, "image": args.image, "image_id": image_id, "generated": generated,
                "scope": "protocol-and-lifecycle" if args.linux_app_cli else "protocol-only"}
    if args.linux_app_cli:
        with args.linux_app_cli.open("rb") as binary:
            manifest["linux_app_cli_sha256"] = hashlib.file_digest(binary, "sha256").hexdigest()
        if args.linux_build_inputs:
            manifest["linux_build_inputs_sha256"] = hashlib.sha256(
                args.linux_build_inputs.read_bytes()).hexdigest()
    if args.old_linux_app_cli:
        manifest["old_image_id"] = args.old_image_id
        manifest["old_app_cli_sha256"] = hashlib.sha256(args.old_linux_app_cli.read_bytes()).hexdigest()
        manifest["old_pingap_sha256"] = hashlib.sha256(args.old_pingap_bin.read_bytes()).hexdigest()
    (root / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    container_name = f"pingap-upgrade-{args.arch}-{secrets.token_hex(6)}"
    command = ["docker", "run", "--rm", "--name", container_name,
               "--network", "none", "--platform", f"linux/{args.arch}",
               "--entrypoint", "python3", "-v", f"{root}:/verification",
               "-v", f"{Path(__file__).resolve()}:/verification/verify_pingap_upgrade.py:ro"]
    if args.linux_app_cli:
        command += ["-v", f"{args.linux_app_cli.resolve()}:/verification/app-cli:ro"]
    if args.old_linux_app_cli:
        command += ["-v", f"{args.old_linux_app_cli.resolve()}:/verification/old-app-cli:ro",
                    "-v", f"{args.old_pingap_bin.resolve()}:/verification/old-pingap:ro"]
    command += [image_id, "/verification/verify_pingap_upgrade.py", "--inside"]
    (root / "docker-command.json").write_text(json.dumps(command) + "\n")
    try:
        result = subprocess.run(command, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                timeout=480, check=False)
    finally:
        subprocess.run(["docker", "rm", "-f", container_name], stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, timeout=30, check=False)
    (root / "docker.log").write_text(result.stdout, encoding="utf-8")
    print(result.stdout)
    print(f"Evidence: {root}")
    return result.returncode


def request(port, path, headers=None, method="GET", body=None):
    client = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        client.request(method, path, body=body, headers=headers or {})
        response = client.getresponse()
        return {"status": response.status,
                "headers": {name.lower(): value for name, value in response.getheaders()},
                "body": response.read().decode("utf-8", errors="replace")}
    finally:
        client.close()


def exact_read(connection, size):
    data = b""
    while len(data) < size:
        chunk = connection.recv(size - len(data))
        if not chunk:
            raise EOFError("WebSocket closed before complete frame")
        data += chunk
    return data


def websocket_frame(connection):
    first, second = exact_read(connection, 2)
    length = second & 0x7f
    if length == 126:
        length = struct.unpack("!H", exact_read(connection, 2))[0]
    elif length == 127:
        length = struct.unpack("!Q", exact_read(connection, 8))[0]
    mask = exact_read(connection, 4) if second & 0x80 else None
    payload = exact_read(connection, length)
    if mask:
        payload = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
    return first & 0x0f, payload


class ControlledBackend(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def do_GET(self):
        if self.path == "/health":
            self.server.health_observations.append({"time": time.monotonic(),
                                                     "status": 200 if self.server.healthy else 503})
            status, body = (200, b"healthy") if self.server.healthy else (503, b"not ready")
        elif self.headers.get("Upgrade", "").lower() == "websocket":
            key = self.headers["Sec-WebSocket-Key"]
            accept = base64.b64encode(hashlib.sha1(
                (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
            self.send_response(101)
            self.send_header("Upgrade", "websocket")
            self.send_header("Connection", "Upgrade")
            self.send_header("Sec-WebSocket-Accept", accept)
            self.send_header("Sec-WebSocket-Protocol", "vite-hmr")
            self.end_headers()
            self.wfile.flush()
            opcode, payload = websocket_frame(self.connection)
            if opcode != 1:
                return
            reply = json.dumps({"service": self.server.service, "path": self.path,
                                "host": self.headers["Host"], "echo": payload.decode()}).encode()
            self.connection.sendall(bytes([0x81, len(reply)]) + reply)
            self.close_connection = True
            return
        elif self.path.startswith("/events") or self.path.startswith("/keep/events"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(b"event: fixture\ndata: first\n\n")
            self.wfile.flush()
            time.sleep(1.0)
            self.wfile.write(b"event: fixture\ndata: second\n\n")
            self.wfile.flush()
            self.close_connection = True
            return
        else:
            status = 502 if self.path.endswith("/app-error") else 200
            body = json.dumps({"service": self.server.service, "path": self.path,
                               "host": self.headers["Host"],
                               "forwarded_host": self.headers.get("X-Forwarded-Host"),
                               "forwarded_proto": self.headers.get("X-Forwarded-Proto")}).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("X-Pingap-EType", "forged-by-application")
        self.end_headers()
        self.wfile.write(body)


def backend(port, service, healthy=True):
    server = ThreadingHTTPServer(("127.0.0.1", port), ControlledBackend)
    server.daemon_threads = True
    server.service, server.healthy = service, healthy
    server.health_observations = []
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def stop_process(process):
    if process.poll() is None:
        process.send_signal(signal.SIGTERM)
        try:
            process.wait(timeout=12)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def wait_for(predicate, *, seconds=45):
    deadline = time.monotonic() + seconds
    last = None
    while time.monotonic() < deadline:
        try:
            last = predicate()
            if last:
                return last
        except (OSError, ValueError, http.client.HTTPException):
            pass
        time.sleep(0.1)
    raise AssertionError(f"condition did not become true in {seconds}s; last={last}")


def health_snapshot(admin, count, healthy):
    snapshot = admin()
    values = snapshot.get("upstream_healthy_status", {})
    if len(values) == count and all(value["healthy"] == healthy and value["total"] == 1
                                    for value in values.values()):
        return snapshot
    return None


def websocket_request(path):
    with socket.create_connection(("127.0.0.1", 9080), timeout=5) as connection:
        key = base64.b64encode(secrets.token_bytes(16)).decode()
        connection.sendall((f"GET {path} HTTP/1.1\r\nHost: upgrade.test:9080\r\n"
                            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
                            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
                            "Sec-WebSocket-Protocol: vite-hmr\r\n\r\n").encode())
        headers = b""
        while not headers.endswith(b"\r\n\r\n"):
            headers += exact_read(connection, 1)
        expected = base64.b64encode(hashlib.sha1(
            (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest())
        assert headers.startswith(b"HTTP/1.1 101"), headers.decode()
        assert expected.lower() in headers.lower(), headers.decode()
        assert b"sec-websocket-protocol: vite-hmr" in headers.lower(), headers.decode()
        payload = b"upgrade-hmr-payload"
        mask = secrets.token_bytes(4)
        connection.sendall(bytes([0x81, 0x80 | len(payload)]) + mask
                           + bytes(value ^ mask[index % 4] for index, value in enumerate(payload)))
        opcode, reply = websocket_frame(connection)
        assert opcode == 1, opcode
        return {"status": 101, "body": json.loads(reply)}


def sse_request(path):
    client = http.client.HTTPConnection("127.0.0.1", 9080, timeout=5)
    try:
        client.request("GET", path, headers={"Host": "upgrade.test:9080"})
        response = client.getresponse()
        assert response.status == 200, response.status
        assert response.getheader("Content-Type") == "text/event-stream"
        first = response.readline() + response.readline() + response.readline()
        first_time = time.monotonic()
        second = response.readline() + response.readline() + response.readline()
        delta = time.monotonic() - first_time
        assert first == b"event: fixture\ndata: first\n\n", first
        assert second == b"event: fixture\ndata: second\n\n", second
        assert delta >= 0.7, f"SSE buffered first event until response completion: delta={delta}"
        return {"status": response.status, "first": first.decode(), "second": second.decode(),
                "event_gap_seconds": delta}
    finally:
        client.close()


def inside():
    root = Path("/verification")
    manifest = json.loads((root / "manifest.json").read_text())
    report = {"identity": manifest, "scenarios": [], "status": "running",
              "selected": 25 + (manifest["scope"] == "protocol-and-lifecycle")}
    results = report["scenarios"]

    def case(name, mode, action):
        started = time.monotonic()
        try:
            detail = action()
            results.append({"name": name, "mode": mode, "status": "pass", "detail": detail,
                            "elapsed_seconds": round(time.monotonic() - started, 3)})
            return True
        except Exception as error:
            results.append({"name": name, "mode": mode, "status": "fail", "error": str(error),
                            "elapsed_seconds": round(time.monotonic() - started, 3)})
            return False

    try:
        version = run([str(root / "pingap"), "--version"])
        assert version.strip() == f"pingap {VERSION} ({COMMIT}, tls=openssl)", version
        report["binary_version"] = version.strip()
        report["node_version"] = run(["node", "--version"]).strip()
        assert report["node_version"] == "v22.23.2", report["node_version"]
        linked = subprocess.run(["ldd", str(root / "pingap")], text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=False)
        report["linkage"] = linked.stdout
        assert "not found" not in linked.stdout, linked.stdout
        for generated in manifest["generated"]:
            mode = generated["mode"]
            config = root / mode / "pingap.toml"
            if not case("same-version-config-test", mode,
                 lambda: {"command": ["pingap", "-t", "-c", str(config)],
                          "output": run([str(root / "pingap"), "-t", "-c", str(config)])}):
                continue
            servers = [backend(service["port"], service["service"], healthy=mode != "managed")
                       for service in generated["services"]]
            user, password = secrets.token_hex(16), secrets.token_hex(16)
            env = os.environ.copy()
            env.update({"PINGAP_ADMIN_ADDR": "127.0.0.1:3018", "PINGAP_ADMIN_USER": user,
                        "PINGAP_ADMIN_PASSWORD": password})
            def admin():
                timestamp = str(int(time.time()))
                token = hashlib.sha256(f"{user}:{password}:{timestamp}".encode()).hexdigest()
                response = request(3018, "/api/basic", {"Authorization": f"{token}:{timestamp}"})
                assert response["status"] == 200, response
                return json.loads(response["body"])
            with (root / mode / "pingap.log").open("w") as log:
                process = subprocess.Popen([str(root / "pingap"), "-c", str(config), "--autoreload"],
                                           env=env, stdout=log, stderr=subprocess.STDOUT)
                try:
                    wait_for(lambda: admin())
                    def authenticate():
                        anonymous = request(3018, "/api/basic")
                        assert anonymous["status"] == 401, anonymous
                        wrong = request(3018, "/api/basic", {"Authorization": "invalid:0"})
                        assert wrong["status"] == 401, wrong
                        actual = admin()
                        assert actual["config_hash"].upper() == generated["hash"].upper(), actual
                        return {"anonymous_status": anonymous["status"], "wrong_status": wrong["status"],
                                "config_hash": actual["config_hash"], "expected_hash": generated["hash"]}
                    case("admin-authentication-and-hash", mode, authenticate)
                    if mode == "managed":
                        def first_check():
                            unhealthy = wait_for(lambda: health_snapshot(admin, len(servers), 0))
                            response = request(9080, "/api/echo")
                            assert response["status"] == 503, response
                            assert response["headers"].get("x-pingap-etype"), response
                            assert all(server.health_observations for server in servers)
                            observations = [{"service": server.service, "first": server.health_observations[0]}
                                            for server in servers]
                            for server in servers:
                                server.healthy = True
                            healthy = wait_for(lambda: health_snapshot(admin, len(servers), 1))
                            return {"initial": unhealthy["upstream_healthy_status"],
                                    "unavailable_request": response, "first_health_requests": observations,
                                    "recovered": healthy["upstream_healthy_status"]}
                        case("initial-healthcheck-and-recovery", mode, first_check)
                    wait_for(lambda: health_snapshot(admin, len(servers), 1))
                    for path, service, rewritten in [("/hello?value=1", "root", "/hello?value=1"),
                        ("/api/echo?value=1", "api", "/echo?value=1"),
                        ("/keep/echo", "kept", "/keep/echo")]:
                        def routing(path=path, service=service, rewritten=rewritten):
                            response = request(9080, path, {"Host": "upgrade.test:9080"})
                            assert response["status"] == 200, response
                            body = json.loads(response["body"])
                            assert (body["service"], body["path"], body["host"]) == (
                                service, rewritten, "upgrade.test:9080"), response
                            assert "x-pingap-etype" not in response["headers"], response
                            if mode != "custom":
                                assert body["forwarded_host"] is None, response
                                assert body["forwarded_proto"] == "http", response
                            if mode == "extend":
                                assert response["headers"].get("x-upgrade-fixture") == "extend", response
                            return {"request": {"path": path, "host": "upgrade.test:9080"},
                                    "response": response}
                        case(f"http-route-{service}-host-rewrite", mode, routing)
                    def origin():
                        response = request(9080, "/api/app-error")
                        assert response["status"] == 502, response
                        assert json.loads(response["body"])["service"] == "api", response
                        assert "x-pingap-etype" not in response["headers"], response
                        return {"request": "/api/app-error", "response": response}
                    case("application-error-origin-stripped", mode, origin)
                    def hmr():
                        response = websocket_request("/api/hmr")
                        assert response["body"] == {"service": "api", "path": "/hmr",
                            "host": "upgrade.test:9080", "echo": "upgrade-hmr-payload"}, response
                        return {"request": "/api/hmr", "response": response}
                    case("websocket-vite-hmr-upgrade-and-frame", mode, hmr)
                    case("sse-events-delivered-before-completion", mode,
                         lambda: {"request": "/api/events", "response": sse_request("/api/events")})
                finally:
                    stop_process(process)
                    for server in servers:
                        server.shutdown()
                        server.server_close()
        if manifest["scope"] == "protocol-and-lifecycle":
            case("old-release-metadata-preserved-stop-start", "managed", lambda: lifecycle(root))
        report["status"] = "pass" if len(results) == report["selected"] and all(
            result["status"] == "pass" for result in results) else "fail"
    except Exception as error:
        report["status"] = "blocked"
        report["error"] = str(error)
    (root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    for result in results:
        print(f"{result['status']}: {result['mode']}/{result['name']}" +
              (f" ({result['error']})" if result["status"] != "pass" else ""))
    print(f"status={report['status']}, selected={report['selected']}, executed={len(results)}, "
          f"passed={sum(result['status'] == 'pass' for result in results)}, "
          f"failed={sum(result['status'] == 'fail' for result in results)}")
    if "error" in report:
        print(report["error"])
    return 0 if report["status"] == "pass" else 1


def lifecycle(root):
    binary_version = run([str(root / "app-cli"), "--version"]).strip()
    manifest = json.loads((root / "manifest.json").read_text())
    has_old_pair = "old_app_cli_sha256" in manifest
    workspace = root / "old-release"
    shutil.copytree(root / "managed", workspace)
    path = workspace / "release.lock.toml"
    if has_old_pair:
        old_binary_version = run([str(root / "old-app-cli"), "--version"]).strip()
        old_pingap_version = run([str(root / "old-pingap"), "--version"]).strip()
        assert old_pingap_version == f"pingap {OLD_VERSION} ({OLD_COMMIT}, tls=openssl)", old_pingap_version
        old_output = run([str(root / "old-app-cli"), "gen-lock", "--workspace", str(workspace)])
        (root / "old-gen-lock.log").write_text(old_output)
        actual = tomllib.loads(path.read_text())
        assert (actual["pingap"]["version"], actual["pingap"]["commit"]) == (OLD_VERSION, OLD_COMMIT), actual
    else:
        lock = path.read_text()
        lock = lock.replace(f'version = "{VERSION}"', f'version = "{OLD_VERSION}"')
        lock = lock.replace(f'commit = "{COMMIT}"', f'commit = "{OLD_COMMIT}"')
        path.write_text(lock)
    before = path.read_bytes()
    old_lock = tomllib.loads(before.decode())
    service_ports = [service["port"] for service in old_lock["services"]]
    sentinel = workspace / "business-data-retained.txt"
    sentinel.write_text("persisted-business-data")
    token = secrets.token_hex(24)
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("APP_CLI_", "APP_DEPLOY_"))}
    env.update({"APP_CLI_DEPLOY_TOKEN": token, "APP_CLI_RUN_PROFILE": "prod",
                "PROJECT_ID": "pingap-upgrade-validation",
                "APP_CLI_STATE_ROOT": str(root / "lifecycle-state"),
                "APP_CLI_PINGAP_RUNTIME_DIR": str(root / "lifecycle-runtime")})
    observations = []
    headers = {"X-Deploy-Token": token}
    def api(endpoint):
        response = request(3010, endpoint, headers)
        if response["status"] == 503:
            raise OSError(f"owner initializing: {endpoint}")
        assert response["status"] == 200, response
        body = json.loads(response["body"])
        assert body["success"], body
        return body["data"]
    def readiness():
        body = api("/v1/app/readiness")
        return body if body.get("ready") else None
    def stopped_ports():
        for port in [9080] + service_ports:
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                    return None
            except OSError:
                continue
        return True
    def submit(kind):
        identity = api("/v1/runtime/identity")
        status = api("/v1/runtime/status")
        op = {"operation_id": f"upgrade-{kind}-{secrets.token_hex(6)}",
              "expected_runtime_instance_id": identity["runtime_instance_id"],
              "expected_revision": status["revision"], "workspace_id": identity["workspace_id"],
              "kind": kind, "profile": {"profile": "source",
                                          "input": {"workspace_id": identity["workspace_id"]}}}
        response = request(3010, "/v1/runtime/operations",
                           dict(headers, **{"Content-Type": "application/json"}),
                           "POST", json.dumps(op))
        assert response["status"] == 202, response
        def terminal():
            actual = api(f"/v1/runtime/operations/{op['operation_id']}")
            state = actual.get("state")
            if state in ("failed", "cancelled", "recovery_required"):
                raise AssertionError(actual)
            return actual if state == "succeeded" else None
        result = wait_for(terminal, seconds=100)
        assert result["operation_id"] == op["operation_id"], result
        return result
    command = [str(root / "app-cli"), "serve", "--workspace", str(workspace),
               "--log-dir", str(root / "lifecycle-logs"), "--admin-addr", "127.0.0.1:3010",
               "--pingap-bin", str(root / "pingap")]
    with (root / "lifecycle.log").open("w") as log:
        first_command = command.copy()
        if has_old_pair:
            first_command[0], first_command[-1] = str(root / "old-app-cli"), str(root / "old-pingap")
        process = subprocess.Popen(first_command, env=env, stdout=log, stderr=subprocess.STDOUT)
        try:
            initial = wait_for(readiness, seconds=100)
            assert path.read_bytes() == before, "automatic startup rewrote old release.lock metadata"
            response = request(9080, "/api/echo")
            assert response["status"] == 200 and json.loads(response["body"])["service"] == "api", response
            observations.append({"step": "old-lock-startup", "readiness": initial,
                                 "lock_sha256": hashlib.sha256(before).hexdigest(), "http": response})
            result = submit("stop")
            stop_operation_id = result["operation_id"]
            stopped = api("/v1/runtime/status")
            assert stopped["desired"] == "stopped", stopped
            stopped_readiness = api("/v1/app/readiness")
            assert not stopped_readiness["ready"], stopped_readiness
            assert path.read_bytes() == before, "Stop rewrote old release.lock metadata"
            wait_for(stopped_ports, seconds=20)
            observations.append({"step": "stop", "operation": result, "status": stopped,
                                 "readiness": stopped_readiness, "business_ports_closed": [9080] + service_ports})
            stop_process(process)
            assert process.returncode == 0, process.returncode
            process = subprocess.Popen(command, env=env, stdout=log, stderr=subprocess.STDOUT)
            def restored_stopped():
                value = api("/v1/runtime/status")
                return value if value.get("desired") == "stopped" else None
            restored = wait_for(restored_stopped)
            assert path.read_bytes() == before
            assert not api("/v1/app/readiness")["ready"]
            wait_for(stopped_ports, seconds=20)
            old_operation = api(f"/v1/runtime/operations/{stop_operation_id}")
            assert old_operation["operation_id"] == stop_operation_id
            assert old_operation["state"] == "succeeded", old_operation
            observations.append({"step": "owner-restart-remains-stopped", "status": restored,
                                 "preserved_operation": old_operation,
                                 "business_ports_closed": [9080] + service_ports})
            result = submit("start")
            ready = wait_for(readiness, seconds=100)
            response = request(9080, "/api/echo")
            assert response["status"] == 200 and json.loads(response["body"])["service"] == "api", response
            assert sentinel.read_text() == "persisted-business-data"
            observations.append({"step": "explicit-start", "operation": result,
                                 "readiness": ready, "http": response, "business_data_retained": True})
            result = submit("restart")
            ready = wait_for(readiness, seconds=100)
            response = request(9080, "/api/echo")
            assert response["status"] == 200 and json.loads(response["body"])["service"] == "api", response
            assert sentinel.read_text() == "persisted-business-data"
            observations.append({"step": "restart", "operation": result, "readiness": ready,
                                 "http": response, "business_data_retained": True})
        finally:
            stop_process(process)
    result = {"app_cli_version": binary_version, "observations": observations,
              "old_state_producer": "actual-old-binary" if has_old_pair else "new-binary-with-old-metadata-fixture"}
    if has_old_pair:
        result.update({"old_app_cli_version": old_binary_version, "old_pingap_version": old_pingap_version})
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inside", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--app-cli", type=Path)
    parser.add_argument("--linux-app-cli", type=Path)
    parser.add_argument("--linux-build-inputs", type=Path,
                        help="冻结Linux编译输入的receipt；记录其SHA256关联编译产物")
    parser.add_argument("--old-linux-app-cli", type=Path,
                        help="从旧镜像抽取的真实app-cli，用于产生旧状态后升级同一目录")
    parser.add_argument("--old-pingap-bin", type=Path)
    parser.add_argument("--old-image-id", help="旧pair来源的实际不可变Docker镜像ID")
    parser.add_argument("--asset", type=Path)
    parser.add_argument("--asset-sha256")
    parser.add_argument("--arch", choices=("amd64", "arm64"))
    parser.add_argument("--image")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.inside:
        return inside()
    for name in ("app_cli", "asset", "asset_sha256", "arch", "image", "output"):
        if getattr(args, name) is None:
            parser.error(f"--{name.replace('_', '-')} is required")
    if args.old_linux_app_cli or args.old_pingap_bin or args.old_image_id:
        if not all((args.linux_app_cli, args.old_linux_app_cli, args.old_pingap_bin, args.old_image_id)):
            parser.error("old-pair validation requires --linux-app-cli, both old binaries and --old-image-id")
    return prepare(args)


if __name__ == "service":
    server = backend(int(os.environ["PORT"]), Path.cwd().name)
    while True:
        time.sleep(1)
elif __name__ == "__main__":
    sys.exit(main())
