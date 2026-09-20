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
async function waitRunning(binary, args, env, child, timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs;
  let error;
  while (Date.now() < deadline) {
    if (child.exitCode !== null || child.signalCode !== null) throw new Error("native owner exited before readiness");
    try {
      const status = await control(binary, "status", args, env, env.FILE_SERVER_PROXY_LAUNCH_ID);
      if (status.phase === "Running" && status.instance_id === env.FILE_SERVER_PROXY_LAUNCH_ID) return status;
    } catch (cause) { error = cause; }
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  throw new Error(`native owner readiness unknown; receipt preserved: ${error?.message || "deadline"}`);
}
module.exports = { control, waitRunning };
