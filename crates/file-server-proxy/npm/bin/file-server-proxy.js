#!/usr/bin/env node
"use strict";

// CLI 入口：`file-server-proxy` 命令——编排"单 Rust 进程（代理+内嵌 rust file-server）
// + TS nuwax-file-server（依赖安装，唯一额外进程）"的完整生命周期。
//
// 命令：
//   start [flags]   拉起所需上游并运行代理（默认前台；--detached 后台+日志文件）
//   stop [--all]    停代理；--all 或 TS 由本 CLI 拉起时连带停 nuwax-file-server
//   status          报告 proxy/TS 各组件状态
//   restart [flags] = stop（容错未运行）+ start（同 flags）
//
// start flags：
//   --policy <userapp_split|all_rust|all_ts>   路由策略（默认 userapp_split）
//   --port <N>      代理监听端口（默认 60000）
//   --rust-port <N> 内嵌 rust file-server 端口（默认 8086）
//   --ts-port <N>   TS 端口（默认随机分配未占用端口；仅 userapp_split/all_ts 需要 TS）

const { spawn } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

const { ensureBinary } = require("../lib/index");
const { ensureTs, tsStop, probeHealth, probeExistingTs, waitForHttpHealth } =
  require("../lib/orchestrate");
const {
  logPath,
  readState,
  writeState,
  clearState,
  pidAlive,
  killAndWait,
  tcpUp,
  waitTcpFree,
} = require("../lib/daemon");

const VERSION = require("../package.json").version;

// 管道消费方提前关闭（`| head`、`| grep -m1` 等）时 stdout 写入 EPIPE——node 默认
// uncaught crash（退出码 1 + 丑陋栈）；输出被截断是调用方意图，按其静默退出。
for (const stream of [process.stdout, process.stderr]) {
  if (stream && typeof stream.on === "function") {
    stream.on("error", (err) => {
      if (err.code === "EPIPE") process.exit(0);
      throw err;
    });
  }
}

const DEFAULT_LISTEN_PORT = 60000;
const DEFAULT_RUST_PORT = 8086;
const POLICIES = new Set(["userapp_split", "all_rust", "all_ts", "ts_first"]);
// TS_UPSTREAM_PORT 的缺省值与 Rust 侧 NUWAX_FILE_SERVER_INTERNAL_PORT 一致；仅在
// all_rust（不用 TS）时作为占位传入
const FALLBACK_TS_PORT = 60001;

function usage() {
  return `file-server-proxy ${VERSION}

Usage:
  file-server-proxy start [--policy <userapp_split|all_rust|all_ts|ts_first>]
                          [--port <60000>] [--rust-port <8086>]
                          [--ts-port <N>] [--detached]
  file-server-proxy stop [--all]
  file-server-proxy status
  file-server-proxy restart [start flags]
  file-server-proxy --version | -V

Policies:
  userapp_split  /api/userapp* or x-service-type:userapp → embedded rust;
                  everything else → nuwax-file-server (default)
  all_rust       everything → embedded rust file-server (no TS process needed)
  all_ts         everything → nuwax-file-server
  ts_first       only /api/userapp* → embedded rust; legacy paths → TS even
                  with x-service-type header (TS handles service_type in-band)

Environment:
  FILE_SERVER_PROXY_BINARY      use a custom proxy binary (skip OSS download)
  FILE_SERVER_PROXY_TARGET      override the Rust target triple
  FILE_SERVER_PROXY_SKIP_DOWNLOAD=1  skip postinstall pre-download`;
}

function fail(message, code = 1) {
  console.error(`file-server-proxy: ${message}`);
  process.exit(code);
}

// 轻量 argv 解析（与包内其他脚本一致：零运行时依赖）。非法 flag/值直接报错退出。
function parseArgs(argv) {
  const out = { command: null, policy: "userapp_split", detached: false, all: false };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    const next = () => {
      const value = argv[++i];
      if (value === undefined) fail(`missing value for ${arg}`);
      return value;
    };
    switch (arg) {
      case "start":
      case "stop":
      case "status":
      case "restart":
        if (out.command) fail(`multiple commands given (${out.command}, ${arg})`);
        out.command = arg;
        break;
      case "--policy":
        out.policy = next();
        break;
      case "--port":
        out.port = parsePort(arg, next());
        break;
      case "--rust-port":
        out.rustPort = parsePort(arg, next());
        break;
      case "--ts-port":
        out.tsPort = parsePort(arg, next());
        break;
      case "--detached":
        out.detached = true;
        break;
      case "--all":
        out.all = true;
        break;
      case "--help":
      case "-h":
        console.log(usage());
        process.exit(0);
        break;
      case "--version":
      case "-V":
        console.log(VERSION);
        process.exit(0);
        break;
      default:
        fail(`unknown argument: ${arg}\n\n${usage()}`);
    }
  }
  if (!out.command) {
    console.error(usage());
    process.exit(1);
  }
  if (!POLICIES.has(out.policy)) {
    fail(`invalid --policy ${out.policy}: expected one of ${[...POLICIES].join(" | ")}`);
  }
  return out;
}

function parsePort(flag, value) {
  const port = Number(value);
  if (!Number.isInteger(port) || port <= 0 || port > 65535) {
    fail(`invalid port for ${flag}: ${value}`);
  }
  return port;
}

// 当前是否有活实例（state + pid 双重校验；陈旧 state 视为未运行并顺手清理）。
function runningState() {
  const state = readState();
  if (!state) return null;
  if (!pidAlive(state.pid)) {
    clearState();
    return null;
  }
  return state;
}

async function cmdStart(flags) {
  const running = runningState();
  if (running) {
    fail(`already running (pid ${running.pid}, port ${running.port}); run "stop" first`);
  }

  const listenPort = flags.port ?? DEFAULT_LISTEN_PORT;
  const rustPort = flags.rustPort ?? DEFAULT_RUST_PORT;
  const needsTs = flags.policy !== "all_rust";

  const binaryPath = await ensureBinary();

  let tsPort = null;
  let tsManaged = false;
  if (needsTs) {
    const ts = await ensureTs(flags.tsPort);
    tsPort = ts.port;
    tsManaged = ts.managed;
  }

  const childEnv = {
    ...process.env,
    FILE_SERVER_PORT: String(listenPort),
    RUST_UPSTREAM_PORT: String(rustPort),
    TS_UPSTREAM_PORT: String(tsPort ?? FALLBACK_TS_PORT),
    ROUTE_POLICY: flags.policy,
    EMBED_FILE_SERVER: "1",
  };
  // 0.2.0+ 二进制走 CLI 参数（参数>env>默认）；env 同步保留双通道——兼容
  // FILE_SERVER_PROXY_BINARY 指向旧版二进制（只认 env）的场景
  const binaryArgs = ["--embed", "--policy", flags.policy, "--port", String(listenPort),
    "--rust-port", String(rustPort)];
  if (tsPort) binaryArgs.push("--ts-port", String(tsPort));

  const state = {
    pid: null, // spawn 后回填
    port: listenPort,
    policy: flags.policy,
    rustPort,
    tsPort,
    tsManaged,
    detached: flags.detached,
    startedAt: new Date().toISOString(),
  };

  if (flags.detached) {
    fs.mkdirSync(path.dirname(logPath()), { recursive: true });
    const logFd = fs.openSync(logPath(), "a");
    const child = spawn(binaryPath, binaryArgs, {
      env: childEnv,
      detached: true,
      stdio: ["ignore", logFd, logFd],
      windowsHide: true,
    });
    // spawn 异步失败（binary 缺失/不可执行）必须监听，否则 uncaught 'error' 崩溃
    let spawnError = null;
    // detached child 立即退出（bind 失败等）会成为僵尸——父进程不 wait 时
    // pidAlive 对僵尸恒真（骗过存活判定）。挂 exit 监听让 node 自动 reap 并置标志。
    let childDead = false;
    child.on("error", (err) => {
      spawnError = err;
      childDead = true;
    });
    child.on("exit", () => {
      childDead = true;
    });
    child.unref();
    state.pid = child.pid;
    writeState(state);
    // 存活判定用 /health：经代理转发上游的 200 能证明"听者是我们的 proxy"——
    // 端口被第三方占用（探测连到占用者但无 HTTP 响应）与 child 秒退（bind 失败
    // 等）都给不出健康响应；childDead 兜底僵尸场景（exit 监听置标志 + 自动 reap）。
    const healthy = await waitForHttpHealth(listenPort, 10000);
    if (!healthy || childDead) {
      const tail = readLogTail();
      // 先清完自己拉起的进程（proxy + managed TS）再报错退出，防泄漏
      await killAndWait(child.pid).catch(() => {});
      if (tsManaged) await tsStop().catch(() => {});
      clearState();
      fail(
        spawnError
          ? `failed to execute ${binaryPath}: ${spawnError.message}`
          : childDead
            ? `proxy exited before becoming healthy on ${listenPort}${tail}`
            : `proxy did not become healthy on ${listenPort} within 10s${tail}`,
      );
    }
    console.log(
      `file-server-proxy started (detached, pid ${child.pid}): http://127.0.0.1:${listenPort} ` +
        `[policy=${flags.policy} rust=127.0.0.1:${rustPort}` +
        `${tsPort ? ` ts=127.0.0.1:${tsPort}${tsManaged ? " (managed)" : " (reused)"}` : ""}]`,
    );
    console.log(`  log: ${logPath()}`);
    return;
  }

  // 前台模式：stdio 继承（日志直接可见），Ctrl-C/SIGTERM 整体清理后随码退出。
  const child = spawn(binaryPath, binaryArgs, {
    env: childEnv,
    stdio: "inherit",
    windowsHide: true,
  });
  state.pid = child.pid;
  writeState(state);

  let tearingDown = false;
  const teardown = () => {
    if (tearingDown) return;
    tearingDown = true;
    try {
      child.kill("SIGTERM");
    } catch {
      /* 已退出 */
    }
  };
  process.on("SIGINT", teardown);
  process.on("SIGTERM", teardown);

  child.on("exit", (code, signal) => {
    clearState();
    const exitWith = () => process.exit(code ?? (signal ? 1 : 0));
    // 先清完 TS 再退（process.exit 不等 pending promise）
    if (tsManaged) {
      tsStop()
        .then(() => console.error("• managed nuwax-file-server stopped"))
        .finally(exitWith);
    } else {
      exitWith();
    }
  });

  child.on("error", (err) => {
    clearState();
    if (tsManaged) {
      // spawn 失败也带走自己拉起的 TS，防泄漏（fail 同步退出不等 promise）
      tsStop()
        .catch(() => {})
        .finally(() => fail(`failed to execute ${binaryPath}: ${err.message}`));
    } else {
      fail(`failed to execute ${binaryPath}: ${err.message}`);
    }
  });
}

function readLogTail(maxChars = 2000) {
  try {
    const text = fs.readFileSync(logPath(), "utf8").trimEnd();
    return text ? `\n--- proxy.log tail ---\n${text.slice(-maxChars)}` : "";
  } catch {
    return "";
  }
}

async function cmdStop(flags) {
  const state = runningState();
  if (!state) {
    fail("not running");
  }
  const stopped = await killAndWait(state.pid);
  await waitTcpFree(state.port);
  clearState();
  console.log(
    `file-server-proxy stopped (pid ${state.pid}, port ${state.port})${stopped ? "" : " (force-killed)"}`,
  );
  if (state.tsManaged || flags.all) {
    if (await tsStop()) {
      console.log("• nuwax-file-server stopped");
    } else {
      console.error("⚠ nuwax-file-server stop reported failure (check its state)");
    }
  } else if (state.tsPort && !state.tsManaged) {
    console.log(
      `• nuwax-file-server on 127.0.0.1:${state.tsPort} was reused/external — left running (use --all to stop it)`,
    );
  }
}

async function cmdStatus() {
  const state = runningState();
  if (!state) {
    console.log("file-server-proxy: not running");
  } else {
    const entry = (await tcpUp(state.port)) ? "tcp ok" : "tcp down";
    console.log(`file-server-proxy: running (pid ${state.pid}, ${state.detached ? "detached" : "foreground"})`);
    console.log(`  entry : http://127.0.0.1:${state.port} (${entry})`);
    console.log(`  policy: ${state.policy}`);
    console.log(`  rust  : in-process (embedded, no internal port)`);
    if (state.tsPort) {
      const tsHealth = (await probeHealth(state.tsPort)) ? "health ok" : "health down";
      console.log(
        `  ts    : 127.0.0.1:${state.tsPort} (${state.tsManaged ? "managed" : "reused/external"}, ${tsHealth})`,
      );
    }
  }
  const external = await probeExistingTs();
  if (external) {
    console.log(
      `nuwax-file-server: running on 127.0.0.1:${external.port}${state && state.tsPort === external.port ? "" : " (not wired to this proxy)"}`,
    );
  } else if (!state || state.tsPort === null) {
    console.log("nuwax-file-server: not running");
  }
}

async function main() {
  const flags = parseArgs(process.argv.slice(2));
  switch (flags.command) {
    case "start":
      await cmdStart(flags);
      break;
    case "stop":
      await cmdStop(flags);
      break;
    case "status":
      await cmdStatus();
      break;
    case "restart": {
      const running = runningState();
      if (running) {
        await killAndWait(running.pid);
        await waitTcpFree(running.port);
        if (running.tsManaged) await tsStop();
        clearState();
        console.log(`file-server-proxy stopped (pid ${running.pid}); restarting…`);
      } else {
        console.log("file-server-proxy not running; starting…");
      }
      // restart 携带的 flags 原样交给 start（--all 在此无意义，剥除）
      const { command, all, ...startFlags } = flags;
      void command;
      void all;
      await cmdStart(startFlags);
      break;
    }
    default:
      fail(`unhandled command: ${flags.command}`);
  }
}

main().catch((err) => {
  console.error(`file-server-proxy: ${err.message}`);
  process.exit(1);
});
