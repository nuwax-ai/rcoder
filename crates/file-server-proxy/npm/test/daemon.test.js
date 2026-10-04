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
