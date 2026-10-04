#!/usr/bin/env node
// P5 真实链验证: npm CLI → 当前构建的 Rust 二进制 → 真实 HTTP。
// 隔离原则: 独立临时 FILE_SERVER_PROXY_STATE_DIR; finally 只按捕获身份清理
// 本测试实例, 不触碰已有代理; 不再用 HOME 默认状态根。
"use strict";
const { spawnSync } = require("node:child_process");
const { readFileSync, existsSync, mkdtempSync, rmSync } = require("node:fs");
const { join } = require("node:path");
const { tmpdir } = require("node:os");
const assert = require("node:assert/strict");

const BIN = process.env.FILE_SERVER_PROXY_E2E_BINARY;
const NPM_BIN = join(__dirname, "..", "..", "npm", "bin", "file-server-proxy.js");

function npm(args, env) {
  const out = spawnSync("node", [NPM_BIN, ...args], {
    encoding: "utf8", timeout: 90000, windowsHide: true,
    env: { ...process.env, FILE_SERVER_PROXY_BINARY: BIN, ...env },
  });
  if (out.status !== 0) throw new Error(`npm ${args.join(" ")} failed: ${out.stderr}${out.stdout}`);
  return out.stdout.trim();
}

(async () => {
  assert(BIN, "FILE_SERVER_PROXY_E2E_BINARY must point at the binary under test");
  const stateDir = mkdtempSync(join(tmpdir(), "fs-proxy-e2e-"));
  const env = { FILE_SERVER_PROXY_STATE_DIR: stateDir };
  let ownedInstanceId = null;
  try {
    // 1) 首次 start: native profile + 动态端口
    const first = JSON.parse(npm(["start", "--port", "0", "--native", "--policy", "all_rust", "--detached"], env));
    assert.equal(first.launch, "created", JSON.stringify(first));
    assert.match(first.address, /^127\.0\.0\.1:\d+$/, `loopback dynamic address: ${first.address}`);
    ownedInstanceId = first.instance_id;
    // credentials 在独立 state dir 下
    const credPath = join(stateDir, "native-3132372e302e302e31-0", "credentials.json");
    if (!existsSync(credPath)) {
      // port key uses the host scope (0.0.0.0 default for directory naming)
      const alt = join(stateDir, "native-302e302e302e30-0", "credentials.json");
      assert.ok(existsSync(alt), `credentials.json in state dir: ${stateDir}`);
    }
    const credFile = existsSync(credPath) ? credPath : join(stateDir, "native-302e302e302e30-0", "credentials.json");
    const token = JSON.parse(readFileSync(credFile, "utf8")).file_api_token;
    assert.ok(token && token.length >= 32);

    // 2) 真实 HTTP: 带 token 200 / 无 token 401
    const ok = await fetch(`http://${first.address}/health`, { headers: { "X-Proxy-Token": token } });
    assert.equal(ok.status, 200);
    const unauthorized = await fetch(`http://${first.address}/health`);
    assert.equal(unauthorized.status, 401);

    // 3) status 不泄 token
    const statusText = npm(["status", "--port", "0"], env);
    assert.ok(!statusText.includes(token));

    // 4) 复用: 新 launch 命中同一 owner
    const second = JSON.parse(npm(["start", "--port", "0", "--native", "--policy", "all_rust", "--detached"], env));
    assert.equal(second.launch, "reused");
    assert.equal(second.instance_id, first.instance_id);

    // 5) 管理面不可经文件入口触达（P1-5 具体白名单）
    const adminProbe = await fetch(`http://${first.address}/api/system/file-server/stop`, { headers: { "X-Proxy-Token": token } });
    assert.equal(adminProbe.status, 404, `admin endpoint must not be reachable: ${adminProbe.status}`);

    // 6) 身份停止
    const wrongId = spawnSync("node", [NPM_BIN, "stop", "--port", "0", "--instance-id", "00000000-0000-0000-0000-000000000000"], {
      encoding: "utf8", timeout: 90000, env: { ...process.env, FILE_SERVER_PROXY_BINARY: BIN, ...env },
    });
    assert.notEqual(wrongId.status, 0, "foreign instance id must be rejected");
    const stopped = JSON.parse(npm(["stop", "--port", "0", "--instance-id", ownedInstanceId], env));
    assert.equal(stopped.phase, "Stopped");
    ownedInstanceId = null;
    console.log("E2E_NATIVE_LINK_OK");
  } finally {
    if (ownedInstanceId) {
      try { npm(["stop", "--port", "0", "--instance-id", ownedInstanceId], env); } catch {}
    }
    try { rmSync(stateDir, { recursive: true, force: true }); } catch {}
  }
})().catch(error => { console.error("E2E_NATIVE_LINK_FAIL:", error.message); process.exit(1); });
