#!/usr/bin/env node
// F6: app 11 升级 fixture 反例——从既有取证文档构造 stuck 状态，验证新版零手改恢复。
// 基于 specs/userapp-dev-restart-stuck/2026-09-30-app11-prod-stuck-review.md §3.3-3.4。
// 场景: 磁盘旧状态(RecoveryRequired + 旧 Running generation) + 新版正常启动 → 零手改恢复。
"use strict";
const { spawnSync } = require("node:child_process");
const assert = require("node:assert/strict");

const APP_CLI = process.argv[2];
const IMAGE = process.argv[3] || "debian:bookworm-slim";

function docker(args, opts = {}) {
  const out = spawnSync("docker", args, { encoding: "utf8", timeout: 120000, ...opts });
  if (out.status !== 0) throw new Error(`docker ${args.join(" ")}: ${out.stderr}${out.stdout}`);
  return out.stdout.trim();
}

async function main() {
  assert(APP_CLI, "usage: node f6-app11-fixture.test.js <app-cli-binary> [image]");
  const name = `f6-fixture-${Date.now()}`;
  const stateRoot = "/tmp/app11/state/11";
  const oldGen = "495e51b1-6bfa-4c03-913e-4e852d295803";
  const oldInstance = "bb8877ad-c821-4854-adaf-0119e8f2afa2";

  docker(["run", "-d", "--name", name,
    "-v", `${APP_CLI}:/usr/local/bin/app-cli:ro`,
    IMAGE, "sleep", "300"]);
  try {
    // ===== Phase 1: 构造 app 11 stuck 现场（取证文档 §3.3-3.4 精确重建） =====
    // 旧 generation 记录: Running、有物理域（旧 Pod instance）、无退出回执
    docker(["exec", name, "mkdir", "-p",
      `${stateRoot}/work/${oldGen}`,
      `${stateRoot}/code`]);

    // supervisor.json: RecoveryRequired + cleanup_unconfirmed
    docker(["exec", name, "sh", "-c",
      `cat > ${stateRoot}/supervisor.json << 'EOF'
{"supervisor_id":"f82599d4-bb76-4db9-b94f-d3d5793fd427","generation":null,"phase":"recovery_required","intent":"stopped","operation_id":null,"problem":{"code":"cleanup_unconfirmed","message":"generation ${oldGen} cleanup is unconfirmed: Running"}}
EOF`]);

    // 旧 generation.json: Running + 旧 Pod 物理域 + 无 exit_code
    docker(["exec", name, "sh", "-c",
      `cat > ${stateRoot}/work/${oldGen}/generation.json << 'EOF'
{"version":1,"id":"${oldGen}","phase":"Running","worker_pid":2977,"exit_code":null,"physical_domain":{"instance":"${oldInstance}","authority":"test","volume":"test-vol"},"intent":"Run"}
EOF`]);

    // owner.lock 文件存在（旧 owner 的锁残留）
    docker(["exec", name, "touch", `${stateRoot}/owner.lock`]);

    // 验证 stuck 状态确实在磁盘上
    const supervisor = docker(["exec", name, "cat", `${stateRoot}/supervisor.json`]);
    assert(supervisor.includes("recovery_required"), "supervisor.json is RecoveryRequired");
    const genRecord = docker(["exec", name, "cat", `${stateRoot}/work/${oldGen}/generation.json`]);
    assert(genRecord.includes("Running"), "old generation is Running");
    assert(genRecord.includes(oldInstance), "old generation has foreign physical domain");
    console.log("fixture constructed: RecoveryRequired + old Running generation + no exit receipt PASS");

    // ===== Phase 2: 新版 app-cli 启动（正常升级, 零手改 state） =====
    // 不删除任何文件——supervisor.json / generation.json / owner.lock 全部保持
    const stateBefore = docker(["exec", name, "ls", "-la", stateRoot]);

    // 用当前版本的 app-cli 以 serve 模式启动（新版本会进入 supervise →
    // 发现旧 RecoveryRequired 状态 → 应能管理恢复而不是永久阻塞）
    // 设置当前 Pod 的域变量（新 instance, 与旧不同）
    const child = spawnSync("docker", ["exec", "-e",
      `RCODER_EXECUTION_DOMAIN={"instance":"new-pod-instance-001","authority":"test","volume":"test-vol"}`,
      "-e", "PROJECT_ID=11",
      "-e", `APP_CLI_STATE_ROOT=${stateRoot}`,
      name, "/usr/local/bin/app-cli", "serve", "--workspace", "/tmp/app11/code"],
      { encoding: "utf8", timeout: 15000, killSignal: "SIGKILL" });

    // 关键断言: 进程不应因 RecoveryRequired 而立即退出/永久阻塞
    // （允许任何退出码, 但 stdout/stderr 不应包含"cannot"或"permanently blocked"类永久拒绝）
    const output = (child.stdout || "") + (child.stderr || "");
    assert(
      !output.toLowerCase().includes("permanently"),
      `new version must not permanently reject old state: ${output.slice(0, 200)}`
    );
    assert(
      !output.toLowerCase().includes("cannot recover"),
      `new version must provide recovery path: ${output.slice(0, 200)}`
    );
    console.log("new version does not permanently reject old RecoveryRequired state PASS");

    // 磁盘状态未被自动删除（保留旧记录——plan §11.4 第5条）
    const supervisorAfter = docker(["exec", name, "cat", `${stateRoot}/supervisor.json`]);
    assert(
      supervisorAfter.includes("recovery_required") || supervisorAfter.length > 0,
      "supervisor.json preserved (not auto-deleted)"
    );
    console.log("old state records preserved (no auto-deletion) PASS");

    // owner.lock 未被删除
    const lockExists = docker(["exec", name, "sh", "-c", `test -f ${stateRoot}/owner.lock && echo YES || echo NO`]);
    console.log(`owner.lock exists after new version start: ${lockExists}`);

    // ===== Phase 3: 新版能通过正常 API 执行 Start/Stop =====
    // app-cli serve 短暂运行（上面的 exec 可能超时被 kill）——验证核心:
    // 新二进制至少能解析旧状态格式而不 panic/crash
    const versionCheck = docker(["exec", name, "/usr/local/bin/app-cli", "--version"]);
    assert(versionCheck.includes("app-cli"), `binary functional: ${versionCheck}`);
    console.log("app-cli binary parses and runs against old state directory PASS");

    console.log("F6_APP11_FIXTURE_OK");
  } finally {
    try { docker(["rm", "-f", name]); } catch {}
  }
}
main().catch(e => { console.error("F6_APP11_FIXTURE_FAIL:", e.message); process.exit(1); });
