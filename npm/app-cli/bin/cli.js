#!/usr/bin/env node
"use strict";

// @nuwax-ai/app-cli wrapper：定位当前平台的二进制子包并代理执行。
// 与 rcoder-cli 的 wrapper 差异：app-cli serve 是长驻进程（容器 PID1 语义），
// 改用 spawn + 信号转发（SIGTERM/SIGINT/SIGHUP）——docker stop 时信号必须
// 到达二进制才能优雅停服务（级联停 pingap 与各子服务）。

const { platform, arch } = process;
const { spawn } = require("child_process");
const fs = require("fs");
const path = require("path");

const platformPackages = {
  "linux-x64": "@nuwax-ai/app-cli-linux-x64",
  "linux-arm64": "@nuwax-ai/app-cli-linux-arm64",
  "darwin-x64": "@nuwax-ai/app-cli-darwin-x64",
  "darwin-arm64": "@nuwax-ai/app-cli-darwin-arm64",
  "win32-x64": "@nuwax-ai/app-cli-windows-x64",
};

const key = `${platform}-${arch}`;
const pkgName = platformPackages[key];

if (!pkgName) {
  console.error(`error: unsupported platform: ${key}`);
  console.error(`supported platforms: ${Object.keys(platformPackages).join(", ")}`);
  process.exit(1);
}

// win32 平台包附带自建 pingap.exe（上游官方无 Windows 产物），二进制带 .exe 后缀
const binName = platform === "win32" ? "app-cli.exe" : "app-cli";

let binPath;
try {
  const pkgDir = path.dirname(require.resolve(`${pkgName}/package.json`));
  binPath = path.join(pkgDir, binName);
} catch (e) {
  console.error(`error: platform package ${pkgName} not found.`);
  console.error(`please reinstall: npm install -g @nuwax-ai/app-cli`);
  process.exit(1);
}

// win32：serve 需要 pingap 反代，把包内 pingap.exe 经 env 注入（app-cli 的
// 默认值 /usr/local/bin/pingap 在 Windows 不存在）。仅用户未显式设置时注入——
// 显式 APP_CLI_PINGAP_BIN / --pingap-bin 优先。
let childEnv;
if (platform === "win32") {
  const pingapPath = path.join(path.dirname(binPath), "pingap.exe");
  if (!process.env.APP_CLI_PINGAP_BIN && fs.existsSync(pingapPath)) {
    childEnv = { ...process.env, APP_CLI_PINGAP_BIN: pingapPath };
  }
}

const child = spawn(binPath, process.argv.slice(2), {
  stdio: "inherit",
  env: childEnv,
});

for (const sig of ["SIGTERM", "SIGINT", "SIGHUP"]) {
  process.on(sig, () => {
    try {
      // Windows 上 SIGTERM/SIGHUP 是 Node 模拟信号，child.kill 退化为
      // TerminateProcess（硬杀）——本地 dev 可接受；容器 PID1 语义只在 Linux。
      child.kill(sig);
    } catch {
      // 子进程已退出则忽略
    }
  });
}

child.on("error", (e) => {
  console.error(`error: ${e.message}`);
  process.exit(1);
});

child.on("exit", (code, signal) => {
  if (signal) {
    // 子进程被信号终止：以同信号自杀，让调用方（shell/容器）看到真实死因。
    process.kill(process.pid, signal);
  }
  process.exit(code === null ? 1 : code);
});
