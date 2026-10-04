"use strict";
// Rust owns scope locks, instance receipts, authenticated control and drain.
// JS never turns PID files or a listening port into process authority.
const { execFile } = require("node:child_process");
const { promisify } = require("node:util");
const execute = promisify(execFile);
async function control(binary, action, args = [], env = process.env, expectedInstance) {
  const { stdout } = await execute(binary, [action, "--native-owner", ...(expectedInstance ? ["--instance-id", expectedInstance] : []), ...args], {
    env, windowsHide: true, timeout: 65000, maxBuffer: 16384,
  });
  return JSON.parse(stdout);
}
// PX-01: readiness is tied to the REAL supervisor/owner identity published by
// the status receipt (instance_id), never to this wrapper's launch UUID. The
// launch id is only a request correlation: when the receipt carries it this
// launch created the owner; when a different owner is already Running the
// start is a verified reuse and the wrapper child exiting is NOT a readiness
// failure (and must never stop the reused owner).
async function waitRunning(binary, args, env, child, timeoutMs = 15000) {
  const launchId = env.FILE_SERVER_PROXY_LAUNCH_ID;
  const deadline = Date.now() + timeoutMs;
  let error;
  while (Date.now() < deadline) {
    try {
      const status = await control(binary, "status", args, env);
      if (status.phase === "Running") {
        if (launchId && status.launch_request_id === launchId) {
          return { ...status, launch: "created" };
        }
        return { ...status, launch: "reused" };
      }
    } catch (cause) { error = cause; }
    // The wrapper child may exit right after printing a reuse status; keep
    // polling — the real owner runs under the supervisor, not under this child.
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  throw new Error(`native owner readiness unknown; receipt preserved: ${error?.message || "deadline"}`);
}
module.exports = { control, waitRunning };
