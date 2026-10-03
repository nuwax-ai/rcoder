#!/usr/bin/env python3
"""Two isolated, real-agent UserApp repair cases. Preserves volumes, removes owned containers.

This spends LLM tokens when run. --help and importing the module do not contact a model.
No shared RCoder instance is used: current file-server-proxy and app-cli run in each
new container, and the image's OpenCode CLI performs all project adaptation.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import shlex
import subprocess
import sys
import time
import urllib.parse
import uuid
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
CASES = ("misplaced-nested", "imported-source")
SKILLS = ("userapp-scaffold", "userapp-dev-guide")
REQUIRED_CHECKS = {
    "current_binaries", "agent_capabilities", "prompt_and_skills",
    "unadapted_fixture", "initial_failed_task", "agent_completed",
    "guide_and_reference_read", "source_read", "project_changed",
    "original_source_and_data_preserved", "root_validate", "production_build",
    "dev_task_completed", "real_html", "real_api", "controlled_stop",
    "data_preserved_after_runtime", "container_removed",
}


class Failed(RuntimeError):
    pass


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def dotenv(path: Path) -> dict[str, str]:
    """Parse literal values, never source a shell file or expand command substitution."""
    result = {}
    for line in path.read_text().splitlines():
        match = re.match(r"\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*)", line)
        if not match:
            continue
        key, raw = match.groups()
        if key not in {"LLM_API_KEY", "LLM_BASE_URL", "LLM_MODEL"}:
            continue
        values = shlex.split(raw, comments=True, posix=True)
        if len(values) > 1:
            raise Failed(f"{key} must be one literal value")
        result[key] = values[0] if values else ""
    missing = [key for key in ("LLM_API_KEY", "LLM_BASE_URL", "LLM_MODEL") if not result.get(key)]
    if missing:
        raise Failed("missing model configuration: " + ", ".join(missing))
    return result


class Redactor:
    def __init__(self, secrets=()):
        self.secrets = sorted((value for value in secrets if value), key=len, reverse=True)

    def text(self, value: str) -> str:
        for secret in self.secrets:
            value = value.replace(secret, "[REDACTED]")
        # Tool errors can echo a URL after normalizing the configured endpoint.
        value = re.sub(r"(https?://)[^/\s\"'\\]*@", r"\1[REDACTED]@", value)
        return re.sub(r"(https?://[^\s\"'?#\\]+)\?[^\s\"'\\]+", r"\1?[REDACTED]", value)

    def value(self, value):
        if isinstance(value, str):
            return self.text(value)
        if isinstance(value, list):
            return [self.value(item) for item in value]
        if isinstance(value, dict):
            return {key: ("[REDACTED]" if key.lower() in {"api_key", "apikey", "authorization", "baseurl", "base_url"}
                          else self.value(item)) for key, item in value.items()}
        return value

    def artifact(self, content, *, json_lines=False) -> str:
        if json_lines:
            return "".join(json.dumps(self.value(json.loads(line)), ensure_ascii=False) + "\n"
                           for line in content.splitlines() if line.strip())
        if isinstance(content, str):
            return self.text(content)
        return json.dumps(self.value(content), ensure_ascii=False, indent=2)


def connection_refused(observation: dict) -> bool:
    """Only a completed native socket observation can prove a closed endpoint."""
    return (observation.get("observed") is True
            and isinstance(observation.get("errno"), int)
            and observation["errno"] == observation.get("refused_errno")
            and observation.get("errno_name") == "ECONNREFUSED")


TCP_PROBE = """import datetime,errno,json,socket,sys
sock=socket.socket();sock.settimeout(.3)
try:
 code=sock.connect_ex(('127.0.0.1',int(sys.argv[1])))
 print(json.dumps({'observed':True,'time':datetime.datetime.now(datetime.timezone.utc).isoformat(),'errno':code,'errno_name':errno.errorcode.get(code,'CONNECTED' if code==0 else 'UNKNOWN'),'refused_errno':errno.ECONNREFUSED}))
except OSError as error:
 print(json.dumps({'observed':False,'error_type':type(error).__name__,'errno':error.errno}))
finally: sock.close()
"""


def provider_config(values: dict[str, str], model: str, prompt_path: str) -> dict:
    return {
        "model": f"verification/{model}",
        "enabled_providers": ["verification"],
        "provider": {"verification": {
            "npm": "@ai-sdk/openai-compatible", "name": "Verification",
            "options": {"baseURL": values["LLM_BASE_URL"], "apiKey": values["LLM_API_KEY"]},
            "models": {model: {"name": model, "limit": {"context": 128000, "output": 16384}}},
        }},
        "instructions": [prompt_path],
        "permission": {
            "read": "allow", "edit": "allow",
            "bash": {"*": "allow", "pkill*": "deny", "killall*": "deny"},
            "external_directory": "deny", "webfetch": "deny", "websearch": "deny",
        },
    }


def server_source(marker: str, imported: bool) -> str:
    shared = "import { message } from '../../packages/shared/index.mjs';" if imported else f"const message = {json.dumps(marker)};"
    return f"""import http from 'node:http';
import fs from 'node:fs';
{shared}
const page = fs.readFileSync(new URL('./index.html', import.meta.url));
http.createServer((req, res) => {{
  if (req.url === '/health') {{ res.writeHead(200, {{'content-type':'text/plain'}}); res.end('ready'); }}
  else if (req.url === '/api/message') {{ res.writeHead(200, {{'content-type':'application/json'}}); res.end(JSON.stringify({{message}})); }}
  else {{ res.writeHead(200, {{'content-type':'text/html'}}); res.end(page); }}
}}).listen(Number(process.env.PORT || 3000), '0.0.0.0');
"""


def fixture(case: str, app: str) -> tuple[dict[str, str], list[str], list[str], str]:
    """Original projects only. The imported case intentionally has no UserApp manifests."""
    marker = "nested-original-v1" if case == "misplaced-nested" else "imported-business-v1"
    common = {
        "AGENTS.md": "# Original project rules\n保留既有页面、API、server.mjs 和 shared 业务实现；不添加第三方依赖，不提交 Git。SENTRY、userdata 与 state 中的既有文件必须保留。\n",
        "SENTRY": f"source-sentry-{app}\n",
        "userdata/keep.txt": f"user-data-{app}\n",
        "state/SENTRY": f"platform-state-{app}\n",
        ".gitignore": ".agents/\n.opencode/\n.claude/\nnode_modules/\nstate/*\n!state/SENTRY\nbuilds/\n.run/\n.previous/\n.staging/\n.local-deploy/\nrelease.lock.toml\n*.zip\ndist/\n",
    }
    if case == "misplaced-nested":
        common.update({
            "README.md": "已有 web 应用，页面及 API 均须保留；当前平台构建和预览不可用。\n",
            "code/workspace.manifest.toml": 'schema_version=1\n[workspace]\nname="existing-nested-app"\n',
            "code/web/project.manifest.toml": '''schema_version=1
[project]
service_id="web"
name="Existing web"
type="node"
[build]
command=["python3","-m","zipfile","-c","artifact.zip","server.mjs","index.html"]
artifact="artifact.zip"
[run]
command=["node","server.mjs"]
[devrun]
command=["node","server.mjs"]
[health]
readiness_path="/health"
[proxy]
path="/"
strip_prefix=false
''',
            "code/web/server.mjs": server_source(marker, False),
            "code/web/index.html": f"<!doctype html><title>Existing nested page</title><main>{marker}</main>\n",
        })
        return common, ["web"], ["code/web/server.mjs", "code/web/index.html"], marker
    common.update({
        "README.md": "原始 monorepo 只有 site 服务，保留该服务名称和共享模块；尚未接入 UserApp。直接 node apps/site/server.mjs 可运行。\n",
        "package.json": json.dumps({"name": "existing-site", "version": "1.0.0", "private": True, "type": "module", "workspaces": ["apps/*", "packages/*"], "scripts": {"start": "node apps/site/server.mjs"}}, indent=2) + "\n",
        "package-lock.json": json.dumps({"name": "existing-site", "version": "1.0.0", "lockfileVersion": 3, "requires": True, "packages": {"": {"name": "existing-site", "version": "1.0.0", "workspaces": ["apps/*", "packages/*"]}, "apps/site": {"version": "1.0.0"}, "packages/shared": {"name": "@fixture/shared", "version": "1.0.0"}, "node_modules/site": {"resolved": "apps/site", "link": True}, "node_modules/@fixture/shared": {"resolved": "packages/shared", "link": True}}}, indent=2) + "\n",
        "apps/site/package.json": '{"name":"site","version":"1.0.0","private":true,"type":"module"}\n',
        "apps/site/server.mjs": server_source(marker, True),
        "apps/site/index.html": f"<!doctype html><title>Original imported page</title><main>{marker}</main>\n",
        "packages/shared/package.json": '{"name":"@fixture/shared","version":"1.0.0","type":"module","exports":"./index.mjs"}\n',
        "packages/shared/index.mjs": f"export const message = {json.dumps(marker)};\n",
    })
    return common, ["site"], ["apps/site/server.mjs", "apps/site/index.html", "packages/shared/index.mjs"], marker


def tool_evidence(events: list[dict]) -> dict:
    evidence = {"tools": 0, "guide": [], "reference": [], "source": [], "terminal": False}
    for event in events:
        if event.get("type") == "step_finish" and event.get("part", {}).get("reason") in {"stop", "end_turn"}:
            evidence["terminal"] = True
        part = event.get("part", {})
        state = part.get("state", {})
        if event.get("type") != "tool_use" or state.get("status") != "completed":
            continue
        evidence["tools"] += 1
        tool = part.get("tool", "")
        inputs = state.get("input", {})
        text = json.dumps(inputs, ensure_ascii=False)
        output = state.get("output", "")
        if not isinstance(output, str) or not output.strip():
            continue
        reading = tool in {"read", "read_file", "skill"}
        if tool in {"bash", "shell"}:
            command = str(inputs.get("command", ""))
            reading = (state.get("metadata", {}).get("exit") == 0
                       and re.search(r"(?:^|[\s;&|])(?:cat|sed|head|tail|rg)\s", command) is not None)
        if not reading:
            continue
        item = {"tool": tool, "input": inputs, "output_chars": len(output),
                "output_sha256": sha(output.encode()), "exit": state.get("metadata", {}).get("exit")}
        if ("userapp-dev-guide" in text and (tool == "skill" or "SKILL.md" in text)
                and "SOURCE_WORKSPACE" in output and "repair_target" in output):
            evidence["guide"].append(item)
        if "import-existing-project.md" in text and "失败任务与诊断" in output and "SOURCE_WORKSPACE" in output:
            evidence["reference"].append(item)
        source_body = (("server.mjs" in text and "createServer" in output)
                       or ("package.json" in text and '"name"' in output)
                       or ("manifest.toml" in text and "schema_version" in output
                           and ("[workspace]" in output or "[project]" in output)))
        if source_body:
            evidence["source"].append(item)
    return evidence


def sse_events(text: str) -> list[dict]:
    events = []
    for frame in re.split(r"\r?\n\r?\n", text):
        name, data = "", []
        for line in frame.splitlines():
            if line.startswith("event:"):
                name = line[6:].strip()
            elif line.startswith("data:"):
                data.append(line[5:].lstrip())
        if name and data:
            events.append({"event": name, "data": json.loads("\n".join(data))})
    return events


class Harness:
    def __init__(self, args, values, image):
        self.args, self.values, self.image = args, values, image
        self.run_id = uuid.uuid4().hex
        self.redactor = Redactor([values["LLM_API_KEY"], values["LLM_BASE_URL"]])
        self.artifacts = args.report.parent / (args.report.stem + "-artifacts")
        self.artifacts.mkdir(parents=True, exist_ok=False)
        self.report = {"run_id": self.run_id, "image": image, "model": args.model or values["LLM_MODEL"],
                       "planned": list(args.case or CASES), "preflight_only": args.preflight_only, "ai_executed": False, "success": False, "cases": [],
                       "binaries": {name: {"path": str(path.resolve()), "sha256": sha(path.read_bytes())}
                                    for name, path in (("app-cli", args.app_cli), ("file-server-proxy", args.file_server_proxy))}}

    def save(self):
        self.args.report.parent.mkdir(parents=True, exist_ok=True)
        self.args.report.write_text(json.dumps(self.redactor.value(self.report), ensure_ascii=False, indent=2))

    def command(self, argv, *, stdin=None, timeout=60, check=True):
        try:
            result = subprocess.run(argv, input=stdin, capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired as error:
            if not check:
                def decoded(value):
                    return value.decode(errors="replace") if isinstance(value, bytes) else (value or "")
                return subprocess.CompletedProcess(argv, 124, decoded(error.stdout), decoded(error.stderr) + f"\nTimed out after {timeout}s")
            raise Failed(f"command timed out after {timeout}s: {argv[0]}") from error
        if check and result.returncode:
            raise Failed(self.redactor.text(f"{argv[0]} failed ({result.returncode}): {result.stderr[-5000:]}"))
        return result

    def docker(self, *argv, **kwargs):
        return self.command(["docker", *argv], **kwargs)

    def record(self, case, name, ok, detail=None):
        case["checks"].append({"name": name, "ok": bool(ok), "detail": self.redactor.value(detail)})
        self.save()
        print(f"{case['case']}: {name}: {'PASS' if ok else 'FAIL'}", flush=True)
        if not ok:
            raise Failed(name)

    def artifact(self, case, name, content):
        path = self.artifacts / case["case"] / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(self.redactor.artifact(content, json_lines=name.endswith(".jsonl")))
        return str(path.resolve())

    def execute(self, cid, *argv, **kwargs):
        return self.docker("exec", "-i", cid, *argv, **kwargs)

    def write_files(self, cid, root, files):
        program = "import json,pathlib,sys; root=pathlib.Path(sys.argv[1]); files=json.load(sys.stdin); [(root.joinpath(p).parent.mkdir(parents=True,exist_ok=True),root.joinpath(p).write_text(t)) for p,t in files.items()]"
        self.execute(cid, "python3", "-c", program, root, stdin=json.dumps(files))

    def http(self, cid, path, *, method="GET", body=None, port=60000, timeout=15):
        program = """import json,sys,urllib.request,urllib.error
p=json.load(sys.stdin); body=json.dumps(p['body']).encode() if p['body'] is not None else None
request=urllib.request.Request('http://127.0.0.1:'+str(p['port'])+p['path'],data=body,method=p['method'],headers={'content-type':'application/json'})
opener=urllib.request.build_opener(urllib.request.ProxyHandler({}))
try: response=opener.open(request,timeout=p['timeout'])
except urllib.error.HTTPError as error: response=error
print(json.dumps({'status':response.status,'content_type':response.headers.get('content-type',''),'body':response.read().decode()}))
"""
        result = self.execute(cid, "python3", "-c", program,
                              stdin=json.dumps({"path": path, "method": method, "body": body, "port": port, "timeout": timeout}),
                              timeout=timeout + 5)
        return json.loads(result.stdout)

    def spawn(self, cid, workspace, log, argv):
        program = "import os,sys; os.chdir(sys.argv[1]); stream=open(sys.argv[2],'ab',buffering=0); os.dup2(stream.fileno(),1); os.dup2(stream.fileno(),2); os.execvp(sys.argv[3],sys.argv[3:])"
        self.docker("exec", "-d", cid, "python3", "-c", program, workspace, log, *argv)

    def tcp_probe(self, cid, port=9080):
        result = self.execute(cid, "python3", "-c", TCP_PROBE, str(port), timeout=5)
        observation = json.loads(result.stdout)
        if observation.get("observed") is not True:
            raise Failed(f"TCP observation failed: {observation.get('error_type', 'invalid response')}")
        return observation

    def runtime_status(self, cid, state_root):
        program = """import json,pathlib,sys,urllib.request
token=pathlib.Path(sys.argv[1],'token').read_text().strip()
request=urllib.request.Request('http://127.0.0.1:3010/v1/runtime/status',headers={'X-Deploy-Token':token})
opener=urllib.request.build_opener(urllib.request.ProxyHandler({}))
with opener.open(request,timeout=3) as response: print(response.read().decode())
"""
        return json.loads(self.execute(cid, "python3", "-c", program, state_root).stdout)

    def snapshot(self, cid, workspace):
        program = """import hashlib,json,pathlib,sys
root=pathlib.Path(sys.argv[1]); exclude={'.git','.agents','.opencode','.claude','node_modules','.run','.previous','.staging','builds','state','dist'}
result={str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in root.rglob('*') if p.is_file() and not p.is_symlink() and not any(part in exclude for part in p.relative_to(root).parts) and p.suffix!='.zip'}
for name in ['SENTRY','userdata/keep.txt','state/SENTRY','package-lock.json']:
 p=root/name
 if p.is_file(): result[name]=hashlib.sha256(p.read_bytes()).hexdigest()
print(json.dumps(result))
"""
        return json.loads(self.execute(cid, "python3", "-c", program, workspace).stdout)

    def validate(self, cid, workspace):
        result = self.execute(cid, "/usr/local/bin/app-cli", "validate", "--workspace", workspace, "--dev", "--json", check=False)
        try:
            report = json.loads(result.stdout)
        except ValueError as error:
            raise Failed(f"validate did not return one JSON report (exit {result.returncode})") from error
        return result.returncode, report

    def preserved(self, before, after, movable):
        pinned = ["SENTRY", "userdata/keep.txt", "state/SENTRY"]
        if "package-lock.json" in before:
            pinned += ["package-lock.json", "packages/shared/index.mjs", "apps/site/server.mjs", "apps/site/index.html"]
        return all(after.get(path) == before[path] for path in pinned) and all(before[path] in after.values() for path in movable)

    def run_case(self, name):
        app = "repair" + uuid.uuid4().hex[:12]
        workspace = f"/home/user/{app}"
        volume = f"rcoder-project-repair-{self.run_id}-{name}"
        case = {"case": name, "app_id": app, "workspace": workspace, "volume": volume, "checks": [], "success": False}
        self.report["cases"].append(case)
        cid = None
        try:
            self.docker("volume", "create", "--label", f"rcoder.repair.run={self.run_id}", volume)
            domain = {"authority": "userapp-project-repair", "volume": volume, "instance_source_env": "RCODER_PHYSICAL_POD_UID", "instance": ""}
            env = {"PROJECT_ID": app, "SERVICE_TYPE": "userapp-builder", "APP_CLI_MANAGED": "1",
                   "APP_CLI_RUNTIME_WORKSPACE": workspace, "APP_CLI_WORKSPACE": workspace,
                   "APP_CLI_STATE_ROOT": f"{workspace}/state/{app}", "USERAPP_WORKSPACE_DIR": workspace,
                   "USERAPP_SINGLE_APP_ID": app, "APP_CLI_REQUIRE_PG": "0",
                   "APP_CLI_SUPERVISOR_SOCKET": "/run/verification/no-supervisord.sock",
                   "LOG_BASE_DIR": "/run/verification/runtime-logs", "FILE_SERVER_LOG_DIR": "/run/verification/proxy-logs",
                   "FILE_SERVER_APP_CLI_BIN": "/usr/local/bin/app-cli", "RCODER_RUNTIME_IMAGE_DIGEST": self.image,
                   "RCODER_EXECUTION_DOMAIN": json.dumps(domain), "RCODER_PHYSICAL_POD_UID": str(uuid.uuid4())}
            argv = ["run", "-d", "--name", f"rcoder-project-repair-{app}", "--label", f"rcoder.repair.run={self.run_id}",
                    "--mount", f"type=volume,src={volume},dst={workspace},volume-nocopy", "--tmpfs", "/run/verification:rw,exec,mode=1777"]
            for key, value in env.items():
                argv += ["-e", f"{key}={value}"]
            cid = self.docker(*argv, "--entrypoint", "sleep", self.image, "infinity").stdout.strip()
            case["container_id"] = cid
            for binary in ("app-cli", "file-server-proxy"):
                path = getattr(self.args, binary.replace("-", "_"))
                self.docker("cp", str(path.resolve()), f"{cid}:/usr/local/bin/{binary}")
            actual = self.execute(cid, "sha256sum", "/usr/local/bin/app-cli", "/usr/local/bin/file-server-proxy").stdout
            self.record(case, "current_binaries", all(item["sha256"] in actual for item in self.report["binaries"].values()), actual)
            capabilities = json.loads(self.execute(cid, "python3", "-c", "import json,os,shutil; print(json.dumps({'cli':shutil.which('opencode') or shutil.which('nuwaxcode'),'node':shutil.which('node'),'git':shutil.which('git'),'pingap_version':os.getenv('RCODER_PINGAP_VERSION'),'pingap_commit':os.getenv('RCODER_PINGAP_COMMIT')}))").stdout)
            cli = capabilities.get("cli")
            self.record(case, "agent_capabilities", bool(cli and capabilities.get("node") and capabilities.get("git") and capabilities.get("pingap_version") and capabilities.get("pingap_commit")), capabilities)
            case["agent_version"] = self.execute(cid, cli, "--version").stdout.strip()
            help_result = self.execute(cid, cli, "run", "--help")
            help_text = help_result.stdout + help_result.stderr
            self.artifact(case, "agent-help.txt", help_text)
            if not all(option in help_text for option in ("--format", "--model", "--dir")):
                raise Failed("installed OpenCode CLI lacks the required JSON/model/directory interface")
            files, expected_services, movable, marker = fixture(name, app)
            self.write_files(cid, workspace, files)
            case["original_fixture"] = self.artifact(case, "original-files.json", files)
            template = self.args.template_root
            prompt = (template / "prompts/sandbox-system-prompt.md").read_text()
            prompt += (f"\n\n本次隔离应用的基础设施信息：app_id={app}，平台源码根={workspace}。"
                       f"现有 UserApp 控制入口 http://127.0.0.1:60000/api/v1/userapp，启动/重启请求JSON含app_id={app}。"
                       "请使用已安装技能与真实工具完成任务；不要读取或打印模型凭据、进程环境或自建模型配置。"
                       "不提交Git，不修改SENTRY/userdata/既有state文件，不pkill/清锁解锁。\n")
            self.write_files(cid, "/run/verification", {"system-prompt.md": prompt})
            expected_skill_hashes = {}
            for skill in SKILLS:
                source = template / "skills" / skill
                if not (source / "SKILL.md").is_file():
                    raise Failed(f"missing complete skill: {skill}")
                for item in source.rglob("*"):
                    if item.is_file():
                        expected_skill_hashes[f"{skill}/{item.relative_to(source)}"] = sha(item.read_bytes())
                for directory in (".agents/skills", ".opencode/skills"):
                    self.execute(cid, "mkdir", "-p", f"{workspace}/{directory}")
                    self.docker("cp", str(source), f"{cid}:{workspace}/{directory}/")
            skill_hashes = json.loads(self.execute(cid, "python3", "-c", "import hashlib,json,pathlib,sys; root=pathlib.Path(sys.argv[1]); print(json.dumps({str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in root.rglob('*') if p.is_file()}))", f"{workspace}/.agents/skills").stdout)
            case["prompt_sha256"] = sha(prompt.encode())
            case["skill_hashes"] = skill_hashes
            probe_config = provider_config({"LLM_BASE_URL": "http://127.0.0.1:9/v1", "LLM_API_KEY": "nonsecret-config-probe"}, self.report["model"], "/run/verification/system-prompt.md")
            probe_code = """import hashlib,json,os,pathlib,subprocess,sys
p=json.load(sys.stdin); os.environ['OPENCODE_CONFIG_CONTENT']=json.dumps(p['config'])
for key,name in [('HOME','home'),('XDG_CONFIG_HOME','config'),('XDG_DATA_HOME','data'),('XDG_CACHE_HOME','cache'),('XDG_STATE_HOME','state')]:
 directory='/run/verification/probe/'+name; pathlib.Path(directory).mkdir(parents=True,exist_ok=True); os.environ[key]=directory
result=subprocess.run([p['cli'],'debug','config','--pure'],capture_output=True,text=True)
if result.returncode: print(result.stderr,file=sys.stderr); sys.exit(result.returncode)
resolved=json.loads(result.stdout)
print(json.dumps({'instructions':resolved.get('instructions',[]),'model':resolved.get('model'),'provider_present':'verification' in resolved.get('provider',{}),'prompt_sha256':hashlib.sha256(pathlib.Path('/run/verification/system-prompt.md').read_bytes()).hexdigest()}))
"""
            pure_probe = json.loads(self.execute(cid, "python3", "-c", probe_code, stdin=json.dumps({"config": probe_config, "cli": cli})).stdout)
            self.record(case, "prompt_and_skills", skill_hashes == expected_skill_hashes
                        and "userapp-dev-guide/references/import-existing-project.md" in skill_hashes
                        and "/run/verification/system-prompt.md" in pure_probe["instructions"]
                        and pure_probe["prompt_sha256"] == case["prompt_sha256"]
                        and pure_probe["provider_present"], pure_probe)
            self.execute(cid, "git", "-C", workspace, "init", "-q")
            self.execute(cid, "git", "-C", workspace, "add", "--", *files)
            self.execute(cid, "git", "-C", workspace, "-c", "user.name=Fixture", "-c", "user.email=fixture@localhost", "commit", "-qm", "Original unadapted fixture")
            original_head = self.execute(cid, "git", "-C", workspace, "rev-parse", "HEAD").stdout.strip()
            before = self.snapshot(cid, workspace)
            self.artifact(case, "before.json", before)
            initial_exit, initial = self.validate(cid, workspace)
            self.record(case, "unadapted_fixture", initial_exit != 0 and initial.get("valid") is False and "workspace.manifest.toml" not in before,
                        {"exit_code": initial_exit, "validation": initial})
            self.execute(cid, "mkdir", "-p", "/run/verification/runtime-logs")
            self.spawn(cid, workspace, "/run/verification/proxy.log", ["/usr/local/bin/file-server-proxy", "--embed", "--policy", "all_rust", "--port", "60000"])
            deadline = time.monotonic() + 30
            while True:
                try:
                    if self.http(cid, "/health", timeout=2)["status"] == 200:
                        break
                except (Failed, ValueError):
                    pass
                if time.monotonic() >= deadline:
                    raise Failed("current file-server-proxy did not become ready")
                time.sleep(0.2)
            initial_task = json.loads(self.http(cid, "/api/v1/userapp/dev/start", method="POST", body={"app_id": app})["body"])
            data = initial_task.get("data") or {}
            task_id = data.get("task_id")
            self.record(case, "initial_failed_task", initial_task.get("success") is False and bool(task_id)
                        and any(item.get("repair_target") == "project" and item.get("scope") == "task" for item in data.get("diagnostics", [])), initial_task)
            task_snapshot = self.http(cid, f"/api/v1/userapp/tasks/{task_id}?app_id={app}")
            initial_sse = self.http(cid, f"/api/v1/userapp/tasks/{task_id}/logs/stream?app_id={app}")
            self.artifact(case, "initial-task.json", task_snapshot)
            self.artifact(case, "initial-task.sse", initial_sse["body"])
            initial_events = sse_events(initial_sse["body"])
            retrieved = json.loads(task_snapshot["body"]).get("data", {})
            diagnostic_keys = ("code", "phase", "repair_target", "scope", "workspace_root", "detected_workspace_root", "file", "field", "service_id")
            stable_diagnostics = lambda items: [{key: item.get(key) for key in diagnostic_keys} for item in items]
            if (retrieved.get("status") != "failed" or retrieved.get("id") != task_id
                    or stable_diagnostics(retrieved.get("diagnostics", [])) != stable_diagnostics(data.get("diagnostics", []))
                    or not any(event["event"] == "failed" for event in initial_events)
                    or not any(event["event"] == "log" and event["data"].get("service") == "workspace"
                               and event["data"].get("line") for event in initial_events)):
                raise Failed("initial failed task was not recoverable through GET and SSE")
            if self.args.preflight_only:
                case["preflight_passed"] = True
                return
            user_prompt = ("这个已有应用在平台无法构建和预览。请保留原页面与API业务，把它接入当前工作目录代表的应用，并验证实际可访问。"
                           if name == "misplaced-nested" else
                           "这是已有site网页和API项目。请把site作为独立服务接入UserApp开发预览，保留现有接口、页面、共享模块和依赖管理，并完成实际验证。")
            user_prompt += "\n当前平台失败诊断：" + json.dumps(data.get("diagnostics", []), ensure_ascii=False)
            case["user_prompt"] = user_prompt
            config = provider_config(self.values, self.report["model"], "/run/verification/system-prompt.md")
            agent_runner = """import json,os,pathlib,sys
payload=json.load(sys.stdin)
for key,directory in [('HOME','home'),('XDG_CONFIG_HOME','config'),('XDG_DATA_HOME','data'),('XDG_CACHE_HOME','cache'),('XDG_STATE_HOME','state')]:
 path='/run/verification/agent/'+directory; pathlib.Path(path).mkdir(parents=True,exist_ok=True); os.environ[key]=path
os.environ['OPENCODE_CONFIG_CONTENT']=json.dumps(payload['config'])
os.chdir(payload['workspace'])
os.execv(payload['cli'],[payload['cli'],'run','--pure','--format','json','--model',payload['config']['model'],'--dir',payload['workspace'],payload['prompt']])
"""
            self.report["ai_executed"] = True
            case["ai_executed"] = True
            self.save()
            result = self.execute(cid, "python3", "-c", agent_runner,
                                  stdin=json.dumps({"config": config, "workspace": workspace, "cli": cli, "prompt": user_prompt}),
                                  timeout=self.args.agent_timeout, check=False)
            case["trajectory"] = self.artifact(case, "agent-events.jsonl", result.stdout)
            case["agent_stderr"] = self.artifact(case, "agent-stderr.log", result.stderr)
            events = []
            for line in result.stdout.splitlines():
                if line.strip():
                    try:
                        event = json.loads(line)
                    except ValueError as error:
                        raise Failed("OpenCode stdout contains a non-JSON event") from error
                    if isinstance(event, dict):
                        events.append(event)
            evidence = tool_evidence(events)
            self.record(case, "agent_completed", result.returncode == 0 and evidence["terminal"] and evidence["tools"] > 0
                        and not any(event.get("type") == "error" for event in events), {"exit_code": result.returncode, "evidence": evidence})
            self.record(case, "guide_and_reference_read", bool(evidence["guide"] and evidence["reference"]), evidence)
            self.record(case, "source_read", bool(evidence["source"]), evidence["source"])
            after = self.snapshot(cid, workspace)
            case["after_snapshot"] = self.artifact(case, "after-agent.json", after)
            diff = self.execute(cid, "git", "-C", workspace, "diff", "--no-ext-diff", "--binary", "HEAD").stdout
            untracked_code = """import pathlib,subprocess,sys
root=pathlib.Path(sys.argv[1]); names=subprocess.check_output(['git','-C',str(root),'ls-files','--others','--exclude-standard','-z']).decode().split(chr(0))
for name in filter(None,names):
 p=root/name
 if p.is_file() and not p.is_symlink():
  result=subprocess.run(['git','diff','--no-ext-diff','--no-index','--binary','--','/dev/null',str(p)],capture_output=True,text=True)
  if result.returncode not in (0,1): raise RuntimeError('cannot record added source file')
  print(result.stdout,end='')
"""
            diff += self.execute(cid, "python3", "-c", untracked_code, workspace).stdout
            status = self.execute(cid, "git", "-C", workspace, "status", "--short").stdout
            self.artifact(case, "agent.diff", diff)
            self.artifact(case, "agent-status.txt", status)
            self.record(case, "project_changed", before != after and "workspace.manifest.toml" in after, status)
            current_head = self.execute(cid, "git", "-C", workspace, "rev-parse", "HEAD").stdout.strip()
            self.record(case, "original_source_and_data_preserved", original_head == current_head and self.preserved(before, after, movable))
            exit_code, validation = self.validate(cid, workspace)
            self.artifact(case, "validate-dev.json", validation)
            actual_services = sorted(item.get("service_id", "") for item in validation.get("services", []))
            self.record(case, "root_validate", exit_code == 0 and validation.get("valid") is True
                        and validation.get("report_version") == 1 and validation.get("scope") == "configuration"
                        and validation.get("profile") == "dev" and validation.get("diagnostics") == []
                        and validation.get("skipped_checks") == []
                        and validation.get("services_complete") is True and validation.get("topology_checked") is True
                        and actual_services == expected_services, {"exit_code": exit_code, "services": actual_services, "report": validation})
            build = self.execute(cid, "/usr/local/bin/app-cli", "build", "--workspace", workspace,
                                 "--deploy-dir", "/run/verification/production-build", timeout=self.args.runtime_timeout, check=False)
            self.artifact(case, "production-build.log", build.stdout + "\n" + build.stderr)
            self.record(case, "production_build", build.returncode == 0, {"exit_code": build.returncode})
            owner_ready = False
            try:
                owner = json.loads(self.http(cid, "/v1/runtime/identity", port=3010, timeout=2)["body"]).get("data", {})
                owner_ready = owner.get("application_id") == app and owner.get("source_root") == workspace
                if owner and not owner_ready:
                    raise Failed("agent left an owner outside the expected application source root")
            except Failed as error:
                if str(error).startswith("agent left"):
                    raise
            if not owner_ready:
                self.spawn(cid, workspace, "/run/verification/owner.log", ["/usr/local/bin/app-cli", "serve", "--control-only", "--workspace", workspace, "--log-dir", "/run/verification/runtime-logs", "--admin-addr", "127.0.0.1:3010"])
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    try:
                        owner = json.loads(self.http(cid, "/v1/runtime/identity", port=3010, timeout=2)["body"]).get("data", {})
                        if owner.get("application_id") == app and owner.get("source_root") == workspace:
                            break
                    except (Failed, ValueError):
                        pass
                    time.sleep(0.2)
                else:
                    raise Failed("current app-cli management owner did not initialize")
            admission = json.loads(self.http(cid, "/api/v1/userapp/dev/restart", method="POST", body={"app_id": app}, timeout=self.args.runtime_timeout)["body"])
            task_id = (admission.get("data") or {}).get("task_id")
            self.artifact(case, "runtime-admission.json", admission)
            if not admission.get("success") or not task_id:
                raise Failed("real dev/restart was not admitted")
            case["runtime_task_id"] = task_id
            deadline = time.monotonic() + self.args.runtime_timeout
            snapshot = {}
            while time.monotonic() < deadline:
                snapshot = json.loads(self.http(cid, f"/api/v1/userapp/tasks/{task_id}?app_id={app}")["body"])
                state = (snapshot.get("data") or {}).get("status")
                if state in {"completed", "failed", "cancelled"}:
                    break
                time.sleep(0.3)
            self.artifact(case, "runtime-task.json", snapshot)
            stream = self.http(cid, f"/api/v1/userapp/tasks/{task_id}/logs/stream?app_id={app}")
            self.artifact(case, "runtime-task.sse", stream["body"])
            runtime_events = sse_events(stream["body"])
            starts = {event["data"].get("service") for event in runtime_events if event["event"] == "service_start_ok"}
            logs = {event["data"].get("service") for event in runtime_events if event["event"] == "log" and "启动成功" in event["data"].get("line", "")}
            self.record(case, "dev_task_completed", (snapshot.get("data") or {}).get("status") == "completed"
                        and set(expected_services).issubset(starts) and set(expected_services).issubset(logs),
                        {"service_start_ok": sorted(starts), "startup_log_services": sorted(logs)})
            html = self.http(cid, "/", port=9080)
            api = self.http(cid, "/api/message", port=9080)
            self.artifact(case, "http-html.json", html)
            self.artifact(case, "http-api.json", api)
            self.record(case, "real_html", html["status"] == 200 and "text/html" in html["content_type"] and marker in html["body"])
            self.record(case, "real_api", api["status"] == 200 and json.loads(api["body"]).get("message") == marker)
            before_stop_identity = json.loads(self.http(cid, "/v1/runtime/identity", port=3010)["body"]).get("data", {})
            stopped = json.loads(self.http(cid, "/api/v1/userapp/dev/stop", method="POST", body={"app_id": app}, timeout=self.args.runtime_timeout)["body"])
            unavailable = False
            stop_observations = []
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                observation = self.tcp_probe(cid)
                stop_observations.append(observation)
                if connection_refused(observation):
                    unavailable = True
                    break
                if observation.get("errno") != 0:
                    raise Failed(f"Stop socket observation is inconclusive: {observation.get('errno_name')}")
                time.sleep(0.2)
            case["stop_socket_observations"] = self.artifact(case, "stop-socket-observations.json", stop_observations)
            still_running = self.docker("inspect", "--format", "{{.State.Running}}", cid).stdout.strip() == "true"
            after_stop_identity = json.loads(self.http(cid, "/v1/runtime/identity", port=3010)["body"]).get("data", {})
            after_stop_status = self.runtime_status(cid, f"{workspace}/state/{app}")
            self.record(case, "controlled_stop", stopped.get("success") is True and unavailable and still_running
                        and after_stop_identity.get("runtime_instance_id") == before_stop_identity.get("runtime_instance_id")
                        and after_stop_identity.get("source_root") == workspace
                        and (after_stop_status.get("data") or {}).get("desired") == "stopped",
                        {"response": stopped, "http_unavailable": unavailable, "container_running": still_running,
                         "socket_observations": stop_observations, "management_identity": after_stop_identity,
                         "management_status": after_stop_status})
            self.record(case, "data_preserved_after_runtime", self.preserved(before, self.snapshot(cid, workspace), movable))
        except (Exception, KeyboardInterrupt) as error:
            case["error"] = self.redactor.text(str(error))
            print(f"{name}: FAILED: {case['error']}", flush=True)
            if cid:
                logs = self.execute(cid, "python3", "-c", "from pathlib import Path; [print(str(p),p.read_text(errors='replace')[-12000:]) for p in [Path('/run/verification/proxy.log'),Path('/run/verification/owner.log')] if p.is_file()]", check=False)
                self.artifact(case, "failure-runtime.log", logs.stdout + logs.stderr)
            if isinstance(error, KeyboardInterrupt):
                self.report["interrupted"] = True
        finally:
            if cid:
                try:
                    identity = json.loads(self.docker("inspect", "--format", '{{json .Config.Labels}}', cid).stdout)
                    if identity.get("rcoder.repair.run") != self.run_id:
                        raise Failed("refusing cleanup: captured container ownership changed")
                    removed = self.docker("rm", "-f", cid, check=False)
                    self.record(case, "container_removed", removed.returncode == 0, "persistent fixture volume retained")
                except Exception as error:
                    case["cleanup_error"] = self.redactor.text(str(error))
            checks = {item["name"] for item in case["checks"] if item["ok"]}
            case["success"] = not case.get("error") and not case.get("cleanup_error") and REQUIRED_CHECKS.issubset(checks)
            self.save()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", default="dev-rcoder-agent-runner:latest")
    parser.add_argument("--app-cli", type=Path, required=True)
    parser.add_argument("--file-server-proxy", type=Path, required=True)
    parser.add_argument("--template-root", type=Path, default=REPO.parent / "userapp-workspace-template")
    parser.add_argument("--env-file", type=Path, default=REPO / ".env.local")
    parser.add_argument("--model", default="")
    parser.add_argument("--case", action="append", choices=CASES, help="defaults to both original fixtures")
    parser.add_argument("--preflight-only", action="store_true", help="check owned fixtures/tooling/failed tasks without any LLM request; never reports AI success")
    parser.add_argument("--agent-timeout", type=int, default=900)
    parser.add_argument("--runtime-timeout", type=int, default=180)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    for binary in (args.app_cli, args.file_server_proxy):
        if not binary.is_file() or binary.read_bytes()[:4] != b"\x7fELF":
            parser.error(f"a current Linux ELF binary is required: {binary}")
    if args.report.exists() or args.agent_timeout <= 0 or args.runtime_timeout <= 0:
        parser.error("use a new report path and positive timeout values")
    if args.case and len(args.case) != len(set(args.case)):
        parser.error("duplicate case selection")
    values = dotenv(args.env_file)
    model = args.model or values["LLM_MODEL"]
    if not re.fullmatch(r"[A-Za-z0-9_.:/-]{1,160}", model):
        parser.error("invalid model identifier")
    endpoint = urllib.parse.urlsplit(values["LLM_BASE_URL"])
    if endpoint.scheme not in {"http", "https"} or not endpoint.hostname:
        parser.error("LLM_BASE_URL must be an HTTP(S) endpoint")
    image = subprocess.run(["docker", "image", "inspect", "--format", "{{.Id}}", args.image], capture_output=True, text=True, check=True).stdout.strip()
    harness = Harness(args, values, image)
    harness.save()
    for name in harness.report["planned"]:
        harness.run_case(name)
        if harness.report.get("interrupted"):
            break
    harness.report["success"] = len(harness.report["cases"]) == len(harness.report["planned"]) and all(case["success"] for case in harness.report["cases"])
    if args.preflight_only:
        harness.report["preflight_passed"] = len(harness.report["cases"]) == len(harness.report["planned"]) and all(case.get("preflight_passed") and not case.get("error") and not case.get("cleanup_error") for case in harness.report["cases"])
    harness.save()
    print(f"Report: {args.report.resolve()}; real AI success={harness.report['success']}")
    if args.preflight_only:
        return 0 if harness.report["preflight_passed"] else 1
    return 0 if harness.report["success"] else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Failed as error:
        print(str(error), file=sys.stderr)
        raise SystemExit(2)
