"use strict";
const fs = require("node:fs");
const path = require("node:path");
// Explicit external upstreams are never adopted. JS resolves installed paths;
// Rust retains the process-tree handle. Caller-selected tools take precedence.
function compatibilityArgs(policy, tsPort, env = {}, resolve = require.resolve, runtime = process) {
  const node = env.FILE_SERVER_PROXY_TS_NODE;
  const entry = env.FILE_SERVER_PROXY_TS_ENTRY;
  if ((node !== undefined) !== (entry !== undefined)) throw new Error("TS_NODE and TS_ENTRY must be configured together");
  if (node !== undefined && (!path.isAbsolute(node) || !path.isAbsolute(entry) ||
      !fs.statSync(node).isFile() || !fs.statSync(entry).isFile())) {
    throw new Error("explicit TS toolchain requires existing absolute files");
  }
  if (policy === "all_rust") {
    if (node !== undefined) throw new Error("explicit TS toolchain conflicts with all_rust policy");
    return [];
  }
  if (tsPort !== undefined) {
    if (!Number.isInteger(tsPort) || tsPort <= 0 || tsPort > 65535) throw new Error("invalid external TS port");
    if (node !== undefined) throw new Error("external TS port conflicts with a managed TS toolchain");
    return ["--ts-port", String(tsPort)];
  }
  if (node !== undefined) return [];
  if (runtime.versions.electron) throw new Error("Electron requires an explicit standalone Node and TS entry toolchain");
  let resolved;
  try { resolved = resolve("nuwax-file-server/dist/server.js"); }
  catch (error) { throw new Error("explicit TS compatibility requires installing nuwax-file-server, or --ts-port for an external upstream", { cause: error }); }
  env.FILE_SERVER_PROXY_TS_NODE = runtime.execPath;
  env.FILE_SERVER_PROXY_TS_ENTRY = resolved;
  return [];
}
module.exports = { compatibilityArgs };
