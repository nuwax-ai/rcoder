"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const daemon = require("../lib/daemon");
const { compatibilityArgs } = require("../lib/orchestrate");

test("standard Rust path does not resolve or spawn a TS dependency", () => {
  assert.deepEqual(compatibilityArgs("all_rust"), []);
});
test("external TS selection requires an explicit valid port without PID adoption", () => {
  assert.deepEqual(compatibilityArgs("ts_first", 12345), ["--ts-port", "12345"]);
  for (const port of [0, -1, 65536]) {
    assert.throws(() => compatibilityArgs("all_ts", port), /invalid external/);
  }
});
test("exited wrapper child does not fail reuse readiness; unknown stays unknown", async () => {
  // PX-01: 复用形态下包装子进程正常退出（打印 status 后离开）, 就绪由
  // status 回执的真实身份判定; status 不可达（binary 缺失）时到 deadline
  // 报 readiness unknown——不把子进程退出误报为就绪失败, 也不误报成功。
  await assert.rejects(daemon.waitRunning("unused", [], {}, { exitCode: 0, signalCode: null }, 120), /readiness unknown/);
});
test("control cannot turn an unavailable executable into successful stop", async () => {
  await assert.rejects(daemon.control("/definitely-missing-native-owner-control", "stop"), /ENOENT/);
});

test("managed TS is explicitly resolved without creating a global PID receipt", () => {
 const env = {};
 assert.deepEqual(compatibilityArgs("ts_first", undefined, env, () => "/installed/server.js"), []);
 assert.equal(env.FILE_SERVER_PROXY_TS_NODE, process.execPath);
 assert.equal(env.FILE_SERVER_PROXY_TS_ENTRY, "/installed/server.js");
 assert.throws(() => compatibilityArgs("all_ts", undefined, {}, () => { throw new Error("missing"); }), /requires installing/);
});

test("explicit TS toolchain is preserved and partial configuration rejected", () => {
 const fs = require("node:fs"), os = require("node:os"), path = require("node:path");
 const dir = fs.mkdtempSync(path.join(os.tmpdir(), "proxy-tools-"));
 try {
  const entry = path.join(dir, "custom-entry.js"); fs.writeFileSync(entry, "");
  const env = { FILE_SERVER_PROXY_TS_NODE: process.execPath, FILE_SERVER_PROXY_TS_ENTRY: entry };
  assert.deepEqual(compatibilityArgs("all_ts", undefined, env, () => { throw new Error("must not resolve"); }), []);
  assert.equal(env.FILE_SERVER_PROXY_TS_NODE, process.execPath);
  assert.equal(env.FILE_SERVER_PROXY_TS_ENTRY, entry);
  assert.throws(() => compatibilityArgs("all_ts", undefined, { FILE_SERVER_PROXY_TS_NODE: process.execPath }), /together/);
  assert.throws(() => compatibilityArgs("all_ts", 12345, env), /conflicts/);
 } finally { fs.rmSync(dir, {recursive:true}); }
});
test("Electron executable is never silently used as Node", () => {
 assert.throws(() => compatibilityArgs("all_ts", undefined, {}, () => "/unused", {versions:{electron:"1"},execPath:"/Electron"}), /standalone Node/);
});

test("P1-1: control passes identity, request id and per-call budget to the CLI", async () => {
  const fs = require("node:fs"), os = require("node:os"), path = require("node:path");
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "proxy-ctl-"));
  const fake = path.join(dir, "fake-control");
  fs.writeFileSync(fake, `#!/usr/bin/env node
process.stdout.write(JSON.stringify(process.argv.slice(2)));
`);
  fs.chmodSync(fake, 0o755);
  try {
    const out = await daemon.control(fake, "stop", ["--port", "0"], { PATH: process.env.PATH }, "inst-1", { requestId: "req-9", supervisorId: "sup-1", timeoutMs: 5000 });
    assert.deepEqual(out, ["stop", "--native-owner", "--instance-id", "inst-1", "--supervisor-id", "sup-1", "--request-id", "req-9", "--port", "0"]);
    const minimal = await daemon.control(fake, "status", [], { PATH: process.env.PATH });
    assert.deepEqual(minimal, ["status", "--native-owner"]);
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
});

test("P1-1: waitRunning enforces the nominal budget across a slow status call", async () => {
  const fs = require("node:fs"), os = require("node:os"), path = require("node:path");
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "proxy-slow-"));
  // A status that always hangs longer than the per-call cap: if waitRunning
  // used the raw 65s default per call, a 1s budget would still block ~5s+.
  const fake = path.join(dir, "fake-control");
  fs.writeFileSync(fake, `#!/bin/sh
sleep 30
`);
  fs.chmodSync(fake, 0o755);
  const start = Date.now();
  try {
    await assert.rejects(
      daemon.waitRunning(fake, [], { FILE_SERVER_PROXY_LAUNCH_ID: "x" }, { exitCode: null, signalCode: null }, 1000),
      /readiness unknown/,
    );
    const elapsed = Date.now() - start;
    assert.ok(elapsed < 15000, `budget must bound hung status calls (took ${elapsed}ms)`);
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
});

// Protocol fixtures run the actual npm wrapper as a separate process. They
// exercise lifecycle/signal ownership, not Rust supervision or real HTTP.
const fixtureFs = require("node:fs");
const fixturePath = require("node:path");
const fixtureOs = require("node:os");
const { spawn: fixtureSpawn } = require("node:child_process");
const { once: fixtureOnce } = require("node:events");
const wrapperPath = fixturePath.resolve(__dirname, "../bin/file-server-proxy.js");

async function waitFixture(check, budget = 3000) {
  const deadline = Date.now() + budget;
  while (Date.now() < deadline) {
    if (check()) return;
    await new Promise(resolve => setTimeout(resolve, 15));
  }
  throw new Error("protocol fixture deadline exceeded");
}

function wrapperFixture(mode) {
  const root = fixtureFs.mkdtempSync(fixturePath.join(fixtureOs.tmpdir(), "proxy-wrapper-"));
  const binary = fixturePath.join(root, "protocol-binary");
  const preload = fixturePath.join(root, "budgets.cjs");
  const daemonPath = fixturePath.resolve(__dirname, "../lib/daemon.js");
  fixtureFs.writeFileSync(binary, `#!/usr/bin/env node
const fs = require("node:fs"), path = require("node:path");
const root = process.env.FILE_SERVER_PROXY_STATE_DIR;
const args = process.argv.slice(2);
if (args.includes("--version")) { console.log("file-server-proxy 0.2.3"); process.exit(0); }
const action = args[0];
fs.appendFileSync(path.join(root, "calls.jsonl"), JSON.stringify({action, args}) + "\\n");
const status = path.join(root, "status.json");
const stop = path.join(root, "stop.flag");
if (action === "start") {
  const owned = {version:2, phase:"Running", instance_id:"11111111-1111-4111-8111-111111111111", supervisor_id:"22222222-2222-4222-8222-222222222222", address:"127.0.0.1:60000", launch_request_id:process.env.FILE_SERVER_PROXY_LAUNCH_ID};
  fs.writeFileSync(path.join(root, "started.flag"), "started");
  const ended = () => {fs.writeFileSync(path.join(root,"ended.flag"),"ended");process.exit(0)};
  process.on("SIGTERM",ended);
  const tick = setInterval(() => {
    if (fs.existsSync(stop)) { clearInterval(tick); ended(); }
    if (process.env.PROXY_FIXTURE_MODE === "created" || fs.existsSync(path.join(root, "publish.flag"))) fs.writeFileSync(status, JSON.stringify(owned));
  }, 15);
} else if (action === "status") {
  console.log(fs.existsSync(status) ? fs.readFileSync(status, "utf8") : JSON.stringify({phase:"Starting"}));
} else if (action === "stop") {
  fs.writeFileSync(stop, "stopped");
  console.log(JSON.stringify({phase:"Stopped", instance_id:"11111111-1111-4111-8111-111111111111"}));
} else { process.exit(2); }
`);
  fixtureFs.chmodSync(binary, 0o755);
  // Shorten only the test budget while retaining the real wrapper/control
  // implementation and the real spawned protocol processes.
  fixtureFs.writeFileSync(preload, `const d=require(${JSON.stringify(daemonPath)}); const wait=d.waitRunning; d.waitRunning=(...args)=>wait(...args,600);`);
  if (mode === "reused" || mode === "legacy") {
    fixtureFs.writeFileSync(fixturePath.join(root, "status.json"), JSON.stringify({
      phase: "Running", instance_id: "33333333-3333-4333-8333-333333333333",
      ...(mode === "legacy" ? {} : { supervisor_id: "44444444-4444-4444-8444-444444444444", launch_request_id: "another-launch" }), address: "127.0.0.1:60000",
    }));
  }
  const child = fixtureSpawn(process.execPath, ["--require", preload, wrapperPath, "start", "--port", "0", "--native"], {
    env: { ...process.env, FILE_SERVER_PROXY_BINARY: binary, FILE_SERVER_PROXY_STATE_DIR: root, PROXY_FIXTURE_MODE: mode },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stdout = "", stderr = "";
  child.stdout.on("data", chunk => { stdout += chunk; });
  child.stderr.on("data", chunk => { stderr += chunk; });
  const exited = fixtureOnce(child, "exit");
  const closed = fixtureOnce(child, "close");
  return {
    root, child, exited, closed,
    stdout: () => stdout,
    stderr: () => stderr,
    calls: () => fixtureFs.existsSync(fixturePath.join(root, "calls.jsonl"))
      ? fixtureFs.readFileSync(fixturePath.join(root, "calls.jsonl"), "utf8").trim().split("\n").filter(Boolean).map(JSON.parse) : [],
    async cleanup() {
      fixtureFs.writeFileSync(fixturePath.join(root, "stop.flag"), "fixture cleanup");
      if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
      await closed;
      if (fixtureFs.existsSync(fixturePath.join(root, "started.flag"))) {
        await waitFixture(() => fixtureFs.existsSync(fixturePath.join(root, "ended.flag")));
      }
      fixtureFs.rmSync(root, { recursive: true, force: true });
    },
  };
}

test("foreground signal after Ready stops only its captured created instance", { skip: process.platform === "win32" }, async () => {
  const fixture = wrapperFixture("created");
  try {
    await waitFixture(() => fixture.stdout().includes('"launch":"created"'));
    fixture.child.kill("SIGTERM");
    await fixture.exited;
    const stops = fixture.calls().filter(call => call.action === "stop");
    assert.equal(stops.length, 1, "Ready foreground signal must invoke an identity-checked Stop");
    assert.equal(stops[0].args[stops[0].args.indexOf("--instance-id") + 1], "11111111-1111-4111-8111-111111111111");
    assert.ok(stops[0].args.includes("--request-id"));
    assert.equal(stops[0].args[stops[0].args.indexOf("--supervisor-id") + 1], "22222222-2222-4222-8222-222222222222");
    assert.match(fixture.stderr(), /stopped owned instance/);
  } finally { await fixture.cleanup(); }
});

test("early foreground signal waits for creation proof then stops the same request", { skip: process.platform === "win32" }, async () => {
  const fixture = wrapperFixture("deferred");
  try {
    await waitFixture(() => fixtureFs.existsSync(fixturePath.join(fixture.root, "started.flag")));
    fixture.child.kill("SIGTERM");
    fixtureFs.writeFileSync(fixturePath.join(fixture.root, "publish.flag"), "publish");
    await fixture.exited;
    const stops = fixture.calls().filter(call => call.action === "stop");
    assert.equal(stops.length, 1);
    assert.equal(stops[0].args[stops[0].args.indexOf("--instance-id") + 1], "11111111-1111-4111-8111-111111111111");
  } finally { await fixture.cleanup(); }
});

test("foreground reused attachment exits on signal without stopping the foreign owner", { skip: process.platform === "win32" }, async () => {
  const fixture = wrapperFixture("reused");
  try {
    await waitFixture(() => fixture.stdout().includes('"launch":"reused"'));
    fixture.child.kill("SIGTERM");
    const [, signal] = await fixture.exited;
    assert.equal(signal, null, "attachment handles its signal instead of default signal death");
    assert.equal(fixture.calls().filter(call => call.action === "stop").length, 0);
    assert.match(fixture.stderr(), /reused.*stays running/);
    assert.equal(JSON.parse(fixtureFs.readFileSync(fixturePath.join(fixture.root, "status.json"), "utf8")).instance_id, "33333333-3333-4333-8333-333333333333");
  } finally { await fixture.cleanup(); }
});

test("legacy owner without supervisor or launch fields remains queryable and reused", { skip: process.platform === "win32" }, async () => {
  const fixture = wrapperFixture("legacy");
  try {
    await waitFixture(() => fixture.stdout().includes('"launch":"reused"'));
    fixture.child.kill("SIGTERM");
    await fixture.exited;
    assert.equal(fixture.calls().filter(call => call.action === "stop").length, 0);
    assert.equal(JSON.parse(fixtureFs.readFileSync(fixturePath.join(fixture.root, "status.json"), "utf8")).instance_id, "33333333-3333-4333-8333-333333333333");
  } finally { await fixture.cleanup(); }
});

test("unknown startup exits the foreground wrapper within its budget without blind Stop", { skip: process.platform === "win32" }, async () => {
  const fixture = wrapperFixture("unknown");
  try {
    await waitFixture(() => fixtureFs.existsSync(fixturePath.join(fixture.root, "started.flag")));
    await waitFixture(() => fixture.child.exitCode !== null || fixture.child.signalCode !== null, 2500);
    assert.equal(fixture.child.exitCode, 1);
    await waitFixture(() => fixture.child.stdout.destroyed && fixture.child.stderr.destroyed, 1000);
    await fixture.closed;
    assert.match(fixture.stderr(), /readiness unknown/);
    assert.equal(fixture.calls().filter(call => call.action === "stop").length, 0);
  } finally { await fixture.cleanup(); }
});

test("cancel retries keep one request identity and spend one remaining deadline", { skip: process.platform === "win32" }, async () => {
  const root = fixtureFs.mkdtempSync(fixturePath.join(fixtureOs.tmpdir(), "proxy-cancel-budget-"));
  const binary = fixturePath.join(root, "protocol-binary");
  const calls = fixturePath.join(root, "calls.jsonl");
  fixtureFs.writeFileSync(binary, `#!/usr/bin/env node
const fs=require("node:fs"), args=process.argv.slice(2), calls=process.env.FIXTURE_CALLS;
const first=!fs.existsSync(calls);
fs.appendFileSync(calls, JSON.stringify(args)+"\\n");
setTimeout(()=>{process.stderr.write("injected missing reply");process.exit(2)}, first?300:3000);
`);
  fixtureFs.chmodSync(binary, 0o755);
  const start = Date.now();
  try {
    await assert.rejects(daemon.stopOwned(binary, [], { ...process.env, FIXTURE_CALLS: calls }, { instance_id: "captured-generation", supervisor_id: "captured-supervisor" }, "same-stop-request", start + 900), /cancel outcome unknown/);
    assert.ok(Date.now() - start < 1600, "retry must not allocate a second full cancel budget");
    const recorded = fixtureFs.readFileSync(calls, "utf8").trim().split("\n").map(JSON.parse);
    assert.equal(recorded.length, 2);
    for (const args of recorded) {
      assert.equal(args[args.indexOf("--instance-id") + 1], "captured-generation");
      assert.equal(args[args.indexOf("--request-id") + 1], "same-stop-request");
      assert.equal(args[args.indexOf("--supervisor-id") + 1], "captured-supervisor");
    }
  } finally { fixtureFs.rmSync(root, { recursive: true, force: true }); }
});

test("native fixture cleanup preserves unknown or still-held owner state", async () => {
  const { cleanupState } = require("../../tests/e2e/native-link.test.js");
  for (const failure of ["missing identity", "stop failed", "owner still held"]) {
    const root = fixtureFs.mkdtempSync(fixturePath.join(fixtureOs.tmpdir(), "proxy-cleanup-"));
    fixtureFs.writeFileSync(fixturePath.join(root, "owner.lock"), "preserve evidence");
    try {
      const owned = failure === "missing identity" ? null : { instance_id: "captured" };
      await assert.rejects(cleanupState(root, owned, async () => {
        if (failure === "stop failed") throw new Error("stop failed");
      }, async () => { throw new Error("owner still held"); }), /cleanup unknown.*state preserved/);
      assert.equal(fixtureFs.readFileSync(fixturePath.join(root, "owner.lock"), "utf8"), "preserve evidence");
    } finally { fixtureFs.rmSync(root, { recursive: true, force: true }); }
  }
});
