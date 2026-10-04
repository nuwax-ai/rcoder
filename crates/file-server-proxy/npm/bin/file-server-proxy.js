#!/usr/bin/env node
"use strict";
const { randomUUID } = require("node:crypto");
const { spawn } = require("node:child_process");
const { ensureBinary } = require("../lib/index");
const { control, waitRunning } = require("../lib/daemon");
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
    if (flag === "--instance-id") {
      const value = argv[++i];
      if (!value) throw new Error("missing --instance-id");
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
  const launchId = env.FILE_SERVER_PROXY_LAUNCH_ID;
  const args = ["start", "--native-owner", "--embed", "--policy", policy, ...forwarded, ...compatibilityArgs(policy, tsPort, env)];
  const child = spawn(binary, args, { env, detached, windowsHide: true, stdio: detached ? "ignore" : "inherit" });
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
  const cancelRequestId = randomUUID();
  const onSignal = () => { cancelled = true; };
  const listeners = detached ? [] : [["SIGINT", onSignal], ["SIGTERM", onSignal]];
  for (const [signal, handler] of listeners) process.on(signal, handler);
  let cancelledOutcome = null;
  try {
    const status = await waitRunning(binary, scope, env, child);
    if (spawnError) throw spawnError;
    if (cancelled) {
      if (status.launch === "created") {
        let stopped = null;
        try {
          stopped = await control(binary, "stop", scope, env, status.instance_id, { requestId: cancelRequestId });
        } catch (first) {
          // 回复可能丢失于持久受理之后: 同一 request_id 重试一次（同一操作身份）。
          stopped = await control(binary, "stop", scope, env, status.instance_id, { requestId: cancelRequestId })
            .catch(error => { throw new Error(`cancel accepted but outcome unknown (${first.message}; ${error.message}); receipt preserved`); });
        }
        cancelledOutcome = `cancelled; stopped owned instance ${status.instance_id}: ${JSON.stringify(stopped)}`;
      } else {
        cancelledOutcome = "cancelled; owner belongs to another launch (reused) and stays running";
        if (child.exitCode === null && child.signalCode === null) child.kill("SIGTERM");
      }
    } else {
      console.log(JSON.stringify(status));
    }
  } catch (error) {
    if (cancelled) {
      // 归属未能在预算内明确（generation 未发布/未 Ready/换代）: 结果未知, 不伪报。
      cancelledOutcome = `cancelled before ownership resolved: ${error.message}`;
      if (child.exitCode === null && child.signalCode === null) child.kill("SIGTERM");
    } else {
      throw error;
    }
  } finally {
    for (const [signal, handler] of listeners) process.removeListener(signal, handler);
    if (detached) child.unref();
  }
  if (cancelledOutcome !== null) {
    console.error(`file-server-proxy: ${cancelledOutcome}`);
    process.exitCode = 1;
    return;
  }
  if (!detached) {
    const code = await new Promise(resolve => {
      if (child.exitCode !== null || child.signalCode !== null) resolve(child.exitCode ?? 1);
      else child.once("exit", code => resolve(code ?? 1));
    });
    process.exitCode = code;
  }
}
main(process.argv.slice(2)).catch(error => { console.error(`file-server-proxy: ${error.message}`); process.exitCode = 1; });
