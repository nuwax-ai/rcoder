#!/usr/bin/env node
"use strict";
const { randomUUID } = require("node:crypto");
const { spawn } = require("node:child_process");
const { ensureBinary } = require("../lib/index");
const { control, waitRunning } = require("../lib/daemon");
const { compatibilityArgs } = require("../lib/orchestrate");

async function main(argv) {
  if (argv.includes("--help") || argv.includes("-h")) {
    console.log("file-server-proxy start|stop|status|restart|recover|retire [--port N] [--policy all_rust|ts_first|all_ts] [--ts-port N] [--detached]\nRust owns each listener scope; unknown owners require reconciliation, never PID cleanup.");
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
  const child = spawn(binary, args, { env, detached, windowsHide: true, stdio: detached ? "ignore" : "inherit" });
  let spawnError;
  child.on("error", error => { spawnError = error; });
  // Native foreground exit/OS signal uses the same drain path. This wrapper's
  // signal handlers request identity-checked control, never process.kill(PID).
  const launchId = env.FILE_SERVER_PROXY_LAUNCH_ID;
  const onSignal = () => { control(binary, "stop", scope, env, launchId).catch(error => { console.error(error.message); process.exitCode = 1; }); };
  if (!detached) { process.on("SIGINT", onSignal); process.on("SIGTERM", onSignal); }
  try {
    const status = await waitRunning(binary, scope, env, child);
    if (spawnError) throw spawnError;
    console.log(JSON.stringify(status));
  } finally {
    if (detached) child.unref();
  }
  if (!detached) {
    const code = await new Promise(resolve => {
      if (child.exitCode !== null || child.signalCode !== null) resolve(child.exitCode ?? 1);
      else child.once("exit", code => resolve(code ?? 1));
    });
    process.removeListener("SIGINT", onSignal); process.removeListener("SIGTERM", onSignal);
    process.exitCode = code;
  }
}
main(process.argv.slice(2)).catch(error => { console.error(`file-server-proxy: ${error.message}`); process.exitCode = 1; });
