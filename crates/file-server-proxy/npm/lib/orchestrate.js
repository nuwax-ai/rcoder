"use strict";

// nuwax-file-server（TS）托管：探测已有实例（复用）、随机端口拉起、健康等待、停机。
//
// 不走 TS 的 `cli.js start`——它以 `stdio: pipe` 转发 server 日志且从不解除流的
// 引用，server 存活期间 CLI 进程永不退出（spawnSync 等它必超时，超时连带 server
// 被清理）。改为直接 detached 托管 `dist/server.js` + 自写 TS 语义 PID 文件：
// - 与 `nuwax-file-server` CLI 生态兼容（其 stop/status 读同一 PID 文件）；
// - 生命周期完全由本 CLI 掌控（kill/等退出/清理）。
//
// TS 是全局单实例设计（PID 文件在 os.tmpdir()/nuwax-file-server/）：已有健康实例
// 则复用其端口（不接管生命周期）；不健康/僵死实例先清理再拉起并标记 managed。

const fs = require("node:fs");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");
const { spawn } = require("node:child_process");

const { killAndWait, pidAlive } = require("./daemon");

function tsStateDir() {
  return path.join(os.tmpdir(), "nuwax-file-server");
}

// nuwax-file-server 的全局 PID 文件（其 appConfig 定义的 CLI_PID_DIR 在 os.tmpdir() 下）。
function tsPidPath() {
  return path.join(tsStateDir(), "server.pid");
}

// TS server 入口（依赖安装后必然存在；绝对路径 + 本进程 node 直接调用）。
function tsServerPath() {
  return require.resolve("nuwax-file-server/dist/server.js");
}

// TS 包版本（PID 文件元数据，对齐其 CLI 写入的字段）。
function tsVersion() {
  return require("nuwax-file-server/package.json").version;
}

// 读取 TS 全局 PID 文件。损坏/缺失返回 null。
function readTsPid() {
  try {
    return JSON.parse(fs.readFileSync(tsPidPath(), "utf8"));
  } catch {
    return null;
  }
}

function writeTsPid(pidInfo) {
  fs.mkdirSync(tsStateDir(), { recursive: true });
  fs.writeFileSync(tsPidPath(), JSON.stringify(pidInfo, null, 2));
}

function clearTsPid() {
  try {
    fs.unlinkSync(tsPidPath());
  } catch {
    /* best-effort */
  }
}

// HTTP 健康探测（/health，短超时；任何 2xx 都算健康）。
async function probeHealth(port, timeoutMs = 2000) {
  try {
    const res = await fetch(`http://127.0.0.1:${port}/health`, {
      signal: AbortSignal.timeout(timeoutMs),
    });
    return res.ok;
  } catch {
    return false;
  }
}

// 轮询等待健康（拉起 TS 后的就绪确认）。
async function waitHealth(port, timeoutMs, label = "service") {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await probeHealth(port)) return true;
    await new Promise((resolve) => setTimeout(resolve, 300));
  }
  throw new Error(
    `${label} on 127.0.0.1:${port} did not become healthy within ${timeoutMs}ms`,
  );
}

// 找一个未占用的本地端口（listen(0) 拿内核分配的 ephemeral 端口后立即释放）。
// 释放到 TS 真正 bind 之间存在极小竞态，调用方失败时换口重试。
function getFreePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.on("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
  });
}

async function getFreePortWithRetry(tries = 3) {
  let lastError;
  for (let i = 0; i < tries; i++) {
    try {
      return await getFreePort();
    } catch (err) {
      lastError = err;
    }
  }
  throw new Error(`no free port after ${tries} tries: ${lastError.message}`);
}

// 探测 TS 全局实例是否健康运行。健康返回 { port }，否则 null。
async function probeExistingTs() {
  const pidInfo = readTsPid();
  if (!pidInfo) return null;
  const port = Number(pidInfo.port);
  if (!Number.isInteger(port) || port <= 0) return null;
  if (!(await probeHealth(port))) return null;
  return { port, pid: pidInfo.pid };
}

// 清理不健康/僵死的 TS 实例（PID 文件有记录但 health 不通）。
async function reapUnhealthyTs() {
  const pidInfo = readTsPid();
  if (pidInfo && pidAlive(Number(pidInfo.pid))) {
    process.stderr.write(
      `• cleaning up unhealthy nuwax-file-server (pid ${pidInfo.pid}, port ${pidInfo.port})\n`,
    );
    await killAndWait(Number(pidInfo.pid));
  }
  clearTsPid();
}

/**
 * 确保 TS 上游可用。返回 { port, managed, reused }：
 * - 已有健康实例 → 复用（managed=false, reused=true；stop 不动它）
 * - 不健康实例/无实例 → 随机端口（或 desiredPort）拉起并等健康（managed=true）
 */
async function ensureTs(desiredPort) {
  const existing = await probeExistingTs();
  if (existing) {
    process.stderr.write(
      `• reusing running nuwax-file-server on 127.0.0.1:${existing.port}\n`,
    );
    return { port: existing.port, managed: false, reused: true };
  }
  await reapUnhealthyTs();

  const port = desiredPort || (await getFreePortWithRetry());
  // server 日志走它自身的 LOG_BASE_DIR 文件日志（建不了会回退 tmpdir）；stdio 全
  // ignore 防 pipe 引用拖延本进程退出（TS CLI 的坑，见文件头注释）。
  const child = spawn(
    process.execPath,
    [tsServerPath()],
    {
      env: { ...process.env, PORT: String(port), NODE_ENV: "production" },
      stdio: "ignore",
      detached: true,
      windowsHide: true,
    },
  );
  child.unref();
  writeTsPid({
    pid: child.pid,
    startedAt: new Date().toISOString(),
    env: "production",
    port: String(port),
    version: tsVersion(),
    platform: process.platform,
  });
  try {
    // 秒级失败快速暴露（端口被抢/依赖缺失等），健康窗口内仍失败才等满超时
    await new Promise((resolve) => setTimeout(resolve, 800));
    if (!pidAlive(child.pid)) {
      throw new Error("nuwax-file-server exited immediately after spawn");
    }
    await waitHealth(port, 45000, "nuwax-file-server");
  } catch (err) {
    await killAndWait(child.pid);
    clearTsPid();
    throw new Error(`failed to start nuwax-file-server: ${err.message}`);
  }
  process.stderr.write(
    `• nuwax-file-server started on 127.0.0.1:${port} (managed)\n`,
  );
  return { port, managed: true, reused: false };
}

// 停止 TS（读全局 PID 文件优雅终止；无实例视为成功）。
async function tsStop() {
  const pidInfo = readTsPid();
  if (!pidInfo) return true;
  const pid = Number(pidInfo.pid);
  if (pidAlive(pid)) {
    await killAndWait(pid);
  }
  clearTsPid();
  return true;
}

module.exports = {
  tsPidPath,
  tsServerPath,
  readTsPid,
  probeHealth,
  waitHealth,
  getFreePort,
  getFreePortWithRetry,
  probeExistingTs,
  ensureTs,
  tsStop,
};
