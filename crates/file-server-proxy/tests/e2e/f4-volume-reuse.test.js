#!/usr/bin/env node
"use strict";
// Real build/HTTP/Stop against two different containers sharing one owned
// volume. This is a local container contract, not RCoder recycle-controller E2E.
const assert = require("node:assert/strict");
const { UserappFixture } = require("./userapp-container-fixture.cjs");

async function main() {
  const fixture = new UserappFixture("f4", process.argv[2], process.argv[3], process.argv[4]);
  let failure;
  try {
    const firstDomain = await fixture.create();
    fixture.prepare("f4-before-recycle");
    await fixture.boot();
    await fixture.start("start", "f4-before-recycle");
    const firstId = fixture.cid;
    const buildsBefore = fixture.read(fixture.workspace + "/web/builds.log").trim().split("\n").length;
    const firstDiscovery = JSON.parse(fixture.read(fixture.state + "/supervisor.json"));
    const oldGeneration = firstDiscovery.snapshot.generation;
    assert(oldGeneration, "first real running generation required");
    const oldRecord = fixture.read(fixture.state + "/work/" + oldGeneration + "/generation.json");
    assert.equal(JSON.parse(oldRecord).phase, "Running");
    fixture.removeCurrent();
    const secondDomain = await fixture.create();
    fixture.check("replacement container identity differs", firstId !== fixture.cid);
    fixture.check("replacement physical domain differs", firstDomain.instance !== secondDomain.instance);
    fixture.check("same volume preserves original sentinel", fixture.read(fixture.workspace + "/sentinel") === fixture.id);
    fixture.check("old Running generation was retained on volume", fixture.read(fixture.state + "/work/" + oldGeneration + "/generation.json") === oldRecord);
    fixture.marker("f4-after-recycle");
    await fixture.boot();
    await fixture.start("restart", "f4-after-recycle");
    const buildsAfter = fixture.read(fixture.workspace + "/web/builds.log").trim().split("\n").length;
    fixture.check("replacement actually recompiles", buildsAfter > buildsBefore, { buildsBefore, buildsAfter });
    const retained = JSON.parse(fixture.read(fixture.state + "/work/" + oldGeneration + "/generation.json"));
    fixture.check("old running execution remains history rather than forged success", retained.phase === "Running" && retained.physical_domain.instance === firstDomain.instance);
    await fixture.stop();
    fixture.marker("f4-start-after-stop");
    await fixture.start("start", "f4-start-after-stop");
    fixture.check("final sentinel intact", fixture.read(fixture.workspace + "/sentinel") === fixture.id);
    await fixture.stop();
  } catch (error) { failure = error; }
  await fixture.finish(failure);
  console.log("F4_VOLUME_REUSE_OK");
}
main().catch(error => { console.error("F4_VOLUME_REUSE_FAIL:", error.message); process.exitCode = 1; });
