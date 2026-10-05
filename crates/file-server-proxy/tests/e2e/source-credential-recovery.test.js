#!/usr/bin/env node
"use strict";
// Real explicit-PG Source run -> redacted journal -> container replacement ->
// edited Source lock -> new explicit restart. Migrations use a real private PG;
// no database password rotation or synthetic completion receipts.
const fs = require("node:fs");
const path = require("node:path");
const assert = require("node:assert/strict");
const { UserappFixture } = require("./userapp-container-fixture.cjs");

async function main() {
  const fixture = new UserappFixture("source-credentials", process.argv[2], process.argv[3], process.argv[4]);
  const ordinaryRequest = fixture.request.bind(fixture);
  fixture.request = (route, data, port) => {
    if (["/api/v1/userapp/dev/start", "/api/v1/userapp/dev/restart"].includes(route)) {
      // Read the private input inside the container. Never put the password in
      // Node stdout, a shell string, curl argv, report, or the persisted journal.
      const code = 'import json,os,sys,urllib.request\nb={"app_id":sys.argv[1],"pg":{"username":os.environ["POSTGRES_USER"],"password":os.environ["POSTGRES_PASSWORD"] if sys.argv[3]=="valid" else "incorrect-fixture-input"}}\nr=urllib.request.Request("http://127.0.0.1:60000"+sys.argv[2],data=json.dumps(b).encode(),headers={"content-type":"application/json"})\nprint(urllib.request.urlopen(r,timeout=30).read().decode())\n';
      const body = JSON.parse(fixture.exec(["python3", "-c", code, fixture.app, route, data?.fixture_wrong_pg ? "invalid" : "valid"]).stdout);
      assert(body.success, body.message);
      return body.data;
    }
    return ordinaryRequest(route, data, port);
  };
  const migrationCount = () => fixture.exec(["sh", "-ec", 'PGPASSWORD="$POSTGRES_PASSWORD" PGCONNECT_TIMEOUT=2 psql -X -w -h 127.0.0.1 -U "$POSTGRES_USER" -d "$POSTGRES_DB" -qAt -c "SELECT runs FROM source_recovery_check WHERE id=1"']).stdout.trim();
  const expectCredentialFailure = async () => {
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
    fixture.check("credential failure is visible as log before unique failed terminal", events.some(event => event.event === "log" && /PostgreSQL|postgres|database|数据库|authentication/i.test(event.line)) && events.at(-1)?.event === "failed" && events.filter(event => event.event === "failed").length === 1);
    fixture.check("failed credentials do not execute migrations", migrationCount() === "1");
  };
  const recovery = () => {
    const code = 'import os,json,urllib.request\nr=urllib.request.Request("http://127.0.0.1:3010/v1/runtime/recovery",headers={"X-Deploy-Token":os.environ["APP_CLI_DEPLOY_TOKEN"]})\nprint(urllib.request.urlopen(r,timeout=5).read().decode())\n';
    const body = JSON.parse(fixture.exec(["python3", "-c", code]).stdout);
    assert(body.success, body.message);
    return body.data;
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
    const old = JSON.parse(fixture.read(fixture.state + "/.deploy-operation.json"));
    fixture.check("confirmed Source receipt intentionally redacts PG", old.active.request.execution_target === "source" && old.active.request.run_pg.password === "");
    const oldArtifact = old.active.artifact_release_id;
    fixture.removeCurrent();
    await fixture.create();
    fixture.check("new container keeps the original volume and data", fixture.cid !== oldContainer && fixture.read(fixture.workspace + "/sentinel") === fixture.id);
    // Normal managed serve boot must observe the redacted prior input. The
    // original container boot deliberately used control-only to avoid autorun.
    const conf = path.join(fixture.host, "conf-1", "fixture.conf");
    const content = fs.readFileSync(conf, "utf8");
    assert(content.includes("serve --control-only --workspace"));
    fs.writeFileSync(conf, content.replace("serve --control-only --workspace", "serve --workspace"));
    fixture.exec(["supervisorctl", "reread"]);
    fixture.exec(["supervisorctl", "update", "app-cli"]);
    fixture.marker("explicit-after-replacement");
    const previous = fixture.read(manifest);
    assert(previous.includes('name="Quality runtime"'));
    fixture.write({ [manifest]: previous.replace('name="Quality runtime"', 'name="Edited current Source"') });
    await fixture.boot();
    const protectedOwner = recovery();
    fixture.check("redacted previous PG holds automatic business only", protectedOwner.owner_protected && protectedOwner.credentials_required && !protectedOwner.kernel_protected);
    await expectCredentialFailure();
    await fixture.start("restart", "explicit-after-replacement");
    const current = JSON.parse(fixture.read(fixture.state + "/.deploy-operation.json"));
    fixture.check("new Source replaces an older content identity", current.active.artifact_release_id !== oldArtifact && current.active.request.execution_target === "source");
    fixture.check("new successful receipt still contains no PG password", current.active.request.run_pg.password === "");
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
  } catch (failure) { error = failure; }
  await fixture.finish(error);
  console.log("SOURCE_CREDENTIAL_RECOVERY_OK");
}
main().catch(error => { console.error("SOURCE_CREDENTIAL_RECOVERY_FAIL:", error.message); process.exitCode = 1; });
