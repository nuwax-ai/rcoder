"use strict";
// Rust owns scope locks, instance receipts, authenticated control and drain.
// JS never turns PID files or a listening port into process authority.
const { execFile } = require("node:child_process");
const { promisify } = require("node:util");
const execute = promisify(execFile);
// PX-01/P1-1: control carries an explicit per-call budget. The default 65s
// matches the native stop budget; readiness loops pass their REMAINING
// deadline so a stuck status call can never outlive the wrapper's own budget.
async function control(binary, action, args = [], env = process.env, expectedInstance, options = {}) {
  const timeoutMs = options.timeoutMs ?? 65000;
  const requestId = options.requestId;
  const cli = [action, "--native-owner",
    ...(expectedInstance ? ["--instance-id", expectedInstance] : []),
    ...(requestId ? ["--request-id", requestId] : []),
    ...args];
  const { stdout } = await execute(binary, cli, {
    env, windowsHide: true, timeout: timeoutMs, maxBuffer: 16384,
    killSignal: "SIGKILL",
  });
  return JSON.parse(stdout);
}
// PX-01: readiness is tied to the REAL supervisor/owner identity published by
// the status receipt (instance_id), never to this wrapper's launch UUID. The
// launch id is only a request correlation: when the receipt carries it this
// launch created the owner; when a different owner is already Running the
// start is a verified reuse and the wrapper child exiting is NOT a readiness
// failure (and must never stop the reused owner).
//
// P1-1: the nominal budget is enforced ACROSS calls — each status poll gets
// only the remaining deadline (capped per call), so one hung control cannot
// stretch a 15s readiness window into 65s.
async function waitRunning(binary, args, env, child, timeoutMs = 15000) {
  const launchId = env.FILE_SERVER_PROXY_LAUNCH_ID;
  const deadline = Date.now() + timeoutMs;
  let error;
  for (;;) {
    const remaining = deadline - Date.now();
    if (remaining <= 0) break;
    try {
      const status = await control(binary, "status", args, env, null, {
        timeoutMs: Math.min(remaining, 5000),
      });
      if (status.phase === "Running") {
        if (launchId && status.launch_request_id === launchId) {
          return { ...status, launch: "created" };
        }
        return { ...status, launch: "reused" };
      }
    } catch (cause) { error = cause; }
    // The wrapper child may exit right after printing a reuse status; keep
    // polling — the real owner runs under the supervisor, not under this child.
    await new Promise(resolve => setTimeout(resolve, Math.min(100, Math.max(0, deadline - Date.now()))));
  }
  throw new Error(`native owner readiness unknown; receipt preserved: ${error?.message || "deadline"}`);
}
module.exports = { control, waitRunning };
