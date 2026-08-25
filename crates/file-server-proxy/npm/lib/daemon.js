"use strict";

// 守护状态与进程管理：state.json（本 CLI 与 proxy 进程之间的唯一契约）、
// 跨平台 kill、TCP 探测。状态目录与 nuwax-file-server 的 CLI 状态目录
// （os.tmpdir()/nuwax-file-server）同层但独立（os.tmpdir()/file-server-proxy）。

const fs = require("node:fs");
const net = require("node:net");
const { execFileSync } = require("node:child_process");
const os = require("node:os");
const path = require("node:path");

function stateDir() {
  return path.join(os.tmpdir(), "file-server-proxy");
}

function statePath() {
  return path.join(stateDir(), "state.json");
}

function logPath() {
  return path.join(stateDir(), "proxy.log");
}

/**
 * 读取运行状态。字段：
 * - pid        proxy 进程 pid
 * - port       proxy 监听端口（60000 入口）
 * - policy     路由策略（userapp_split|all_rust|all_ts）
 * - rustPort   内嵌 rust file-server 端口
 * - tsPort     TS 上游端口（all_rust 时为 null）
 * - tsManaged  TS 实例是否由本 CLI 拉起（stop 时连带清理的判据）
 * - detached   是否后台模式
 * - startedAt  启动时间（ISO 字符串）
 * 返回 null = 无状态文件（或内容损坏，按无实例处理）。
 */
function readState() {
  try {
    return JSON.parse(fs.readFileSync(statePath(), "utf8"));
  } catch {
    return null;
  }
}

function writeState(state) {
  fs.mkdirSync(stateDir(), { recursive: true });
  fs.writeFileSync(statePath(), JSON.stringify(state, null, 2));
}

function clearState() {
  try {
    fs.unlinkSync(statePath());
  } catch {
    /* best-effort */
  }
}

// pid 对应进程是否存活。EPERM（别人的进程）也视为存活——只判断存在性。
// pid<=0 在 unix 是进程组特殊语义（kill(-1) 广播），不是合法单进程 pid。
function pidAlive(pid) {
  if (!Number.isInteger(pid) || pid <= 0) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    return err.code === "EPERM";
  }
}

/**
 * 终止进程并等待退出（超时强杀）。proxy 二进制是单进程（rust 上游内嵌同进程，
 * 无子进程树），unix 直接 SIGTERM→SIGKILL；Windows 用 taskkill /T /F 兜树。
 */
async function killAndWait(pid, timeoutMs = 8000) {
  if (!pidAlive(pid)) return true;
  if (process.platform === "win32") {
    try {
      execFileSync("taskkill", ["/PID", String(pid), "/T", "/F"], {
        stdio: "ignore",
      });
    } catch {
      /* 进程可能已退出 */
    }
    return true;
  }
  try {
    process.kill(pid, "SIGTERM");
  } catch {
    return true;
  }
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (!pidAlive(pid)) return true;
    await sleep(200);
  }
  try {
    process.kill(pid, "SIGKILL");
  } catch {
    /* 已退出 */
  }
  await sleep(200);
  return !pidAlive(pid);
}

// TCP 端口是否有人监听（connect 成功即认为在监听，立即断开不发包）。
function tcpUp(port, timeoutMs = 1500) {
  return new Promise((resolve) => {
    const socket = net.connect({ port, host: "127.0.0.1" });
    const done = (ok) => {
      socket.destroy();
      resolve(ok);
    };
    socket.setTimeout(timeoutMs, () => done(false));
    socket.on("connect", () => done(true));
    socket.on("error", () => done(false));
  });
}

// 等待端口出现监听（detached 启动确认）。
async function waitTcpUp(port, timeoutMs = 10000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await tcpUp(port)) return true;
    await sleep(300);
  }
  return false;
}

// 等待端口释放（stop 后确认可复用）。
async function waitTcpFree(port, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (!(await tcpUp(port))) return true;
    await sleep(200);
  }
  return false;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

module.exports = {
  stateDir,
  statePath,
  logPath,
  readState,
  writeState,
  clearState,
  pidAlive,
  killAndWait,
  tcpUp,
  waitTcpUp,
  waitTcpFree,
  sleep,
};
