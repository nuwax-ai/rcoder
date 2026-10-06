#!/usr/bin/env node
"use strict";
// Real explicit-PG Source run -> private durable input -> container replacement,
// historical redaction -> current input recovery -> edited Source restart.
// Migrations use a private real PG; no synthetic migration completion receipts.
const fs = require("node:fs");
const path = require("node:path");
const assert = require("node:assert/strict");
const { randomUUID } = require("node:crypto");
const { UserappFixture } = require("./userapp-container-fixture.cjs");


// Probes execute in the actual fixture container. Their output contains only
// identity, permissions and comparison results; the password stays internal.
const DURABLE_INPUT_PROBE = String.raw`import json,os,stat,sys
from pathlib import Path
root=Path(sys.argv[1]);path=root/'.deploy-operation.json'
record=json.loads(path.read_text());request=record['request'];active=record['active']
operation_id=record['operation']['operation_id'];operation_path=root/'operations'/(operation_id+'.json')
operation=json.loads(operation_path.read_text());expected={'username':os.environ['POSTGRES_USER'],'password':os.environ['POSTGRES_PASSWORD']}
print(json.dumps({'operation_id':operation_id,'artifact_id':active['artifact_release_id'],
 'execution_target':active['request']['execution_target'],
 'deployment_input_matches':request.get('run_pg')==expected and active['request'].get('run_pg')==expected,
 'operation_input_matches':operation['request'].get('run_config',{}).get('pg')==expected,
 'operation_identity_matches':operation['request'].get('operation_id')==operation_id and operation['view'].get('operation_id')==operation_id,
 'operation_succeeded':operation['view'].get('state')=='succeeded',
 'deployment_mode':stat.S_IMODE(path.stat().st_mode),'operation_mode':stat.S_IMODE(operation_path.stat().st_mode),
 'deployment_uid':path.stat().st_uid,'operation_uid':operation_path.stat().st_uid}))
`;
const PUBLIC_INPUT_PROBE = String.raw`import json,os,sys,urllib.error,urllib.request
from pathlib import Path
root=Path(sys.argv[1]);task=sys.argv[2]
operation_id=json.loads((root/'.deploy-operation.json').read_text())['operation']['operation_id']
secrets=(os.environ['POSTGRES_PASSWORD'],'incorrect-fixture-input');token=os.environ['APP_CLI_DEPLOY_TOKEN'];results=[]
routes=[(3010,'/v1/runtime/status',True),(3010,'/v1/runtime/recovery',True),
 (3010,'/v1/runtime/recovery',False),(3010,'/v1/runtime/operations/'+operation_id,True),
 (3010,'/v1/runtime/operations/'+operation_id+'/events',True),
 (3010,'/v1/runtime/operations/'+operation_id+'/events/stream',True),(3010,'/v1/deploy/status',True)]
if task:
 routes.extend([(60000,'/api/v1/userapp/tasks/'+task+'?app_id='+sys.argv[3],True),
  (60000,'/api/v1/userapp/tasks/'+task+'/logs/stream?app_id='+sys.argv[3]+'&from_seq=0',True)])
for port,route,authenticated in routes:
 request=urllib.request.Request('http://127.0.0.1:'+str(port)+route,
  headers={'X-Deploy-Token':token} if authenticated else {})
 try:
  with urllib.request.urlopen(request,timeout=15) as response: status=response.status;body=response.read().decode()
 except urllib.error.HTTPError as error: status=error.code;body=error.read().decode()
 results.append({'route':route,'authenticated':authenticated,'status':status,
  'password_absent':all(secret not in body and json.dumps(secret)[1:-1] not in body for secret in secrets),
  'internal_input_absent':'"run_pg"' not in body and '"run_config"' not in body})
print(json.dumps(results))
`;
const LEGACY_REDACTION = String.raw`import hashlib,json,sys
from pathlib import Path
path=Path(sys.argv[1])/'.deploy-operation.json';record=json.loads(path.read_text())
for request in (record['request'],record['active']['request']): request['run_pg']['password']=''
path.write_text(json.dumps(record));path.chmod(0o600)
history=path.with_name('.deploy-operation.legacy-redacted-fixture')
history.write_bytes(path.read_bytes());history.chmod(0o600)
operation_path=path.parent/'operations'/(record['operation']['operation_id']+'.json')
operation=json.loads(operation_path.read_text());operation['request']['run_config']['pg']['password']=''
operation_path.write_text(json.dumps(operation));operation_path.chmod(0o600)
operation_history=operation_path.with_suffix('.legacy-redacted-fixture')
operation_history.write_bytes(operation_path.read_bytes());operation_history.chmod(0o600)
print(json.dumps({'path':str(path),'history_path':str(history),'sha256':hashlib.sha256(path.read_bytes()).hexdigest(),
 'operation_path':str(operation_path),'operation_history_path':str(operation_history),
 'operation_sha256':hashlib.sha256(operation_path.read_bytes()).hexdigest(),
 'artifact_id':record['active']['artifact_release_id']}))
`;
const AUTHENTICATED_MAIN = String.raw`import os,subprocess
_pg_env=dict(os.environ,PGPASSWORD=os.environ["POSTGRES_PASSWORD"],PGCONNECT_TIMEOUT="3")
subprocess.run(["psql","-X","-w","-h","127.0.0.1","-U",os.environ["POSTGRES_USER"],"-d",os.environ["POSTGRES_DB"],"-qAt","-c","SELECT 1"],env=_pg_env,capture_output=True,check=True)
`;

function durableInputIsValid(evidence) {
  return Boolean(evidence.operation_id && evidence.artifact_id
    && evidence.execution_target === "source" && evidence.deployment_input_matches
    && evidence.operation_input_matches && evidence.operation_identity_matches
    && evidence.operation_succeeded && evidence.deployment_mode === 0o600
    && evidence.operation_mode === 0o600 && evidence.deployment_uid === 0
    && evidence.operation_uid === 0);
}
function publicInputIsPrivate(evidence) {
  const required = [
    ["/v1/runtime/status", true], ["/v1/runtime/recovery", true],
    ["/v1/runtime/recovery", false], ["/v1/deploy/status", true],
    [/^\/v1\/runtime\/operations\/[^/]+$/, true],
    [/^\/v1\/runtime\/operations\/[^/]+\/events$/, true],
    [/^\/v1\/runtime\/operations\/[^/]+\/events\/stream$/, true],
    [/^\/api\/v1\/userapp\/tasks\/[^/]+\?app_id=/, true],
    [/^\/api\/v1\/userapp\/tasks\/[^/]+\/logs\/stream\?app_id=/, true],
  ];
  return required.every(([route, authenticated]) => evidence.some(row => row.authenticated === authenticated
    && (typeof route === "string" ? row.route === route : route.test(row.route))))
    && evidence.every(row => row.password_absent && row.internal_input_absent
      && row.status === (row.authenticated ? 200 : 403));
}

async function main() {
  const fixture = new UserappFixture("source-credentials", process.argv[2], process.argv[3], process.argv[4], { postgresPassword: "private-source-fixture-" + randomUUID() });
  let lastTask;
  const ordinaryMarker = fixture.marker.bind(fixture);
  fixture.marker = marker => {
    ordinaryMarker(marker);
    const file = fixture.workspace + "/web/main.py";
    fixture.write({ [file]: AUTHENTICATED_MAIN + fixture.read(file) });
  };
  const ordinaryRequest = fixture.request.bind(fixture);
  fixture.request = (route, data, port) => {
    if (["/api/v1/userapp/dev/start", "/api/v1/userapp/dev/restart"].includes(route)) {
      // Read the private input inside the container. Never put the password in
      // Node stdout, a shell string, curl argv or the public evidence report.
      // Private operations and deployment files intentionally retain the input.
      const code = 'import json,os,sys,urllib.request\nb={"app_id":sys.argv[1],"pg":{"username":os.environ["POSTGRES_USER"],"password":os.environ["POSTGRES_PASSWORD"] if sys.argv[3]=="valid" else "incorrect-fixture-input"}}\nr=urllib.request.Request("http://127.0.0.1:60000"+sys.argv[2],data=json.dumps(b).encode(),headers={"content-type":"application/json"})\nprint(urllib.request.urlopen(r,timeout=30).read().decode())\n';
      const body = JSON.parse(fixture.exec(["python3", "-c", code, fixture.app, route, data?.fixture_wrong_pg ? "invalid" : "valid"]).stdout);
      assert(body.success, body.message);
      lastTask = body.data.task_id;
      return body.data;
    }
    return ordinaryRequest(route, data, port);
  };
  const migrationCount = () => fixture.exec(["sh", "-ec", 'PGPASSWORD="$POSTGRES_PASSWORD" PGCONNECT_TIMEOUT=2 psql -X -w -h 127.0.0.1 -U "$POSTGRES_USER" -d "$POSTGRES_DB" -qAt -c "SELECT runs FROM source_recovery_check WHERE id=1"']).stdout.trim();
  const expectCredentialFailure = async () => {
    const runsBefore = migrationCount();
    const accepted = fixture.request("/api/v1/userapp/dev/restart", { fixture_wrong_pg: true });
    assert(accepted.task_id, "the rejected credential attempt must have a real task");
    const deadline = Date.now() + 150000;
    let task;
    while (Date.now() < deadline) {
      task = fixture.request(`/api/v1/userapp/tasks/${accepted.task_id}?app_id=${fixture.app}`);
      if (["failed", "cancelled", "completed"].includes(task.status)) break;
      await new Promise(resolve => setTimeout(resolve, 200));
    }
    fixture.check("wrong current credentials fail without starting business", task?.status === "failed" && fixture.content().status === 7, { task_id: accepted.task_id });
    const text = fixture.exec(["curl", "-fsS", "--max-time", "10", `http://127.0.0.1:60000/api/v1/userapp/tasks/${accepted.task_id}/logs/stream?app_id=${fixture.app}&from_seq=0`]).stdout;
    const events = text.split("\n").filter(line => line.startsWith("data: ")).map(line => JSON.parse(line.slice(6)));
    const privacy = publicInput(accepted.task_id);
    fixture.check("rejected credential task and its public SSE keep both actual and invalid input private", publicInputIsPrivate(privacy), privacy);
    fixture.report.current_credential_failure = { task, events };
    fixture.check("credential failure is visible as log before unique failed terminal", events.some(event => event.event === "log" && /PostgreSQL|postgres|database|数据库|authentication/i.test(event.line)) && events.at(-1)?.event === "failed" && events.filter(event => event.event === "failed").length === 1);
    fixture.check("failed credentials do not execute migrations", migrationCount() === runsBefore);
  };
  const recovery = () => {
    const code = 'import os,json,urllib.request\nr=urllib.request.Request("http://127.0.0.1:3010/v1/runtime/recovery",headers={"X-Deploy-Token":os.environ["APP_CLI_DEPLOY_TOKEN"]})\nprint(urllib.request.urlopen(r,timeout=5).read().decode())\n';
    const body = JSON.parse(fixture.exec(["python3", "-c", code]).stdout);
    assert(body.success, body.message);
    return body.data;
  };
  const durableInput = () => JSON.parse(fixture.exec(["python3", "-c", DURABLE_INPUT_PROBE, fixture.state]).stdout);
  const publicInput = (task = lastTask) => JSON.parse(fixture.exec(["python3", "-c", PUBLIC_INPUT_PROBE, fixture.state, task || "", fixture.app]).stdout);
  const waitUntil = async (label, observe, timeout = 150000) => {
    const deadline = Date.now() + timeout;
    while (Date.now() < deadline) {
      try { if (observe()) return; } catch { /* retain the real final failure below */ }
      await new Promise(resolve => setTimeout(resolve, 200));
    }
    throw new Error(label + " did not recover within its budget");
  };
  const configureNormalOwner = (containerIndex, unsetPg) => {
    const conf = path.join(fixture.host, "conf-" + containerIndex, "fixture.conf");
    const content = fs.readFileSync(conf, "utf8");
    assert(content.includes("serve --control-only --workspace") || content.includes("serve --workspace"));
    const normal = content.replace("serve --control-only --workspace", "serve --workspace")
      .replace("env -u POSTGRES_USER -u POSTGRES_PASSWORD ", "");
    fs.writeFileSync(conf, unsetPg ? normal.replace("command=/usr/local/bin/app-cli ",
      "command=env -u POSTGRES_USER -u POSTGRES_PASSWORD /usr/local/bin/app-cli ") : normal);
    fixture.exec(["supervisorctl", "reread"]);
    fixture.exec(["supervisorctl", "update", "app-cli"]);
  };
  let error;
  try {
    await fixture.create();
    fixture.prepare("explicit-before-replacement");
    const manifest = fixture.workspace + "/web/project.manifest.toml";
    fixture.write({
      [manifest]: fixture.read(manifest).replace("[run]\n", '[run]\nmigrate=["python3","migrate.py"]\n'),
      [fixture.workspace + "/web/migrate.py"]: 'import os,subprocess\nenv=dict(os.environ,PGPASSWORD=os.environ["POSTGRES_PASSWORD"])\nsubprocess.run(["psql","-X","-w","-h","127.0.0.1","-U",os.environ["POSTGRES_USER"],"-d",os.environ["POSTGRES_DB"],"-v","ON_ERROR_STOP=1","-f","migrate.sql"],env=env,check=True)\n',
      [fixture.workspace + "/web/migrate.sql"]: 'CREATE TABLE IF NOT EXISTS source_recovery_check(id integer PRIMARY KEY,runs integer NOT NULL);\nINSERT INTO source_recovery_check VALUES(1,1) ON CONFLICT(id) DO UPDATE SET runs=source_recovery_check.runs+1;\n'
    });
    await fixture.boot();
    await fixture.start("start", "explicit-before-replacement");
    fixture.check("initial migration really commits to PostgreSQL once", migrationCount() === "1");
    const oldContainer = fixture.cid;
    const old = durableInput();
    fixture.check("private Source operation and deployment retain real PG at 0600", durableInputIsValid(old), old);
    const oldPublic = publicInput();
    fixture.check("authenticated and rejected public API and task SSE omit private PG", publicInputIsPrivate(oldPublic), oldPublic);
    const oldArtifact = old.artifact_id;
    const oldOwner = ordinaryRequest("/v1/runtime/identity", undefined, 3010).runtime_instance_id;
    fixture.removeCurrent();
    await fixture.create();
    fixture.check("new container keeps the original volume and data", fixture.cid !== oldContainer && fixture.read(fixture.workspace + "/sentinel") === fixture.id);
    // The replacement owner inherits neither PG user nor PG password. The
    // business's real TCP SELECT 1 must use the durable prior operation input.
    configureNormalOwner(1, true);
    await fixture.boot();
    await waitUntil("automatic business using durable input", () => fixture.content().stdout === "explicit-before-replacement");
    const envProof = JSON.parse(fixture.exec(["python3", "-c", String.raw`import json,sys
from pathlib import Path
root=Path(sys.argv[1]);discovery=json.loads((root/'supervisor.json').read_text())
generation=discovery['snapshot']['generation'];record=json.loads((root/'work'/generation/'generation.json').read_text());pid=record['worker_pid']
argv=(Path('/proc')/str(pid)/'cmdline').read_bytes().split(bytes([0]))
assert len(argv)>2 and Path(argv[0].decode()).name=='app-cli' and argv[1]==b'serve'
keys={value.split(b'=',1)[0] for value in (Path('/proc')/str(pid)/'environ').read_bytes().split(bytes([0])) if value}
print(json.dumps({'owner_pid':pid,'generation':generation,'pg_environment_absent':b'POSTGRES_USER' not in keys and b'POSTGRES_PASSWORD' not in keys}))
`, fixture.state]).stdout);
    fixture.check("automatic restore owner truly has no inherited PG pair", envProof.pg_environment_absent, envProof);
    fixture.check("new owner automatically restores durable PG and real authenticated HTTP", fixture.content().stdout === "explicit-before-replacement"
      && ordinaryRequest("/v1/runtime/identity", undefined, 3010).runtime_instance_id !== oldOwner
      && !recovery().credentials_required && !recovery().owner_protected && migrationCount() === "1");
    fixture.check("automatic durable recovery preserves the confirmed input and SQL identity", durableInputIsValid(durableInput()) && durableInput().artifact_id === oldArtifact, durableInput());

    // Import only a historical serializer's redaction while the owner is down.
    // The old bytes remain diagnostic; valid inherited input may restore real
    // business without a permanent credentials hold or invented backfill.
    fixture.exec(["supervisorctl", "stop", "app-cli"]);
    const stopped = fixture.exec(["sh", "-ec", 'pgrep -f "[a]pp-cli serve" | wc -l']).stdout.trim();
    fixture.check("owner is physically stopped before importing historical redaction", stopped === "0", { serve_processes: stopped });
    const redacted = JSON.parse(fixture.exec(["python3", "-c", LEGACY_REDACTION, fixture.state]).stdout);
    fixture.report.legacy_redaction = redacted;
    configureNormalOwner(1, false);
    fixture.exec(["supervisorctl", "start", "app-cli"]);
    await waitUntil("legacy redacted business using valid current environment", () => fixture.content().stdout === "explicit-before-replacement");
    const legacyRecovery = recovery();
    fixture.check("historical redaction does not create a permanent credentials hold", !legacyRecovery.owner_protected && !legacyRecovery.credentials_required && !legacyRecovery.kernel_protected, legacyRecovery);
    const legacyInput = JSON.parse(fixture.read(redacted.path));
    const legacyOperation = JSON.parse(fixture.read(redacted.operation_path));
    fixture.check("legacy redacted input is not backfilled and its original bytes remain diagnostic", fixture.exec(["sha256sum", redacted.history_path]).stdout.startsWith(redacted.sha256)
      && fixture.exec(["sha256sum", redacted.operation_history_path]).stdout.startsWith(redacted.operation_sha256)
      && legacyInput.request.run_pg.password === "" && legacyInput.active.request.run_pg.password === ""
      && legacyOperation.request.run_config.pg.password === ""
      && fixture.content().stdout === "explicit-before-replacement" && migrationCount() === "1", redacted);
    await fixture.stop();
    fixture.marker("explicit-after-replacement");
    const previous = fixture.read(manifest);
    assert(previous.includes('name="Quality runtime"'));
    fixture.write({ [manifest]: previous.replace('name="Quality runtime"', 'name="Edited current Source"') });
    await expectCredentialFailure();
    await fixture.start("restart", "explicit-after-replacement");
    const current = durableInput();
    fixture.check("new Source replaces an older content identity", current.artifact_id !== oldArtifact && current.execution_target === "source", current);
    fixture.check("new explicit Source keeps real private input in both 0600 records", durableInputIsValid(current), current);
    const currentPublic = publicInput();
    fixture.check("new operation HTTP and task SSE do not expose private input", publicInputIsPrivate(currentPublic), currentPublic);
    fixture.check("recovery protection is gone after real HTTP", !recovery().owner_protected);
    fixture.check("a new release executes its own migration once", migrationCount() === "2");
    await fixture.stop();
    await fixture.start("start", "explicit-after-replacement");
    fixture.check("Stop and Start do not replay already confirmed SQL", migrationCount() === "2");
    fixture.check("workspace sentinel survives every request", fixture.read(fixture.workspace + "/sentinel") === fixture.id);
    const liveContainer = fixture.cid;
    const killedOwner = ordinaryRequest("/v1/runtime/identity", undefined, 3010).runtime_instance_id;
    const buildCount = fixture.read(fixture.workspace + "/web/builds.log").trim().split("\n").length;
    // All matching processes are confined to this uniquely owned container.
    // Keep file-server alive so the next request exercises its real bootstrap.
    fixture.exec(["pkill", "-9", "-x", "app-cli"]);
    await fixture.start("restart", "explicit-after-replacement");
    fixture.check("pkill retry really builds current Source again", fixture.read(fixture.workspace + "/web/builds.log").trim().split("\n").length === buildCount + 1);
    fixture.check("pkill recovery uses a new owner in the same container", fixture.cid === liveContainer && ordinaryRequest("/v1/runtime/identity", undefined, 3010).runtime_instance_id !== killedOwner);
    fixture.check("pkill recovery keeps data and confirmed SQL", migrationCount() === "2" && fixture.read(fixture.workspace + "/sentinel") === fixture.id);
    await fixture.stop();
  } catch (failure) {
    error = failure;
    try {
    const diagnostics = fixture.exec(["python3", "-c", String.raw`import json,os,sys
from pathlib import Path
roots=[Path(sys.argv[1]),Path('/home/user/logs')]
paths={Path('/home/user/owner.log'),Path('/home/user/proxy.log')}
for root in roots:
 if root.exists(): paths.update(root.rglob('runtime.err.log'));paths.update(root.rglob('runtime.out.log'))
secrets=[value for key,value in os.environ.items() if value and any(part in key.lower() for part in ('password','passwd','token','secret','api_key'))]
secrets.append('incorrect-fixture-input');logs=[]
for path in sorted(paths):
 if not path.is_file():continue
 text=path.read_text(errors='replace')[-16000:]
 for secret in secrets:text=text.replace(secret,'<private-input>')
 logs.append({'path':str(path),'text':text})
print(json.dumps(logs))
`, fixture.workspace], false);
    fixture.report.failure_diagnostics = { returncode: diagnostics.status };
    try { fixture.report.failure_diagnostics.logs = JSON.parse(diagnostics.stdout); }
    catch { fixture.report.failure_diagnostics.capture_failed = true; }
    } catch {
      fixture.report.failure_diagnostics = { capture_failed: true };
    }
  }
  await fixture.finish(error);
  console.log("SOURCE_CREDENTIAL_RECOVERY_OK");
}
module.exports = { DURABLE_INPUT_PROBE, PUBLIC_INPUT_PROBE, LEGACY_REDACTION, AUTHENTICATED_MAIN, durableInputIsValid, publicInputIsPrivate };
if (require.main === module) {
  main().catch(error => { console.error("SOURCE_CREDENTIAL_RECOVERY_FAIL:", error.message); process.exitCode = 1; });
}
