"use strict";
// Shared local-only fixture for the F4/F6 runtime contracts. Missing tools,
// source/binary receipts, or failed product operations are failures, not skips.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const os = require("node:os");
const { randomUUID, createHash } = require("node:crypto");
const { spawnSync } = require("node:child_process");
const repo = path.resolve(__dirname, "../../../..");
const sha256 = file => createHash("sha256").update(fs.readFileSync(file)).digest("hex");

function command(argv, timeout = 180000, requireSuccess = true) {
  const out = spawnSync(argv[0], argv.slice(1), { encoding: "utf8", timeout, maxBuffer: 4 * 1024 * 1024 });
  if (out.error || (requireSuccess && out.status !== 0)) {
    throw new Error(`${argv[0]} ${argv[1] || ""} failed: ${out.error?.message || out.stderr.slice(-1800)}`);
  }
  return out;
}

function currentSource() {
  return JSON.parse(command(["python3", "-c", "import sys,json;sys.path.insert(0,sys.argv[1]);from userapp_root_logs import source_snapshot;from pathlib import Path;print(json.dumps(source_snapshot(Path(sys.argv[2]))))", path.join(repo, "tests-e2e/tools"), repo]).stdout);
}

function verifyBuild(binaries) {
  const receiptPath = process.env.USERAPP_E2E_BUILD_SOURCE;
  assert(receiptPath, "USERAPP_E2E_BUILD_SOURCE must identify the build source and binary SHA256 values");
  const source = currentSource();
  const receipt = JSON.parse(fs.readFileSync(receiptPath, "utf8"));
  assert.equal(receipt.source_inputs_sha256, source.source_inputs_sha256, "binary build source differs from current checkout");
  for (const [name, file] of Object.entries(binaries)) {
    assert.equal(sha256(file), receipt.binaries?.[name], `${name} differs from its build receipt`);
  }
  return source;
}

async function waitFor(label, probe, budget = 90000) {
  const deadline = Date.now() + budget;
  let last;
  while (Date.now() < deadline) {
    try { if (await probe()) return; } catch (error) {
      if (error.taskFailed) throw error;
      last = error;
    }
    await new Promise(resolve => setTimeout(resolve, 200));
  }
  throw new Error(`${label} timed out: ${last?.message || "condition not observed"}`);
}

class UserappFixture {
  constructor(kind, appCli, proxy, image = "dev-rcoder-agent-runner:latest", options = {}) {
    assert(appCli && proxy, "app-cli and file-server-proxy Linux binaries are required");
    this.binaries = { "app-cli": path.resolve(appCli), "file-server-proxy": path.resolve(proxy) };
    this.source = verifyBuild(this.binaries);
    const endpoint = process.env.DOCKER_HOST || command(["docker", "context", "inspect", ...(process.env.DOCKER_CONTEXT ? [process.env.DOCKER_CONTEXT] : []), "--format", '{{(index .Endpoints "docker").Host}}']).stdout.trim();
    assert(/^(unix|npipe):/.test(endpoint), "this fixture only operates a local Docker engine");
    const inspected = JSON.parse(command(["docker", "image", "inspect", image]).stdout)[0];
    const machine = { amd64: 62, arm64: 183 }[inspected.Architecture];
    assert(machine, "fixture image architecture is unsupported");
    for (const file of Object.values(this.binaries)) {
      const header = fs.readFileSync(file).subarray(0, 20);
      assert(header.subarray(0, 4).equals(Buffer.from([0x7f, 0x45, 0x4c, 0x46])) && header[5] === 1 && header.readUInt16LE(18) === machine, "test binary must be a matching little-endian Linux ELF");
    }
    this.kind = kind;
    this.options = options;
    this.id = randomUUID();
    this.app = "e2e" + this.id.replaceAll("-", "").slice(0, 12);
    this.workspace = "/home/user/" + this.app;
    this.state = this.workspace + "/state/" + this.app;
    this.volume = "rcoder-" + kind + "-" + this.id;
    this.image = inspected.Id;
    this.host = fs.mkdtempSync(path.join(os.tmpdir(), kind + "-"));
    this.containers = [];
    this.reportPath = process.env.USERAPP_E2E_REPORT || path.join(repo, "tests-e2e/reports", kind + "-" + this.id + ".json");
    this.report = { kind, source: this.source, image: this.image, volume: this.volume,
      binaries: Object.fromEntries(Object.entries(this.binaries).map(([name, file]) => [name, { path: file, sha256: sha256(file) }])), checks: [], containers: [], success: false };
    command(["docker", "volume", "create", "--label", "rcoder.e2e.owner=" + this.id, this.volume]);
    this.createdAt = JSON.parse(command(["docker", "volume", "inspect", this.volume]).stdout)[0].CreatedAt;
  }
  docker(args, requireSuccess = true) { return command(["docker", ...args], 180000, requireSuccess); }
  exec(args, requireSuccess = true) { return this.docker(["exec", this.cid, ...args], requireSuccess); }
  write(files) {
    this.exec(["python3", "-c", "import pathlib,json,sys\nfor p,t in json.loads(sys.argv[1]).items():\n f=pathlib.Path(p);f.parent.mkdir(parents=True,exist_ok=True);f.write_text(t)\n", JSON.stringify(files)]);
  }
  read(file) { return this.exec(["cat", file]).stdout; }
  check(name, passed, evidence) {
    this.report.checks.push({ name, passed: Boolean(passed), evidence });
    assert(passed, name);
    console.log(name + " PASS");
  }
  async create() {
    const confDir = path.join(this.host, "conf-" + this.containers.length);
    const platformDir = path.join(this.host, "platform-" + this.containers.length);
    fs.mkdirSync(confDir); fs.mkdirSync(platformDir);
    const domain = { authority: "rcoder-quality-fixture", volume: this.volume, instance: randomUUID() };
    fs.writeFileSync(path.join(platformDir, "execution-domain"), JSON.stringify(domain));
    const conf = path.join(this.host, "supervisor-" + this.containers.length + ".conf");
    fs.writeFileSync(conf, `[unix_http_server]\nfile=/var/run/supervisor.sock\n[supervisord]\nnodaemon=true\nlogfile=/tmp/fixture-supervisor.log\npidfile=/var/run/supervisord.pid\n[rpcinterface:supervisor]\nsupervisor.rpcinterface_factory=supervisor.rpcinterface:make_main_rpcinterface\n[supervisorctl]\nserverurl=unix:///var/run/supervisor.sock\n[include]\nfiles=/etc/supervisor/conf.d/*.conf\n`);
    // This private container needs the same real PG program as a builder. Do
    // not skip readiness or rotate credentials to make a runtime test pass.
    fs.copyFileSync(path.join(repo, "docker/rcoder-agent-runner/supervisor/conf.d/postgres.conf"), path.join(confDir, "postgres.conf"));
    const prefix = this.kind === "f6" ? "env -u RCODER_EXECUTION_DOMAIN " : "";
    fs.writeFileSync(path.join(confDir, "fixture.conf"), `[program:app-cli]\ncommand=${prefix}/usr/local/bin/app-cli serve --control-only --workspace ${this.workspace}\ndirectory=${this.workspace}\nautostart=false\nautorestart=false\nstartsecs=0\nstopasgroup=true\nkillasgroup=true\nstopwaitsecs=3\nstdout_logfile=/home/user/owner.log\nredirect_stderr=true\n[program:file-server-proxy]\ncommand=${prefix}/usr/local/bin/file-server-proxy --embed --policy all_rust --port 60000\nautostart=false\nautorestart=false\nstartsecs=0\nstdout_logfile=/home/user/proxy.log\nredirect_stderr=true\n`);
    const env = { PROJECT_ID: this.app, APP_ID: this.app, RCODER_RUNTIME_IMAGE_DIGEST: this.image,
      SERVICE_TYPE: "user-app-builder", USERAPP_SINGLE_APP_ID: this.app,
      USERAPP_WORKSPACE_DIR: this.workspace, APP_CLI_RUNTIME_WORKSPACE: this.workspace, APP_CLI_STATE_ROOT: this.state,
      APP_CLI_MANAGED: "1", APP_CLI_REQUIRE_PG: "1", APP_CLI_DEPLOY_TOKEN: this.app + "-fixture-token", FILE_SERVER_APP_CLI_BIN: "/usr/local/bin/app-cli",
      FILE_SERVER_PROXY_STATE_DIR: this.workspace + "/proxy-state", FILE_SERVER_PROXY_PUBLIC_BIND: "true", FILE_SERVER_LOG_DIR: "/home/user/proxy-logs",
      RCODER_PLATFORM_BINDING_DIR: "/etc/rcoder/fixture-platform" };
    if (this.kind === "f4") env.RCODER_EXECUTION_DOMAIN = JSON.stringify(domain);
    const privateEnv = [];
    if (this.options.postgresPassword) {
      // Configure the fixture's database at first init, without exposing its
      // private password in command argv or the public evidence report.
      assert(!/[\r\n\0]/.test(this.options.postgresPassword));
      const privateEnvFile = path.join(this.host, "postgres-" + this.containers.length + ".env");
      fs.writeFileSync(privateEnvFile, "POSTGRES_PASSWORD=" + this.options.postgresPassword + "\n", { mode: 0o600 });
      privateEnv.push("--env-file", privateEnvFile);
    }
    this.cid = this.docker(["create", "--name", "rcoder-" + this.kind + "-" + randomUUID(), "--label", "rcoder.e2e.owner=" + this.id, "--user", "0",
      "--mount", `type=volume,src=${this.volume},dst=/home/user,volume-nocopy`,
      // app-cli publishes/removes 50-app-services.conf through captured engine
      // ownership. A read-only directory makes its initial drain fail forever.
      "--mount", `type=bind,src=${confDir},dst=/etc/supervisor/conf.d`,
      "--mount", `type=bind,src=${conf},dst=/etc/supervisor/supervisord.conf,readonly`,
      "--mount", `type=bind,src=${platformDir},dst=/etc/rcoder/fixture-platform,readonly`,
      ...Object.entries(env).flatMap(([key, value]) => ["-e", key + "=" + value]),
      ...privateEnv,
      "--entrypoint", "sh", this.image, "-ec", `install -d -o postgres -g postgres "\${PGDATA:-/home/user/.pgdata}"; mkdir -p ${this.workspace} /home/user/proxy-logs /app/logs; exec supervisord -n -c /etc/supervisor/supervisord.conf`]).stdout.trim();
    this.containers.push(this.cid);
    this.report.containers.push({ id: this.cid, domain_instance: domain.instance });
    for (const [name, binary] of Object.entries(this.binaries)) this.docker(["cp", binary, this.cid + ":/usr/local/bin/" + name]);
    this.docker(["start", this.cid]);
    await waitFor("private supervisord", () => this.exec(["test", "-S", "/var/run/supervisor.sock"], false).status === 0);
    for (const tool of ["python3", "curl", "pingap"]) this.exec([tool, "--version"]);
    for (const [name] of Object.entries(this.binaries)) this.check(name + " matches container copy", this.exec(["sha256sum", "/usr/local/bin/" + name]).stdout.startsWith(this.report.binaries[name].sha256));
    await waitFor("fixture PostgreSQL TCP login", () => {
      const result = this.exec(["sh", "-ec", 'PGPASSWORD="$POSTGRES_PASSWORD" PGCONNECT_TIMEOUT=2 psql -X -w -h 127.0.0.1 -U "$POSTGRES_USER" -d "$POSTGRES_DB" -qAt -c "SELECT 1"'], false);
      return result.status === 0 && result.stdout.trim() === "1";
    });
    this.check("fixture PostgreSQL accepts the runtime credentials", true);
    return domain;
  }
  prepare(marker) {
    this.write({ [this.workspace + "/workspace.manifest.toml"]: 'schema_version=1\n[workspace]\nname="quality-runtime"\n',
      [this.workspace + "/web/project.manifest.toml"]: 'schema_version=1\n[project]\nservice_id="web"\nname="Quality runtime"\ntype="python"\n[build]\ncommand=["python3","build.py"]\nartifact="artifact.zip"\n[devbuild]\ncommand=["python3","build.py"]\n[run]\ncommand=["python3","main.py"]\n[devrun]\ncommand=["python3","main.py"]\n[health]\nreadiness_path="/"\n[proxy]\npath="/"\nstrip_prefix=false\n',
      [this.workspace + "/web/build.py"]: 'from pathlib import Path\nimport zipfile\nPath("builds.log").open("a").write("build\\n")\nwith zipfile.ZipFile("artifact.zip","w") as z:z.write("main.py")\n',
      [this.workspace + "/sentinel"]: this.id });
    this.marker(marker);
  }
  marker(marker) {
    this.write({ [this.workspace + "/web/main.py"]: `import os\nfrom http.server import BaseHTTPRequestHandler,HTTPServer\nclass H(BaseHTTPRequestHandler):\n def do_GET(self):\n  self.send_response(200)\n  self.end_headers()\n  self.wfile.write(${JSON.stringify(marker)}.encode())\nHTTPServer(("0.0.0.0",int(os.environ["PORT"])),H).serve_forever()\n` });
  }
  async boot() {
    this.exec(["supervisorctl", "start", "app-cli", "file-server-proxy"]);
    await waitFor("management identity and deploy status", () => {
      this.request("/v1/deploy/status", undefined, 3010);
      const identity = this.request("/v1/runtime/identity", undefined, 3010);
      return Boolean(identity.runtime_instance_id);
    });
    await waitFor("file-server entry", () => this.exec(["curl", "-fsS", "--max-time", "3", "http://127.0.0.1:60000/health"], false).status === 0);
  }
  request(route, data, port = 60000) {
    const args = ["curl", "-fsS", "--max-time", "30"];
    if (data !== undefined) args.push("-H", "content-type: application/json", "--data", JSON.stringify(data));
    args.push(`http://127.0.0.1:${port}${route}`);
    const result = JSON.parse(this.exec(args).stdout);
    assert(result.success === true, `product request rejected: ${result.code}: ${result.message}`);
    return result.data;
  }
  content() { return this.exec(["curl", "-fsS", "--max-time", "3", "http://127.0.0.1:9080/"], false); }
  async start(action, expected) {
    const accepted = this.request("/api/v1/userapp/dev/" + action, { app_id: this.app });
    assert(accepted.task_id, "real build task id required");
    await waitFor("build task completion", () => {
      const task = this.request(`/api/v1/userapp/tasks/${accepted.task_id}?app_id=${this.app}`);
      if (["failed", "cancelled"].includes(task.status)) {
        this.report.failed_task = task;
        throw Object.assign(new Error(`task ${task.status}: ${task.error || task.error_message || "see task logs"}`), { taskFailed: true });
      }
      return task.status === "completed";
    }, 150000);
    await waitFor("business HTTP", () => this.content().stdout === expected);
    this.check(action + " builds and serves expected content", this.content().stdout === expected, { task_id: accepted.task_id });
  }
  async stop() {
    this.request("/api/v1/userapp/dev/stop", { app_id: this.app });
    await waitFor("business port closed", () => this.content().status === 7);
    this.check("Stop keeps management available", Boolean(this.request("/v1/runtime/identity", undefined, 3010).runtime_instance_id));
  }
  removeCurrent() {
    const inspected = JSON.parse(this.docker(["inspect", this.cid]).stdout)[0];
    assert.equal(inspected.Config.Labels["rcoder.e2e.owner"], this.id, "cleanup container identity changed");
    this.docker(["rm", "-f", this.cid]);
  }
  async finish(error) {
    if (error) this.report.error = error.message;
    let cleanupError;
    for (const id of this.containers) {
      const inspected = this.docker(["inspect", id], false);
      if (inspected.status !== 0) {
        const absent = this.docker(["container", "ls", "-a", "--no-trunc", "--filter", "id=" + id, "--format", "{{.ID}}"], false);
        if (absent.status === 0 && !absent.stdout.trim()) continue;
        cleanupError = new Error("container cleanup inspection is unknown: " + id);
        continue;
      }
      try {
        assert.equal(JSON.parse(inspected.stdout)[0].Config.Labels["rcoder.e2e.owner"], this.id);
        this.docker(["rm", "-f", id]);
      } catch (failure) { cleanupError = failure; }
    }
    this.report.volume_retained = null;
    this.report.cleanup_ok = !cleanupError;
    this.report.success = !error && !cleanupError;
    try {
      assert.equal(currentSource().source_inputs_sha256, this.source.source_inputs_sha256, "source changed during fixture execution");
      const volume = JSON.parse(this.docker(["volume", "inspect", this.volume]).stdout)[0];
      assert.equal(volume.Labels["rcoder.e2e.owner"], this.id);
      assert.equal(volume.CreatedAt, this.createdAt);
      this.report.volume_retained = true;
    } catch (failure) { this.report.success = false; cleanupError ||= failure; }
    fs.mkdirSync(path.dirname(this.reportPath), { recursive: true });
    fs.writeFileSync(this.reportPath, JSON.stringify(this.report, null, 2) + "\n");
    if (!cleanupError) fs.rmSync(this.host, { recursive: true });
    if (cleanupError) throw cleanupError;
    if (error) throw error;
    console.log("report: " + this.reportPath);
  }
}

module.exports = { UserappFixture, verifyBuild };
