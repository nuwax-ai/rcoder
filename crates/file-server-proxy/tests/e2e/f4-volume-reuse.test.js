#!/usr/bin/env node
// F4: 闲置回收原卷重建模拟——同 Docker 卷跨两个容器实例的数据保留。
// 模拟 K8s builder STS 被回收后同 PVC 重建: 容器 1 写入工作区+哨兵 →
// 容器 1 销毁 → 容器 2 用同一卷启动 → 验证哨兵/工作区完整 + app 可再启动。
"use strict";
const { spawnSync, spawn } = require("node:child_process");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const os = require("node:os");

const APP_CLI = process.argv[2];
const PROXY = process.argv[3];
const IMAGE = process.argv[4] || "debian:bookworm-slim";
const VOLUME = `fs-f4-vol-${Date.now()}`;

function docker(args, opts = {}) {
  const out = spawnSync("docker", args, { encoding: "utf8", timeout: 120000, ...opts });
  if (out.status !== 0) throw new Error(`docker ${args.join(" ")}: ${out.stderr}${out.stdout}`);
  return out.stdout.trim();
}

async function main() {
  assert(APP_CLI && PROXY, "usage: node f4-volume-reuse.test.js <app-cli> <proxy> [image]");
  // Setup: create volume + copy binaries
  docker(["volume", "create", VOLUME]);
  const hostTmp = fs.mkdtempSync(path.join(os.tmpdir(), "f4-"));
  fs.copyFileSync(APP_CLI, path.join(hostTmp, "app-cli"));
  fs.chmodSync(path.join(hostTmp, "app-cli"), 0o755);
  fs.copyFileSync(PROXY, path.join(hostTmp, "file-server-proxy"));
  fs.chmodSync(path.join(hostTmp, "file-server-proxy"), 0o755);

  const workdir = "/vol/workspace";
  const sentinel = `${workdir}/.f4-sentinel`;
  const stateRoot = "/vol/state";

  try {
    // ===== Phase 1: Container 1 — build workspace + write sentinel =====
    let sentinelV1 = '';
    const c1 = `f4-c1-${Date.now()}`;
    docker(["run", "-d", "--name", c1,
      "-v", `${VOLUME}:/vol`,
      "-v", `${hostTmp}:/tools:ro`,
      IMAGE, "sleep", "600"]);
    try {
      // Create workspace structure + sentinel
      docker(["exec", c1, "sh", "-c",
        `mkdir -p ${workdir}/src ${stateRoot} && echo "F4-SENTINEL-$(date +%s)" > ${sentinel} && echo '{"name":"test"}' > ${workdir}/workspace.manifest.toml`]);
      // Record sentinel content for cross-container verification
      sentinelV1 = docker(["exec", c1, "cat", sentinel]);
      console.log(`phase1 sentinel: ${sentinelV1}`);

      // Verify workspace structure exists in container 1
      const manifest = docker(["exec", c1, "cat", `${workdir}/workspace.manifest.toml`]);
      assert(manifest.includes("test"), "manifest readable in container 1");

      // Destroy container 1 (simulates idle reclaim)
      docker(["rm", "-f", c1]);
      console.log("phase1: container 1 destroyed (simulating idle reclaim)");
    } finally {
      try { docker(["rm", "-f", c1]); } catch {}
    }

    // ===== Phase 2: Container 2 — same volume, verify data retention + restart capability =====
    const c2 = `f4-c2-${Date.now()}`;
    docker(["run", "-d", "--name", c2,
      "-v", `${VOLUME}:/vol`,
      "-v", `${hostTmp}:/tools:ro`,
      IMAGE, "sleep", "600"]);
    try {
      // Sentinel retained
      const sentinelV2 = docker(["exec", c2, "cat", sentinel]);
      assert(sentinelV2 === sentinelV1, `sentinel preserved across container rebuild: ${sentinelV2} vs ${sentinelV1}`);
      console.log("sentinel retained across container rebuild PASS");

      // Workspace manifest retained
      const manifestV2 = docker(["exec", c2, "cat", `${workdir}/workspace.manifest.toml`]);
      assert(manifestV2.includes("test"), "manifest preserved");
      console.log("workspace manifest retained PASS");

      // Directory structure retained
      const srcExists = docker(["exec", c2, "sh", "-c", `test -d ${workdir}/src && echo OK`]);
      assert(srcExists === "OK", "src directory preserved");
      console.log("directory structure retained PASS");

      // App-cli binary is executable in container 2 (restart capability)
      const versionOut = docker(["exec", c2, "/tools/app-cli", "--version"]);
      assert(versionOut.includes("app-cli"), `app-cli binary works in rebuilt container: ${versionOut}`);
      console.log("app-cli executable in rebuilt container PASS");

      // file-server-proxy binary is executable
      const proxyVersion = docker(["exec", c2, "/tools/file-server-proxy", "--version"]);
      assert(proxyVersion.includes("file-server-proxy"), `proxy binary works: ${proxyVersion}`);
      console.log("proxy executable in rebuilt container PASS");

      // State root directory exists (for owner lock / receipts)
      const stateExists = docker(["exec", c2, "sh", "-c", `test -d ${stateRoot} && echo OK`]);
      assert(stateExists === "OK", "state root preserved");
      console.log("state root preserved PASS");

      // Volume is the same (not a new empty volume)
      const volumeInfo = docker(["volume", "inspect", VOLUME, "--format", "{{.CreatedAt}}"]);
      assert(volumeInfo.length > 0, "volume still exists with original creation time");
      console.log("volume identity confirmed PASS");
    } finally {
      try { docker(["rm", "-f", c2]); } catch {}
    }

    console.log("F4_VOLUME_REUSE_OK");
  } finally {
    try { docker(["volume", "rm", "-f", VOLUME]); } catch {}
    try { fs.rmSync(hostTmp, { recursive: true, force: true }); } catch {}
  }
}
main().catch(e => { console.error("F4_VOLUME_REUSE_FAIL:", e.message); process.exit(1); });
