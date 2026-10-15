#!/usr/bin/env python3
"""Real builtin owner + official Pingap: a TCP app intentionally has no /health.

Run the exact same script against the baseline and fixed app-cli binaries. The
baseline must fail a TCP Start on its HTTP publication contract, not compilation.
Every invocation owns a fresh directory and uses authenticated owner Shutdown.
This fixture does not claim supervisord or Kubernetes acceptance.
"""
import argparse
import importlib.util
import json
from pathlib import Path
import subprocess
import socket
import sys
import tomllib
import uuid


def load_pair(repository):
    path = repository / "tools/build/verify_official_pair.py"
    spec = importlib.util.spec_from_file_location("official_pair_fixture", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.NativePair


def run_case(args, name):
    native = load_pair(args.repository)

    class TcpPair(native):
        def native_management_ready(self):
            # HTTP identity is intentionally published before business recovery
            # finishes. Read the live authenticated owner snapshot instead of
            # treating identity/Idle (or the existence of discovery) as readiness.
            discovery = json.loads((self.state / "supervisor.json").read_text())
            if discovery["version"] != 2:
                raise AssertionError("native control protocol is not the expected v2")
            host, port = discovery["address"].rsplit(":", 1)
            frame = {"version": 2, "instance": discovery["instance"], "token": discovery["token"],
                     "request": {"request_id": str(uuid.uuid4()), "action": "status",
                                 "expected_generation": discovery["snapshot"]["generation"]}}
            with socket.create_connection((host, int(port)), timeout=3) as connection:
                connection.sendall(json.dumps(frame).encode() + b"\n")
                with connection.makefile("rb") as reader:
                    raw = reader.readline(32769)
            if len(raw) > 32768 or not raw.endswith(b"\n"):
                raise AssertionError("native status returned an invalid bounded frame")
            reply = json.loads(raw)
            snapshot = reply["snapshot"]
            if (reply["instance"] != discovery["instance"] or reply.get("error") is not None
                    or snapshot["supervisor_id"] != discovery["instance"]):
                raise AssertionError("native owner identity changed during initialization")
            return snapshot["phase"] == "ready" and snapshot.get("error") is None

        def admissible_status(self):
            if not self.native_management_ready():
                return None
            # This route explicitly reports ERR_INITIALIZING while business
            # recovery is pending, even though identity/status are already 200.
            recovery = self.data("/v1/runtime/recovery")
            if recovery["kernel_protected"] or recovery["owner_protected"]:
                raise AssertionError("fresh isolated owner requires recovery")
            current = self.data("/v1/runtime/status")
            if current.get("active_operation_id") or current["recovery_protection"]:
                return None
            if recovery["runtime_instance_id"] != current["runtime_instance_id"]:
                raise AssertionError("recovery and status belong to different owner instances")
            return current

        def operation_result(self, kind, expected="succeeded"):
            current = self.wait(self.admissible_status)
            identity = self.data("/v1/runtime/identity")
            self.check("same management owner before " + kind,
                identity["runtime_instance_id"] == current["runtime_instance_id"])
            operation_id = str(uuid.uuid4())
            request = dict(operation_id=operation_id, kind=kind,
                expected_runtime_instance_id=identity["runtime_instance_id"],
                expected_revision=current["revision"], workspace_id=identity["workspace_id"],
                profile={"profile": "source", "input": {"workspace_id": identity["workspace_id"]}})
            self.data("/v1/runtime/operations", request)

            def terminal():
                result = self.data("/v1/runtime/operations/" + operation_id)
                if result["operation_id"] != operation_id:
                    raise AssertionError("terminal belongs to another operation")
                return result if result["state"] in ("succeeded", "failed", "cancelled", "recovery_required") else None

            result = self.wait(terminal, 100)
            self.result.setdefault("operations", []).append(result)
            self.check(kind + " truthful terminal " + expected, result["state"] == expected)
            return result

        def manifest(self, probe):
            command = json.dumps([sys.executable, "server.py"])
            build = json.dumps([sys.executable, "-c", "pass"])
            strategy = "" if probe is None else f'startup_probe="{probe}"\n'
            development = "[devrun]\ncommand=" + command + "\n" if name == "omitted-devrun" else ""
            (self.workspace / "web/project.manifest.toml").write_text(
                'schema_version=1\n[project]\nservice_id="web"\nname="web"\ntype="python"\n'
                f'[build]\ncommand={build}\nartifact="unused.zip"\n[run]\ncommand={command}\n'
                'shutdown_timeout_seconds=3\n' + development + '[health]\n' + strategy +
                'startup_path="/health"\nreadiness_path="/health"\nstartup_timeout_seconds=3\n'
                '[proxy]\npath="/"\n', encoding="utf-8")
            self.check("real source configuration validation", self.run_cli(
                ["validate", "--workspace", str(self.workspace), "--dev", "--json"], "validate-" + str(probe)) == 0)
            self.check("real source lock generation", self.run_cli(
                ["gen-lock", "--workspace", str(self.workspace), "--dev"], "gen-lock-" + str(probe)) == 0)

        def capture_http(self, port, path):
            observed = self.http(port, path)
            evidence = self.result.setdefault("http_observations", [])
            evidence.append({"port": port, "path": path, "status": observed[0],
                "body": observed[1][:2048].decode("utf-8", errors="replace")})
            # Keep enough bounded diagnostic context even if the last poll fails.
            del evidence[:-256]
            return observed

        def verify_serving(self):
            proxy = self.wait(self.confirmed_proxy)
            confirmation = proxy["confirmation"]
            self.check("same real official instance is confirmed", confirmation["confirmed"]
                and confirmation["contract"] == "official_restart_http_v1"
                and confirmation["proxy_instance_id"] and confirmation["config_digest"]
                and proxy["applied"] is False)
            release = tomllib.loads((self.workspace / "release.lock.toml").read_text())
            port = next(service["port"] for service in release["services"] if service["service_id"] == "web")
            self.check("actual direct business response", self.http(port, "/actual-business") == (200, b"tcp-business"))
            self.check("direct service has no HTTP health endpoint", self.http(port, "/health") == (404, b"absent-health"))
            active = tomllib.loads((self.root / "pingap/active/pingap.toml").read_text())
            if name == "managed-http-health":
                self.check("Managed retains declared HTTP health routing contract",
                    active["upstreams"]["web"]["health_check"] == "http://web/health")

                def unavailable_upstream():
                    observed = self.capture_http(9080, "/actual-business")
                    # This exact failure comes from the pinned official server
                    # when no backend is available, not from our Python app.
                    return observed if (observed[0] == 503 and b"No available upstream for" in observed[1]) else None

                actual = self.wait(unavailable_upstream, 45)
            else:
                self.check("Custom explicitly selects official TCP upstream health",
                    active["upstreams"]["web"]["health_check"].startswith("tcp://web?"))
                def routed_business():
                    observed = self.capture_http(9080, "/actual-business")
                    return observed if observed == (200, b"tcp-business") else None

                actual = self.wait(routed_business)
            if name == "managed-http-health":
                self.check("Managed HTTP health failure truthfully removes business routing",
                    actual[0] == 503 and b"No available upstream for" in actual[1])
            else:
                self.check("actual proxied business response", actual == (200, b"tcp-business"))
                health = self.http(9080, "/health")
                self.result["http_observations"].append({
                    "path": "/health", "status": health[0],
                    "body": health[1][:2048].decode("utf-8", errors="replace")})
                self.check("service deliberately has no HTTP health endpoint", health == (404, b"absent-health"))
            publication = proxy["publication_id"]
            status, body = self.http(9080, "/_pub/" + publication)
            self.check("proxy listener executes exact publication UUID", status == 200 and body.decode() == publication)
            readiness = self.data("/v1/app/readiness")
            if name == "managed-http-health":
                self.check("Managed HTTP readiness remains failed after successful TCP startup",
                    not readiness["ready"] and readiness["status"] == "degraded"
                    and any(not item["ready"] and item["service_id"] == "web"
                            and item["reason_code"] == "HEALTH_HTTP_FAILURE" for item in readiness["services"]))
            else:
                self.check("Custom preserves conservative route readiness contract",
                    not readiness["ready"] and readiness["status"] == "unsupported"
                    and readiness["reason_code"] == "CUSTOM_ROUTE_UNVERIFIED")
            self.result.setdefault("readiness", []).append(readiness)

        def execute(self):
            self.result["case"] = name
            self.result["proxy_mode"] = "managed" if name == "managed-http-health" else "custom"
            workspace_manifest = 'schema_version=1\n[workspace]\nname="tcp-publication"\n'
            if name != "managed-http-health":
                workspace_manifest += '[pingap]\nmode="custom"\nconfig="proxy.toml"\n'
                (self.workspace / "proxy.toml").write_text(
                    '[upstreams.web]\naddrs=["rcoder://web"]\n'
                    'health_check="tcp://web?success=1&failure=1&check_frequency=1s&connection_timeout=1s"\n'
                    '[locations.web]\npath="/"\nupstream="web"\n'
                    '[servers.app]\naddr="0.0.0.0:9080"\nlocations=["web"]\n', encoding="utf-8")
            (self.workspace / "workspace.manifest.toml").write_text(workspace_manifest, encoding="utf-8")
            (self.workspace / "sentinel").write_text("keep-business-data", encoding="utf-8")
            (self.workspace / "web/server.py").write_text('''import http.server, os
class Handler(http.server.BaseHTTPRequestHandler):
 def do_GET(self):
  absent=self.path=="/health";body=b"absent-health" if absent else b"tcp-business"
  self.send_response(404 if absent else 200);self.send_header("Content-Length",str(len(body)));self.end_headers();self.wfile.write(body)
http.server.HTTPServer(("127.0.0.1",int(os.environ["PORT"])),Handler).serve_forever()
''', encoding="utf-8")
            probe = {"explicit-tcp": "tcp", "omitted-devrun": None, "explicit-http": "http", "managed-http-health": "tcp"}[name]
            self.manifest(probe)
            self.output = (self.root / "owner.log").open("wb")
            self.owner = subprocess.Popen([str(self.app), "serve", "--control-only", "--workspace", str(self.workspace),
                "--log-dir", str(self.logs), "--admin-addr", f"127.0.0.1:{self.admin}",
                "--pingap-bin", str(self.proxy)], env=self.env, stdout=self.output, stderr=subprocess.STDOUT)
            initial = self.wait(self.admissible_status)
            self.check("control-only owner finished recovery with no business execution",
                initial["observed"] == "stopped" and not initial.get("active_operation_id")
                and initial["recovery_protection"] is False)
            status, ready = self.http(self.admin, "/ready")
            self.check("fresh control-only management is ready", status == 200 and json.loads(ready)["status"] == "ready")
            owner_identity = self.data("/v1/runtime/identity")["runtime_instance_id"]
            if name == "explicit-http":
                self.operation_result("start", "failed")
                self.check("HTTP failure keeps management available", self.http(self.admin, "/health")[0] == 200)
                self.manifest("tcp")
            self.operation_result("start")
            self.verify_serving()
            self.operation_result("restart")
            self.verify_serving()
            if name != "managed-http-health":
                self.reload_proxy()
            self.verify_serving()
            self.operation_result("stop")
            self.operation_result("start")
            self.verify_serving()
            self.check("original owner retained through repeated source operations",
                self.data("/v1/runtime/identity")["runtime_instance_id"] == owner_identity)
            self.check("business sentinel retained", (self.workspace / "sentinel").read_text() == "keep-business-data")
            self.result["passed"] = True

    pair = TcpPair(args.app_cli, args.pingap, args.root / name)
    try:
        pair.execute()
    except Exception as error:
        pair.result["error"] = str(error)
    finally:
        pair.close()
    return pair.result["passed"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", type=Path, required=True)
    parser.add_argument("--app-cli", type=Path, required=True)
    parser.add_argument("--pingap", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--case", choices=["explicit-tcp", "omitted-devrun", "explicit-http", "managed-http-health"])
    args = parser.parse_args()
    cases = [args.case] if args.case else ["explicit-tcp", "omitted-devrun", "explicit-http", "managed-http-health"]
    passed = [run_case(args, name) for name in cases]
    return 0 if all(passed) else 1


if __name__ == "__main__":
    raise SystemExit(main())
