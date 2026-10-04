#!/usr/bin/env node
"use strict";
const { randomUUID } = require("node:crypto");
const { spawn } = require("node:child_process");
const { ensureBinary } = require("../lib/index");
const { control, waitRunning, stopOwned, CONTROL_TIMEOUT_MS } = require("../lib/daemon");
const { compatibilityArgs } = require("../lib/orchestrate");

async function main(argv) {
  if (argv.includes("--help") || argv.includes("-h")) {
    console.log("file-server-proxy start|stop|status|restart|recover|retire [--port N] [--policy all_rust|ts_first|all_ts] [--ts-port N] [--native] [--detached]\n" +
      "  --native selects the native host profile (loopback default + auto-generated file API credentials in the owner state dir).\n" +
      "  Without --native the managed container default applies (0.0.0.0 bind, no credentials) — the npm wrapper never guesses by OS or directory.\n" +
      "Rust owns each listener scope; unknown owners require reconciliation, never PID cleanup.");
    return;
  }
  const binary = await ensureBinary();
  if (argv.includes("--version") || argv.includes("-V")) {
    const child = spawn(binary, ["--version"], { stdio: "inherit", windowsHide: true });
    child.on("error", error => { console.error(error.message); process.exitCode = 1; });
    child.on("exit", code => { process.exitCode = code ?? 1; });
    return;
  }
  const action = argv.shift();
  if (!["start", "stop", "status", "restart", "recover", "retire"].includes(action)) throw new Error("expected start, stop, status, restart, recover or retire");
  let policy = process.env.FILE_SERVER_PROXY_POLICY || "all_rust";
  let detached = false, tsPort;
  const scope = [], forwarded = [];
  for (let i = 0; i < argv.length; i++) {
    const flag = argv[i];
    if (["--instance-id", "--supervisor-id", "--request-id"].includes(flag)) {
      const value = argv[++i];
      if (!value) throw new Error(`missing ${flag}`);
      scope.push(flag, value); continue;
    }
    if (flag === "--detached") { detached = true; continue; }
    if (flag === "--native" || flag === "--no-native") { forwarded.push(flag); continue; }
    if (flag === "--all") throw new Error("--all cannot authorize stopping external TS instances; stop targets only this Rust owner");
    if (!["--port", "--rust-port", "--ts-port", "--policy"].includes(flag)) throw new Error(`unknown argument ${flag}`);
    const value = argv[++i];
    if (value === undefined) throw new Error(`missing value for ${flag}`);
    if (flag === "--policy") { policy = value === "userapp_split" ? "ts_first" : value; continue; }
    const port = Number(value);
    if (!Number.isInteger(port) || port < (flag === "--port" ? 0 : 1) || port > 65535) throw new Error(`invalid ${flag}`);
    if (flag === "--ts-port") tsPort = port;
    else { forwarded.push(flag, String(port)); if (flag === "--port") scope.push(flag, String(port)); }
  }
  if (!["all_rust", "ts_first", "all_ts"].includes(policy)) throw new Error("unsupported policy");
  const env = { ...process.env };
  if (action === "status" || action === "stop" || action === "recover" || action === "retire") {
    console.log(JSON.stringify(await control(binary, action, scope, env)));
    return;
  }
  if (action === "restart") await control(binary, "stop", scope, env);
  env.FILE_SERVER_PROXY_LAUNCH_ID = randomUUID();
  const args = ["start", "--native-owner", "--embed", "--policy", policy, ...forwarded, ...compatibilityArgs(policy, tsPort, env)];
  const child = spawn(binary, args, { env, detached, windowsHide: true, stdio: detached ? "ignore" : ["inherit", "pipe", "pipe"] });
  if (!detached) {
    child.stdout.pipe(process.stdout, { end: false });
    child.stderr.pipe(process.stderr, { end: false });
  }
  let spawnError;
  child.on("error", error => { spawnError = error; });

  // P1-1 启动取消协议:
  // - 信号只记录取消意图, 分配本次取消的操作身份（request_id）。在归属明确前
  //   绝不用未核验身份 Stop 当前 root——launch UUID 不是 generation。
  // - waitRunning 有界地继续到归属明确:
  //     created → 绑定捕获的 instance_id + 同一 request_id 有界 Stop
  //               （回复丢失时同 id 重试一次; 身份被拒说明换代, 如实报告）;
  //     reused  → 附着退出, 原 owner 保持运行;
  //     未知    → 报告启动结果未知（receipt preserved）, 不伪报 Stopped。
  // - 重复信号幂等（同一意图）; 所有退出路径统一回收监听器。
  let cancelled = false;
  let cancelDeadline;
  let resolveCancel;
  const cancellation = new Promise(resolve => { resolveCancel = resolve; });
  const childExit = new Promise(resolve => {
    child.once("exit", (code, signal) => resolve({ code, signal }));
    child.once("error", error => resolve({ error }));
  });
  const cancelRequestId = randomUUID();
  const onSignal = () => {
    if (!cancelled) {
      cancelled = true;
      cancelDeadline = Date.now() + CONTROL_TIMEOUT_MS;
      resolveCancel();
    }
  };
  const listeners = detached ? [] : [["SIGINT", onSignal], ["SIGTERM", onSignal]];
  for (const [signal, handler] of listeners) process.on(signal, handler);
  try {
    const status = await waitRunning(binary, scope, env, child);
    if (spawnError) throw spawnError;
    if (!cancelled) {
      console.log(JSON.stringify(status));
      if (detached) return;
      const outcome = await Promise.race([
        childExit.then(exit => ({ exit })),
        cancellation.then(() => ({ cancelled: true })),
      ]);
      if (outcome.exit) {
        if (outcome.exit.error) throw outcome.exit.error;
        process.exitCode = outcome.exit.code ?? 1;
        return;
      }
    }
    if (status.launch === "created") {
      const stopped = await stopOwned(binary, scope, env, status, cancelRequestId, cancelDeadline);
      console.error(`file-server-proxy: cancelled; stopped owned instance ${status.instance_id}: ${JSON.stringify(stopped)}`);
    } else {
      console.error("file-server-proxy: cancelled; owner belongs to another launch (reused) and stays running");
    }
    process.exitCode = 1;
  } catch (error) {
    if (cancelled) {
      console.error(`file-server-proxy: cancelled; ${error.message}; receipt preserved`);
      process.exitCode = 1;
      return;
    }
    throw error;
  } finally {
    for (const [signal, handler] of listeners) process.removeListener(signal, handler);
    // Detachment must close the wrapper's output pipes as well as its process
    // reference. Otherwise an execFile caller waits forever for EOF from a
    // surviving launcher. Closing these pipes is not a business Stop receipt.
    if (!detached) {
      child.stdout.unpipe(process.stdout);
      child.stderr.unpipe(process.stderr);
      child.stdout.destroy();
      child.stderr.destroy();
    }
    // Unknown startup keeps its state and launcher; it must not keep this CLI
    // alive forever or authorize an unbound Stop against the current root.
    child.unref();
  }
}
main(process.argv.slice(2)).catch(error => { console.error(`file-server-proxy: ${error.message}`); process.exitCode = 1; });
