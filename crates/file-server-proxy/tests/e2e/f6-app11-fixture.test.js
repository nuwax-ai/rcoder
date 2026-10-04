#!/usr/bin/env node
"use strict";
// A legally serialized, sanitized app11-shape fixture. The old control and
// generation records are imported while no owner is running; the next actual
// serve boot must recover with its read-only platform binding, build, and serve.
const assert = require("node:assert/strict");
const { UserappFixture } = require("./userapp-container-fixture.cjs");

async function main() {
  const fixture = new UserappFixture("f6", process.argv[2], process.argv[3], process.argv[4]);
  let failure;
  try {
    const currentDomain = await fixture.create();
    fixture.prepare("f6-upgraded-workspace");
    const generation = "495e51b1-6bfa-4c03-913e-4e852d295803";
    const oldSupervisor = "f82599d4-bb76-4db9-b94f-d3d5793fd427";
    const oldDomain = { ...currentDomain, instance: "bb8877ad-c821-4854-adaf-0119e8f2afa2" };
    const generationPath = fixture.state + "/work/" + generation + "/generation.json";
    const record = { version: 1, id: generation, supervisor: oldSupervisor, token: "f6-prior-worker-token-never-a-live-credential", intent: "run", phase: "Running", worker_pid: 424242, exit_code: null, error: null, physical_domain: oldDomain };
    const discovery = { version: 2, instance: oldSupervisor, address: "127.0.0.1:1", token: "f6-prior-control-token-never-a-live-credential", snapshot: {
      version: 1, binding: { component: "app-cli", resource: fixture.workspace }, supervisor_id: oldSupervisor,
      generation, phase: "recovery_required", intent: "run", operation_id: null, error: "cleanup is unconfirmed: Running",
      problem: { code: "cleanup_unconfirmed", message: "prior generation has no physical exit receipt" },
    }, requests: [] };
    const original = JSON.stringify(record);
    fixture.write({ [generationPath]: original, [fixture.state + "/supervisor.json"]: JSON.stringify(discovery), [fixture.state + "/owner.lock"]: "" });
    fixture.check("fixture was imported before owner bootstrap", fixture.exec(["supervisorctl", "status", "app-cli"], false).stdout.includes("STOPPED"));
    await fixture.boot();
    const owner = JSON.parse(fixture.read(fixture.state + "/supervisor.json"));
    fixture.check("new serve actually consumes and replaces old discovery", owner.instance !== oldSupervisor && owner.snapshot.binding.resource === fixture.workspace);
    fixture.check("original unknown execution is preserved", fixture.read(generationPath) === original);
    await fixture.start("restart", "f6-upgraded-workspace");
    fixture.check("new generation has current platform identity", JSON.parse(fixture.read(fixture.state + "/supervisor.json")).instance !== oldSupervisor);
    const history = JSON.parse(fixture.read(generationPath));
    fixture.check("old Running stays history without fake exit success", history.phase === "Running" && history.exit_code === null && history.physical_domain.instance === oldDomain.instance);
    await fixture.stop();
    fixture.marker("f6-start-after-stop");
    await fixture.start("start", "f6-start-after-stop");
    fixture.check("sentinel survives recovery and retries", fixture.read(fixture.workspace + "/sentinel") === fixture.id);
    await fixture.stop();
    assert.notEqual(currentDomain.instance, oldDomain.instance);
  } catch (error) { failure = error; }
  await fixture.finish(failure);
  console.log("F6_APP11_FIXTURE_OK");
}
main().catch(error => { console.error("F6_APP11_FIXTURE_FAIL:", error.message); process.exitCode = 1; });
