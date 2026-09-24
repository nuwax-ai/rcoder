#!/usr/bin/env python3
"""Real app-cli + Pingap + React/Vue routing regression, without LLM or containers.

Requires built template-cli and installed template node_modules. All generated
projects, builds and runtime state stay in a fresh temporary directory.
"""
import argparse
import base64
import contextlib
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import socket
import subprocess
import tempfile
import time
import tomllib


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def request(port, path, payload=None, token=None):
    headers = {"X-Deploy-Token": token} if token else {}
    body = None if payload is None else json.dumps(payload)
    if payload is not None:
        headers["Content-Type"] = "application/json"
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=35)
    try:
        connection.request("GET" if payload is None else "POST", path, body, headers)
        response = connection.getresponse()
        return response.status, response.getheader("Content-Type", ""), response.read()
    finally:
        connection.close()


def wait_for(check, process, timeout=60):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        require(process.poll() is None, "app-cli exited before readiness")
        try:
            result = check()
            if result:
                return result
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(0.2)
    raise TimeoutError("app-cli did not reach the expected state")


def check_module(path):
    status, content_type, body = request(9080, path)
    require(status == 200 and "javascript" in content_type,
            f"{path}: expected JavaScript, got {status} {content_type}")
    require(b"<!doctype" not in body.lower(), f"{path}: SPA fallback returned HTML")
    return body


def check_prefix_redirect(prefix):
    connection = http.client.HTTPConnection("127.0.0.1", 9080, timeout=10)
    try:
        for method in ("GET", "HEAD"):
            connection.request(method, prefix + "?probe=bare-prefix", headers={"Accept": "text/html"})
            response = connection.getresponse()
            response.read()
            require(response.status == 302 and response.getheader("Location") == prefix + "/?probe=bare-prefix",
                    f"{prefix}: {method} must redirect to the trailing slash and preserve the query")
    finally:
        connection.close()


def check_hmr(prefix, client_js):
    # Vite versions with websocket-token protection embed the token in the client.
    match = re.search(rb'const wsToken\s*=\s*["\']([^"\']+)', client_js)
    path = prefix + "/" + ("?token=" + match[1].decode() if match else "")
    key = base64.b64encode(secrets.token_bytes(16)).decode()
    with socket.create_connection(("127.0.0.1", 9080), timeout=10) as sock:
        sock.sendall((f"GET {path} HTTP/1.1\r\nHost: localhost:9080\r\n"
                      "Origin: http://localhost:9080\r\nConnection: Upgrade\r\n"
                      "Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n"
                      f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Protocol: vite-hmr\r\n\r\n").encode())
        response = b""
        while b"\r\n\r\n" not in response:
            chunk = sock.recv(4096)
            require(chunk, f"{prefix}: websocket closed before handshake")
            response += chunk
        require(response.startswith(b"HTTP/1.1 101"),
                f"{prefix}: HMR websocket upgrade failed: {response.splitlines()[0]!r}")
        expected = base64.b64encode(hashlib.sha1(
            (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest())
        require(expected.lower() in response.split(b"\r\n\r\n", 1)[0].lower(),
                f"{prefix}: invalid websocket handshake")


@contextlib.contextmanager
def owner(binary, pingap, workspace, root, phase):
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("APP_CLI_", "APP_DEPLOY_")) and key != "APP_RELEASE_ID"}
    state = root / (phase + "-state")
    env.update(APP_CLI_STATE_ROOT=str(state), APP_CLI_REQUIRE_PG="0",
               APP_CLI_DEPLOY_TOKEN=secrets.token_hex(24))
    with (root / (phase + ".log")).open("wb") as log:
        process = subprocess.Popen([
            str(binary), "serve", "--workspace", str(workspace),
            "--log-dir", str(root / (phase + "-logs")),
            "--pingap-bin", str(pingap), "--admin-addr", "127.0.0.1:0",
        ], env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
        try:
            endpoint = state / "endpoint.json"
            wait_for(endpoint.is_file, process)
            address = json.loads(endpoint.read_text())["address"]
            port = int(address.rsplit(":", 1)[1])
            wait_for(lambda: request(port, "/ready")[0] == 200, process)
            yield process, port, env["APP_CLI_DEPLOY_TOKEN"]
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=40)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                    raise RuntimeError(f"app-cli shutdown unconfirmed; inspect {root}")
            require(process.returncode == 0, f"app-cli exited {process.returncode}; inspect {root}")


def verify(args, root):
    workspace = root / "workspace"
    cli = args.templates / "cli/dist/index.js"
    subprocess.run(["node", str(cli), "init", str(workspace), "--frontend", "react"], check=True)
    # Two instances of the same template must keep independent routes/contents.
    for name, prefix in [("admin-web", "/admin/"), ("portal-web", "/portal/")]:
        subprocess.run(["node", str(cli), "add", "frontend-vue3-vite",
                        "--service-id", name, "--path", prefix], cwd=workspace, check=True)
    # Change only TOML after generation: dev AND build must read the new path.
    manifest = workspace / "admin-web/project.manifest.toml"
    config = workspace / "admin-web/vite.config.ts"
    original_config = config.read_bytes()
    manifest.write_text(manifest.read_text().replace('path = "/admin/"', "  path = '/console/'"))
    services = [("frontend-react-vite", "frontend-react-vite", "/react", "main.tsx"),
                ("admin-web", "frontend-vue3-vite", "/console", "main.ts"),
                ("portal-web", "frontend-vue3-vite", "/portal", "main.ts")]
    for name, template, _, entry in services:
        dependencies = args.templates / template / "node_modules"
        require(dependencies.is_dir(), f"install dependencies in {dependencies.parent}")
        # pnpm may repair/install on `run`. Keep each generated project's tree
        # separate: two Vue instances must not mutate one shared node_modules.
        shutil.copytree(dependencies, workspace / name / "node_modules", symlinks=True)
        page = workspace / name / "index.html"
        page.write_text(page.read_text().replace("</head>", f'<meta name="route-probe" content="{name}"></head>'))
        source = workspace / name / "src" / entry
        source.write_text(source.read_text() + f'\nconsole.info("route-probe:{name}");\n')

    # Start an idle owner without a dev environment flag. The operation must
    # select Source, and later reload must preserve that explicit profile.
    with owner(args.app_cli, args.pingap, workspace, root, "dev") as (process, port, token):
        subprocess.run([str(args.app_cli), "gen-lock", "--workspace", str(workspace), "--dev"],
                       check=True, stdout=subprocess.DEVNULL)
        identity = json.loads(request(port, "/v1/runtime/identity")[2])["data"]
        revision = json.loads(request(port, "/v1/runtime/status")[2])["data"]["revision"]
        operation_id = "subpath" + secrets.token_hex(6)
        payload = {"operation_id": operation_id, "kind": "start",
                   "expected_runtime_instance_id": identity["runtime_instance_id"],
                   "expected_revision": revision, "workspace_id": identity["workspace_id"],
                   "profile": {"profile": "source", "input": {"workspace_id": identity["workspace_id"]}}}
        status, _, body = request(port, "/v1/runtime/operations", payload, token)
        require(status == 202, f"source start rejected: {status} {body!r}")

        def finished():
            view = json.loads(request(port, f"/v1/runtime/operations/{operation_id}")[2])["data"]
            require(view["state"] not in ("failed", "cancelled", "recovery_required"),
                    f"source operation failed: {view}")
            return view["state"] == "succeeded"

        wait_for(finished, process, 90)
        check_prefix_redirect("/react")
        for name, _, prefix, entry in services:
            status, _, html = request(9080, prefix + "/")
            require(status == 200, f"{prefix}: missing dev document")
            require(f'content="{name}"'.encode() in html, f"{prefix}: document routed to another service")
            require((prefix + "/@vite/client").encode() in html, f"{prefix}: client escaped base")
            client_js = check_module(prefix + "/@vite/client")
            module = check_module(prefix + "/src/" + entry)
            require(f"route-probe:{name}".encode() in module, f"{prefix}: module routed to another service")
            status, content_type, body = request(9080, prefix + "/health")
            require(status == 200 and "application/json" in content_type and json.loads(body)["status"] == "ok",
                    f"{prefix}: health returned a fallback page")
            check_hmr(prefix, client_js)
            print(f"PASS dev {name}: document, modules, health, HMR upgrade", flush=True)
        status, _, body = request(port, "/v1/proxy/reload", {}, token)
        require(status == 200 and json.loads(body)["data"]["verified"], f"reload failed: {body!r}")
        for _, _, prefix, entry in services:
            check_module(prefix + "/src/" + entry)
        print("PASS reload retains the Source operation's routing profile", flush=True)

    for name, _, _, _ in services:
        subprocess.run(["pnpm", "exec", "vite", "build"], cwd=workspace / name, check=True)
    # A fresh owner uses the artifact profile and the same release lock.
    with owner(args.app_cli, args.pingap, workspace, root, "prod") as (process, port, _):
        wait_for(lambda: json.loads(request(port, "/ready")[2]).get("phase") == "running", process)
        for name, _, prefix, _ in services:
            status, _, html = request(9080, prefix + "/")
            require(status == 200 and b"@vite/client" not in html, f"{prefix}: expected built HTML")
            require(f'content="{name}"'.encode() in html, f"{prefix}: built document routed to another service")
            scripts = re.findall(rb'<script[^>]*src="([^"]+)"', html)
            require(scripts, f"{prefix}: built HTML has no scripts")
            modules = []
            for script in scripts:
                path = script.decode()
                require(path.startswith(prefix + "/assets/"), f"asset escaped public prefix: {path}")
                modules.append(check_module(path))
            require(any(f"route-probe:{name}".encode() in module for module in modules),
                    f"{prefix}: built JavaScript routed to another service")
            print(f"PASS prod {name}: built document and JavaScript assets", flush=True)
    require(config.read_bytes() == original_config, "verification must not rewrite Vite config")
    print("PASS two Vue instances and TOML-only route change in dev/build", flush=True)
    lock = tomllib.loads((workspace / "release.lock.toml").read_text())
    for service in lock["services"]:
        for port in [9080, 9081, 3018, service["port"]]:
            with socket.socket() as probe:
                probe.settimeout(1)
                require(probe.connect_ex(("127.0.0.1", port)) != 0, f"listener {port} leaked after shutdown")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app-cli", required=True, type=Path)
    parser.add_argument("--pingap", required=True, type=Path)
    parser.add_argument("--templates", required=True, type=Path)
    args = parser.parse_args()
    for field in ("app_cli", "pingap", "templates"):
        setattr(args, field, getattr(args, field).resolve(strict=True))
    # Never reuse another runtime's fixed listener.
    for port in (9080, 9081, 3018):
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", port))
    root = Path(tempfile.mkdtemp(prefix="rcoder-subpath-"))
    print(f"Evidence directory: {root}", flush=True)
    verify(args, root)
    print("PASS UserApp dev/prod subpath routing (HMR handshake; no browser rendering claim)")


if __name__ == "__main__":
    main()
