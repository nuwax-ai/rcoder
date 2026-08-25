"use strict";

// daemon + orchestrate 的可本地验证部分：状态文件读写、TCP 探测、随机端口、
// 健康探测（起临时 HTTP 服务）。不涉及真实二进制/TS 进程。

const test = require("node:test");
const assert = require("node:assert");
const http = require("node:http");
const net = require("node:net");

const daemon = require("../lib/daemon");
const orch = require("../lib/orchestrate");

// —— 状态文件 ——

test("state roundtrip: write → read → clear", () => {
  const state = {
    pid: 12345,
    port: 60000,
    policy: "userapp_split",
    rustPort: 8086,
    tsPort: 54321,
    tsManaged: true,
    detached: true,
    startedAt: "2026-08-25T00:00:00.000Z",
  };
  daemon.writeState(state);
  assert.deepStrictEqual(daemon.readState(), state);
  daemon.clearState();
  assert.strictEqual(daemon.readState(), null);
});

test("readState tolerates missing/corrupt file", () => {
  daemon.clearState();
  assert.strictEqual(daemon.readState(), null);
});

test("pidAlive: current process is alive, bogus high pid is not (unix)", () => {
  assert.strictEqual(daemon.pidAlive(process.pid), true);
  // 不断言 bogus pid 必死——不同平台回收策略不同；用 0/负数这种必然非法值
  assert.strictEqual(daemon.pidAlive(0), false);
  assert.strictEqual(daemon.pidAlive(-1), false);
});

// —— TCP 探测 ——

test("tcpUp detects listener and free port", async () => {
  const server = net.createServer(() => {});
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address();
  assert.strictEqual(await daemon.tcpUp(port), true);
  await new Promise((resolve) => server.close(resolve));
  // 关闭后（等内核释放）短暂轮询确认探测为 false
  let down = false;
  for (let i = 0; i < 10 && !down; i++) {
    // eslint-disable-next-line no-await-in-loop
    down = !(await daemon.tcpUp(port));
    if (!down) await new Promise((r) => setTimeout(r, 100));
  }
  assert.strictEqual(down, true);
});

test("waitTcpUp resolves once port listens", async () => {
  const server = net.createServer(() => {});
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address();
  assert.strictEqual(await daemon.waitTcpUp(port, 2000), true);
  await new Promise((resolve) => server.close(resolve));
});

// —— 随机端口 ——

test("getFreePort returns usable ephemeral port", async () => {
  const port = await orch.getFreePort();
  assert.ok(Number.isInteger(port) && port > 0 && port < 65536);
  // 返回的口应可立即 bind（竞态窗口极小）
  const server = net.createServer(() => {});
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, "127.0.0.1", resolve);
  });
  await new Promise((resolve) => server.close(resolve));
});

// —— 健康探测 ——

function startHealthServer() {
  return new Promise((resolve) => {
    const server = http.createServer((req, res) => {
      if (req.url === "/health") {
        res.writeHead(200, { "content-type": "text/plain" });
        res.end("ok");
      } else {
        res.writeHead(404);
        res.end();
      }
    });
    server.listen(0, "127.0.0.1", () => resolve(server));
  });
}

test("probeHealth: 200 on /health is healthy, dead port is not", async () => {
  const server = await startHealthServer();
  const { port } = server.address();
  assert.strictEqual(await orch.probeHealth(port), true);
  await new Promise((resolve) => server.close(resolve));
  assert.strictEqual(await orch.probeHealth(port), false);
});

test("waitHealth resolves when healthy and throws on timeout", async () => {
  const server = await startHealthServer();
  const { port } = server.address();
  assert.strictEqual(await orch.waitHealth(port, 3000, "test-server"), true);
  await new Promise((resolve) => server.close(resolve));
  await assert.rejects(
    () => orch.waitHealth(port, 400, "test-server"),
    /did not become healthy/,
  );
});

// —— TS PID 文件解析 ——

test("readTsPid parses valid file and returns null for garbage", () => {
  const fs = require("node:fs");
  const path = require("node:path");
  const os = require("node:os");
  const pidPath = orch.tsPidPath();
  fs.mkdirSync(path.dirname(pidPath), { recursive: true });
  const original = fs.existsSync(pidPath) ? fs.readFileSync(pidPath, "utf8") : null;
  try {
    fs.writeFileSync(
      pidPath,
      JSON.stringify({ pid: 4242, port: "51234", env: "production" }),
    );
    assert.deepStrictEqual(orch.readTsPid(), {
      pid: 4242,
      port: "51234",
      env: "production",
    });
    fs.writeFileSync(pidPath, "not json at all");
    assert.strictEqual(orch.readTsPid(), null);
  } finally {
    if (original === null) fs.unlinkSync(pidPath);
    else fs.writeFileSync(pidPath, original);
  }
});
