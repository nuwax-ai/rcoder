#!/usr/bin/env python3
"""Real template pip cache through RCoder build, retained-volume recreation and dev HTTP.

Uses a frozen copy of the actual Python template build scripts. The tiny local
wheel is a real dependency, not a pip replacement. Missing Docker/template/API
prerequisites fail this contract. Cleanup removes only captured compute; source,
cache, report and data volumes remain. Python >= 3.11.
"""
import argparse
import base64
import hashlib
import io
import json
import os
from pathlib import Path
import re
import struct
import subprocess
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request
import uuid
import zipfile

REPO = Path(__file__).resolve().parents[2]
SERVICE = "backend-python"
REQUIRED_STEPS = frozenset({
    "Python cache frozen template inputs",
    "Python cache public workspace created",
    "Python cache local index ready",
    "Python cache public template upload accepted",
    "Python cache first build installs real dependency",
    "Python cache artifacts preserve deps and exclude caches",
    "Python cache second build performs no index requests",
    "Python cache platform stop completes original operation",
    "Python cache recreated builder retains original volume and data",
    "Python cache third build performs no index requests",
    "Python cache dev start serves installed dependency over HTTP",
    "Python cache final source inputs unchanged",
    "Python cache owned compute cleanup preserves data",
})


def digest(data):
    return hashlib.sha256(data).hexdigest()


def create_wheel(directory, version="1.0"):
    """Create a valid pure-Python wheel, including hashed RECORD entries."""
    name = f"cache_probe-{version}-py3-none-any.whl"
    info = f"cache_probe-{version}.dist-info"
    files = {
        "cache_probe.py": f"VERSION = {version!r}\n".encode(),
        f"{info}/METADATA": f"Metadata-Version: 2.1\nName: cache-probe\nVersion: {version}\n".encode(),
        f"{info}/WHEEL": b"Wheel-Version: 1.0\nGenerator: rcoder-e2e\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
    }
    record = "".join(f"{path},sha256={base64.urlsafe_b64encode(hashlib.sha256(data).digest()).decode().rstrip('=')},{len(data)}\n"
                     for path, data in files.items())
    files[f"{info}/RECORD"] = (record + f"{info}/RECORD,,\n").encode()
    with zipfile.ZipFile(Path(directory) / name, "w", zipfile.ZIP_DEFLATED) as archive:
        for path, data in files.items():
            archive.writestr(path, data)
    return name


INDEX_SERVER = r'''
import http.server,json,pathlib,threading
root=pathlib.Path('/index');requests=[];lock=threading.Lock()
class H(http.server.SimpleHTTPRequestHandler):
 def __init__(self,*args,**kwargs):super().__init__(*args,directory=str(root),**kwargs)
 def do_GET(self):
  if self.path=='/__stats':
   with lock:data=json.dumps(requests).encode()
   self.send_response(200);self.end_headers();self.wfile.write(data);return
  with lock:
   requests.append(self.path)
   (root/'requests.json').write_text(json.dumps(requests))
  super().do_GET()
 def end_headers(self):
  self.send_header('Cache-Control','public, max-age=3600');super().end_headers()
 def log_message(self,*args):pass
http.server.ThreadingHTTPServer(('0.0.0.0',8080),H).serve_forever()
'''


def dependency_requests(paths):
    return [path for path in paths if path.startswith("/simple/") or path.split("?", 1)[0].endswith(".whl")]


def frozen_template(root):
    """Fail if the actual template contract moved; never reconstruct its cache implementation."""
    root = Path(root).resolve()
    manifest_path = root / SERVICE / "project.manifest.toml"
    manifest = tomllib.loads(manifest_path.read_text())
    if manifest["project"]["service_id"] != SERVICE or manifest["build"] != {
            "command": ["sh", "scripts/build-standalone.sh"], "artifact": "artifact.zip"}:
        raise ValueError("Python template build contract changed; adapt fixture explicitly")
    paths = ("workspace.manifest.toml", f"{SERVICE}/project.manifest.toml",
             f"{SERVICE}/scripts/build-standalone.py", f"{SERVICE}/scripts/build-standalone.sh",
             f"{SERVICE}/scripts/artifact_pack.py", "cli/package.json")
    data = {name: (root / name).read_bytes() for name in paths}
    version = json.loads(data["cli/package.json"])["version"]
    git = subprocess.run(["git", "-C", str(root), "rev-parse", "HEAD"], capture_output=True, text=True, check=True)
    return data, {"root": str(root), "commit": git.stdout.strip(), "template_cli_version": version,
                  "files_sha256": {name: digest(raw) for name, raw in data.items()}}


def fixture_zip(data, index_url, marker):
    """Use the real scripts verbatim; only dependency and business inputs are fixtures."""
    if not index_url.startswith("http://") or '"' in index_url or "\n" in index_url:
        raise ValueError("invalid private index URL")
    manifest = data[f"{SERVICE}/project.manifest.toml"].decode()
    old = 'command = ["sh", "scripts/build-standalone.sh"]'
    command = ["env", "-u", "PIP_CONSTRAINT", "-u", "PIP_REQUIREMENT", "-u", "PIP_EXTRA_INDEX_URL",
               "PIP_CONFIG_FILE=/dev/null", f"PIP_INDEX_URL={index_url}",
               "PIP_TRUSTED_HOST=" + urllib.parse.urlparse(index_url).hostname,
               "PIP_DISABLE_PIP_VERSION_CHECK=1", "PIP_RETRIES=0", "PIP_DEFAULT_TIMEOUT=10",
               "sh", "scripts/build-standalone.sh"]
    if manifest.count(old) != 1:
        raise ValueError("template build command is not uniquely replaceable")
    manifest = manifest.replace(old, "command = " + json.dumps(command))
    main = '''import os,sys,json
from http.server import BaseHTTPRequestHandler,HTTPServer
sys.path.insert(0,os.path.join(os.path.dirname(__file__),'deps'))
import cache_probe
class H(BaseHTTPRequestHandler):
 def do_GET(self):
  self.send_response(200);self.end_headers()
  self.wfile.write(json.dumps({'marker':MARKER,'dependency_version':cache_probe.VERSION}).encode())
HTTPServer(('0.0.0.0',int(os.environ['PORT'])),H).serve_forever()
'''.replace("MARKER", repr(marker))
    files = {"workspace.manifest.toml": data["workspace.manifest.toml"],
             f"{SERVICE}/project.manifest.toml": manifest.encode(),
             f"{SERVICE}/scripts/build-standalone.py": data[f"{SERVICE}/scripts/build-standalone.py"],
             f"{SERVICE}/scripts/build-standalone.sh": data[f"{SERVICE}/scripts/build-standalone.sh"],
             f"{SERVICE}/scripts/artifact_pack.py": data[f"{SERVICE}/scripts/artifact_pack.py"],
             f"{SERVICE}/main.py": main.encode(), f"{SERVICE}/app/__init__.py": b"",
             f"{SERVICE}/requirements.lock": b"cache-probe==1.0\n", "cache-sentinel": marker.encode()}
    target = io.BytesIO()
    with zipfile.ZipFile(target, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, raw in files.items():
            archive.writestr(name, raw)
    return target.getvalue(), {name: digest(raw) for name, raw in files.items()}


def compressed_entry(archive, entry):
    archive.fp.seek(entry.header_offset)
    header = archive.fp.read(30)
    if len(header) != 30 or header[:4] != b"PK\x03\x04":
        raise ValueError("invalid ZIP local header")
    name_length, extra_length = struct.unpack_from("<HH", header, 26)
    archive.fp.seek(name_length + extra_length, 1)
    payload = archive.fp.read(entry.compress_size)
    if len(payload) != entry.compress_size:
        raise ValueError("truncated ZIP compressed data")
    return payload


def inspect_artifacts(child_path, package_path):
    with zipfile.ZipFile(child_path) as child, zipfile.ZipFile(package_path) as package:
        names = child.namelist()
        if "deps/cache_probe.py" not in names or not any(name.endswith(".dist-info/METADATA") for name in names):
            raise ValueError("artifact does not include real installed dependency")
        for archive in (child, package):
            if any(any(part in {".pip-cache", ".deps-stamp", ".deps-build.lock"} for part in Path(name).parts)
                   for name in archive.namelist()):
                raise ValueError("cache or lock leaked into artifact")
        for entry in child.infolist():
            if entry.is_dir():
                continue
            copied = package.getinfo(SERVICE + "/" + entry.filename)
            if (entry.CRC, entry.compress_type, entry.file_size, entry.compress_size) != (
                    copied.CRC, copied.compress_type, copied.file_size, copied.compress_size):
                raise ValueError("workspace package changed child ZIP metadata")
            if compressed_entry(child, entry) != compressed_entry(package, copied):
                raise ValueError("workspace package did not raw copy child compressed entry")
        return {"child_entries": len(names), "package_entries": len(package.namelist()), "raw_copy_verified": True}


def compact_builder(row, app):
    labels = row["Config"].get("Labels") or {}
    if row["Name"] != "/rcoder-app-builder-" + app or labels.get("service-type") != "user-app-builder" or (
            labels.get("rcoder.io/application-id") != app or not labels.get("rcoder.io/lifecycle-id")):
        raise ValueError("builder physical application identity differs")
    mounts = [{key: mount.get(key) for key in ("Type", "Source", "Name", "Destination")}
              for mount in row["Mounts"] if mount["Destination"] == "/home/user/" + app]
    if len(mounts) != 1:
        raise ValueError("builder workspace mount absent or ambiguous")
    return {"id": row["Id"], "image": row["Image"], "name": row["Name"], "app_id": app,
            "lifecycle_id": labels["rcoder.io/lifecycle-id"], "mounts": mounts}


def prove_recreation(before, after):
    if before["id"] == after["id"] or any(before[field] != after[field] for field in
                                           ("image", "name", "app_id", "lifecycle_id", "mounts")):
        raise ValueError("builder recreation changed identity, image or workspace mount")


def docker(*args, check=True, timeout=180):
    result = subprocess.run(["docker", *args], check=False, capture_output=True, text=True, timeout=timeout)
    if check and result.returncode:
        # Docker exec may carry credentials; do not echo argv or arbitrary stderr.
        raise RuntimeError(f"Docker {args[0]} failed (exit={result.returncode})")
    return result


def cleanup(root, run_id, case_id, existing_ids=()):
    """Creation-aware fallback usable by the strict launcher after interruption."""
    root = Path(root).resolve()
    receipt = json.loads((root / "ownership.json").read_text())
    if receipt.get("run_id") != run_id or receipt.get("case_id") != case_id or receipt.get("root") != str(root):
        raise ValueError("Python cache ownership receipt differs")
    app = receipt["app_id"]
    if not re.fullmatch(r"pyc[0-9a-f]{19}", app) or not re.fullmatch(r"rcoder-pip-index-[0-9a-f]{16}", receipt["index_name"]):
        raise ValueError("Python cache resource namespace differs")
    removed = []
    unsettled = []
    for name in ("rcoder-app-builder-" + app, receipt["index_name"]):
        ids = docker("ps", "-aq", "--no-trunc", "--filter", "name=^/" + name + "$").stdout.split()
        if len(ids) > 1:
            raise ValueError("ambiguous owned compute name")
        if not ids and name == receipt["index_name"] and receipt.get("index_creation_state") == "creating":
            unsettled.append("index create outcome is unknown")
        if not ids and name.startswith("rcoder-app-builder-") and receipt.get("builder_creation_state") == "creating":
            unsettled.append("builder create outcome is unknown")
        for cid in ids:
            if cid in existing_ids:
                raise ValueError("refusing cleanup of preexisting compute")
            row = json.loads(docker("inspect", cid).stdout)[0]
            if name.startswith("rcoder-app-builder-"):
                current = compact_builder(row, app)
                captured = receipt.get("builder")
                if not captured or any(current[key] != captured[key] for key in ("id", "image", "lifecycle_id", "mounts")):
                    raise ValueError("builder cleanup requires captured exact physical identity")
            else:
                labels = row["Config"].get("Labels") or {}
                if labels.get("rcoder.e2e.run") != run_id or labels.get("rcoder.e2e.case") != case_id or (
                        receipt.get("index_id") and receipt["index_id"] != cid):
                    raise ValueError("private index cleanup identity differs")
                mounts = [mount for mount in row["Mounts"] if mount.get("Destination") == "/index"]
                if row["Image"] != receipt.get("index_image") or len(mounts) != 1 or (
                        mounts[0].get("Type") != "bind" or Path(mounts[0].get("Source", "/")).resolve() != root / "index"):
                    raise ValueError("private index cleanup image or data mount differs")
            docker("rm", "-f", cid)  # never -v, delete/app or volume rm
            removed.append(cid)
    return {"ok": not unsettled, "removed": removed, "data_retained": True, "unsettled": unsettled}


class Contract:
    def __init__(self, args):
        self.args = args
        self.root = args.report.resolve().parent / "python-dependency-cache"
        self.root.mkdir(parents=True, exist_ok=False)
        self.app = "pyc" + uuid.uuid4().hex[:19]
        self.workspace = "/home/user/" + self.app
        self.marker = "PYTHON_CACHE_" + self.app
        self.report = {"app_id": self.app, "checks": [], "tasks": [], "scope": "real_rcoder_api_docker_template_pip"}
        self.receipt = {"run_id": os.environ.get("E2E_RUN_ID", self.app),
                        "case_id": os.environ.get("E2E_CASE_ID", self.app), "root": str(self.root),
                        "app_id": self.app, "index_name": "rcoder-pip-index-" + uuid.uuid4().hex[:16],
                        "builder_creation_state": "not_started", "index_creation_state": "not_started"}
        self.cid = None
        self.save()

    def save(self):
        self.args.report.parent.mkdir(parents=True, exist_ok=True)
        self.args.report.write_text(json.dumps(self.report, ensure_ascii=False, indent=2) + "\n")
        (self.root / "ownership.json").write_text(json.dumps(self.receipt, indent=2) + "\n")

    def check(self, name, passed, detail=None):
        self.report["checks"].append({"name": name, "ok": bool(passed), "detail": detail})
        self.save()
        print(name, "PASS" if passed else "FAIL", flush=True)
        if not passed:
            raise RuntimeError(name)

    def api(self, path, body=None, raw=None, content_type=None, timeout=180):
        headers = {"X-App-Id": self.app, "content-type": content_type or "application/json"}
        if os.environ.get("E2E_RCODER_API_KEY"):
            headers["x-api-key"] = os.environ["E2E_RCODER_API_KEY"]
        data = raw if raw is not None else (None if body is None else json.dumps(body).encode())
        request = urllib.request.Request(self.args.rcoder.rstrip("/") + path, data=data, headers=headers)
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                status, payload = response.status, json.load(response)
        except urllib.error.HTTPError as error:
            raise RuntimeError(f"{path}: HTTP {error.code}") from error
        if payload.get("success") is not True:
            raise RuntimeError(f"{path}: {payload.get('code')}")
        return status, payload

    def execute(self, *args):
        return docker("exec", self.cid, *args).stdout

    def builder(self):
        row = json.loads(docker("inspect", "rcoder-app-builder-" + self.app).stdout)[0]
        current = compact_builder(row, self.app)
        self.cid = current["id"]
        self.receipt["builder"] = current
        self.receipt["builder_creation_state"] = "captured"
        self.save()
        return row, current

    def wait(self, label, fn, predicate, seconds=240):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            value = fn()
            if predicate(value):
                return value
            time.sleep(0.5)
        raise RuntimeError(label + " deadline exceeded")

    def task(self, action):
        _, response = self.api("/api/v1/userapp/" + action, {"app_id": self.app, "user_id": self.args.user_id})
        task_id = response["data"].get("task_id")
        if not task_id:
            raise RuntimeError(action + " did not admit a real task")
        def observe():
            _, payload = self.api(f"/api/v1/userapp/tasks/{task_id}?app_id={self.app}&user_id={self.args.user_id}")
            task = payload["data"]
            if task.get("id") != task_id or task.get("app_id") != self.app:
                raise RuntimeError("task identity changed")
            if task.get("status") in ("failed", "cancelled"):
                self.report["tasks"].append(task)
                raise RuntimeError(action + " task failed: " + str(task.get("error"))[:300])
            return task
        task = self.wait(action, observe, lambda value: value.get("status") == "completed")
        self.report["tasks"].append(task)
        self.save()
        return task

    def snapshot(self):
        code = '''import hashlib,json,pathlib,sys,sysconfig
r=pathlib.Path(sys.argv[1]);deps=r/'deps';stamp=(r/'.deps-stamp').read_bytes()
files={str(p.relative_to(deps)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(deps.rglob('*')) if p.is_file() and '__pycache__' not in p.parts and p.suffix!='.pyc'}
print(json.dumps({'stamp':json.loads(stamp),'stamp_sha256':hashlib.sha256(stamp).hexdigest(),'files':files,'dependency_mtime_ns':(deps/'cache_probe.py').stat().st_mtime_ns,'pip_cache_nonempty':any(p.is_file() for p in (r/'.pip-cache').rglob('*')),'interpreter':{'executable':sys.executable,'version':sys.version,'platform':sysconfig.get_platform()},'script_sha256':hashlib.sha256((r/'scripts/build-standalone.py').read_bytes()).hexdigest(),'artifact_helper_sha256':hashlib.sha256((r/'scripts/artifact_pack.py').read_bytes()).hexdigest()}))
'''
        return json.loads(self.execute("python3", "-c", code, self.workspace + "/" + SERVICE))

    def index_stats(self):
        code = "import urllib.request;print(urllib.request.urlopen(__import__('sys').argv[1],timeout=5).read().decode())"
        return json.loads(self.execute("python3", "-c", code, self.index_base + "/__stats"))

    def artifacts(self, task, stage):
        relative = task.get("artifact_path")
        if not relative or Path(relative).is_absolute() or ".." in Path(relative).parts:
            raise RuntimeError("build artifact path is absent or unsafe")
        child, package = self.root / (stage + "-child.zip"), self.root / (stage + "-workspace.zip")
        docker("cp", self.cid + ":" + self.workspace + "/" + SERVICE + "/artifact.zip", str(child))
        docker("cp", self.cid + ":" + self.workspace + "/" + relative, str(package))
        if task.get("sha256") != digest(package.read_bytes()) or task.get("size_bytes") != package.stat().st_size:
            raise RuntimeError("workspace artifact differs from completed task receipt")
        return inspect_artifacts(child, package)

    def run(self):
        data, identity = frozen_template(self.args.template_source)
        self.report["template"] = identity
        self.check("Python cache frozen template inputs", bool(identity["commit"]) and bool(identity["files_sha256"]))
        for name in ("rcoder-app-builder-" + self.app, self.receipt["index_name"]):
            if docker("ps", "-aq", "--no-trunc", "--filter", "name=^/" + name + "$").stdout.strip():
                raise RuntimeError("fixture compute name already exists")
        self.receipt["builder_creation_state"] = "creating"
        self.save()
        _, response = self.api("/api/v1/userapp/workspace", {"app_id": self.app, "user_id": self.args.user_id})
        row, before = self.builder()
        self.check("Python cache public workspace created", response["data"].get("container_name") == before["name"].removeprefix("/"), before)
        networks = row["NetworkSettings"]["Networks"]
        candidates = [name for name, value in networks.items() if value.get("IPAddress") and name not in ("host", "none")]
        if len(candidates) != 1:
            raise RuntimeError("builder requires one reachable bridge network for private index")
        network = candidates[0]
        if json.loads(docker("network", "inspect", network).stdout)[0].get("Driver") != "bridge":
            raise RuntimeError("private cache fixture requires Docker bridge network")
        index = self.root / "index"
        (index / "simple/cache-probe").mkdir(parents=True)
        name = create_wheel(index)
        (index / "simple/cache-probe/index.html").write_text(f'<a href="../../{name}">{name}</a>\n')
        (index / "serve.py").write_text(INDEX_SERVER)
        self.receipt["index_creation_state"] = "creating"
        self.receipt["index_image"] = before["image"]
        self.save()
        index_id = docker("create", "--name", self.receipt["index_name"], "--user", "0:0", "--network", network,
               "--label", "rcoder.e2e.run=" + self.receipt["run_id"], "--label", "rcoder.e2e.case=" + self.receipt["case_id"],
               "--mount", "type=bind,src=" + str(index) + ",dst=/index", "--entrypoint", "python3", before["image"], "/index/serve.py").stdout.strip()
        if not re.fullmatch(r"[0-9a-f]{64}", index_id):
            raise RuntimeError("private index create returned no exact physical identity")
        self.receipt.update(index_id=index_id, index_creation_state="created")
        self.save()
        docker("start", index_id)
        index_row = json.loads(docker("inspect", self.receipt["index_name"]).stdout)[0]
        self.receipt["index_id"] = index_row["Id"]
        self.save()
        address = index_row["NetworkSettings"]["Networks"][network]["IPAddress"]
        self.index_base = f"http://{address}:8080"
        # Address remains fixed because this index compute survives builder replacement.
        self.wait("local index", lambda: docker("exec", self.receipt["index_id"], "python3", "-c",
                  "import urllib.request;print(urllib.request.urlopen('http://127.0.0.1:8080/__stats').status)", check=False).returncode,
                  lambda value: value == 0, seconds=15)
        self.check("Python cache local index ready", self.index_stats() == [], {"index_image": before["image"], "network": network})
        raw, hashes = fixture_zip(data, self.index_base + "/simple/", self.marker)
        (self.root / "template.zip").write_bytes(raw)
        self.report["fixture_files_sha256"] = hashes
        boundary = "rcoder-cache-" + uuid.uuid4().hex
        form = bytearray()
        for key, value in (("app_id", self.app), ("user_id", self.args.user_id), ("enable_git", "false")):
            form.extend(f'--{boundary}\r\nContent-Disposition: form-data; name="{key}"\r\n\r\n{value}\r\n'.encode())
        form.extend(f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="template.zip"\r\nContent-Type: application/zip\r\n\r\n'.encode())
        form.extend(raw); form.extend(f"\r\n--{boundary}--\r\n".encode())
        self.api("/api/v1/userapp/init-project-template", raw=bytes(form), content_type="multipart/form-data; boundary=" + boundary)
        self.check("Python cache public template upload accepted", True, {"zip_sha256": digest(raw)})
        task = self.task("build")
        first, requests = self.snapshot(), dependency_requests(self.index_stats())
        self.report["first_snapshot"] = first
        self.report["first_index_requests"] = requests
        self.check("Python cache first build installs real dependency", bool(requests) and any(path.endswith(".whl") for path in requests)
                   and first["files"] and first["stamp"]["files"].get("cache_probe.py", 0) > 0 and first["pip_cache_nonempty"]
                   and first["script_sha256"] == identity["files_sha256"][f"{SERVICE}/scripts/build-standalone.py"]
                   and first["artifact_helper_sha256"] == identity["files_sha256"][f"{SERVICE}/scripts/artifact_pack.py"], first)
        self.check("Python cache artifacts preserve deps and exclude caches", self.artifacts(task, "first")["raw_copy_verified"])
        self.task("build")
        second = self.snapshot()
        self.check("Python cache second build performs no index requests", dependency_requests(self.index_stats()) == requests and second == first)
        _, response = self.api("/computer/pod/stop", {"app_id": self.app, "app_stage": "dev", "service_type": "userapp",
                    "user_id": self.args.user_id, "project_id": self.app, "lifecycle_id": before["lifecycle_id"], "request_id": "stop-" + uuid.uuid4().hex})
        operation = response["data"]
        operation_id = response.get("operation_id")
        expected_path = f"/computer/pod/operations/{self.app}/{operation_id}"
        if not operation_id or operation.get("operation_id") != operation_id or operation.get("status_url") != expected_path or (
                operation.get("app_id") != self.app or operation.get("scope") != "Dev"
                or operation.get("lifecycle_id") != before["lifecycle_id"] or operation.get("action") != "stop"):
            raise RuntimeError("platform Stop did not return exact operation identity")
        def observe_stop():
            _, response = self.api(expected_path)
            value = response["data"]
            if value.get("operation_id") != operation_id or value.get("app_id") != self.app or (
                    value.get("lifecycle_id") != before["lifecycle_id"] or value.get("scope") != "Dev"
                    or value.get("action") != "stop"):
                raise RuntimeError("Stop operation identity changed")
            if value.get("state") in ("failed", "superseded", "recovery_required"):
                raise RuntimeError("Stop failed or requires recovery")
            return value
        stopped = self.wait("platform Stop", observe_stop, lambda value: value.get("state") == "succeeded")
        absent = docker("ps", "-aq", "--no-trunc", "--filter", "id=" + before["id"]).stdout.strip() == ""
        self.check("Python cache platform stop completes original operation", absent and stopped["operation_id"] == operation_id, stopped)
        self.receipt["builder_creation_state"] = "creating"
        self.save()
        self.api("/api/v1/userapp/workspace", {"app_id": self.app, "user_id": self.args.user_id})
        _, after = self.builder()
        prove_recreation(before, after)
        preserved = self.execute("python3", "-c", "import pathlib,sys;print(pathlib.Path(sys.argv[1]).read_text())", self.workspace + "/cache-sentinel").strip()
        self.check("Python cache recreated builder retains original volume and data", preserved == self.marker and self.snapshot() == first,
                   {"before": before, "after": after})
        task = self.task("build")
        self.check("Python cache third build performs no index requests", dependency_requests(self.index_stats()) == requests and self.snapshot() == first)
        self.artifacts(task, "third")
        self.task("dev/start")
        def business():
            result = docker("exec", self.cid, "curl", "-fsS", "--max-time", "3", "http://127.0.0.1:9080/api/python/ready", check=False)
            return json.loads(result.stdout) if result.returncode == 0 else None
        response = self.wait("dependency HTTP", business, lambda value: value == {"marker": self.marker, "dependency_version": "1.0"}, seconds=90)
        self.check("Python cache dev start serves installed dependency over HTTP", response["dependency_version"] == "1.0"
                   and dependency_requests(self.index_stats()) == requests and self.snapshot() == first, response)
        _, final_identity = frozen_template(self.args.template_source)
        self.check("Python cache final source inputs unchanged", final_identity == identity)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rcoder", default=os.environ.get("RCODER_URL", "http://127.0.0.1:8090"))
    parser.add_argument("--template-source", type=Path, default=Path(os.environ.get("E2E_TEMPLATE_SOURCE_DIR", str(REPO.parent / "userapp-workspace-template"))))
    parser.add_argument("--report", required=True, type=Path)
    parser.add_argument("--user-id", default="e2e-python-cache")
    args = parser.parse_args()
    contract = Contract(args)
    try:
        contract.run()
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
        contract.report["error"] = type(error).__name__ + ": " + str(error)[:500]
    finally:
        try:
            result = cleanup(contract.root, contract.receipt["run_id"], contract.receipt["case_id"])
            contract.report["cleanup"] = result
            contract.check("Python cache owned compute cleanup preserves data", result["ok"] and result["data_retained"], result)
        except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
            contract.report["cleanup_error"] = type(error).__name__ + ": " + str(error)[:200]
        passed = {check["name"] for check in contract.report["checks"] if check["ok"]}
        contract.report["missing"] = sorted(REQUIRED_STEPS - passed)
        contract.report["success"] = not contract.report.get("error") and not contract.report.get("cleanup_error") and not contract.report["missing"]
        contract.save()
    return 0 if contract.report["success"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
