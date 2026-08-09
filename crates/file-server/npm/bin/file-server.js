#!/usr/bin/env node
"use strict";

// CLI 入口：`file-server` 命令。首次运行会从 OSS 下载二进制并缓存，之后直接复用。
// 所有 argv 透传给原生二进制；stdio 继承（长驻 HTTP 服务的日志/信号都能正常工作）。

const { spawnSync } = require("node:child_process");
const { ensureBinary } = require("../lib/index");

ensureBinary()
  .then((binaryPath) => {
    const result = spawnSync(binaryPath, process.argv.slice(2), {
      stdio: "inherit",
      windowsHide: true,
    });
    if (result.error) {
      console.error(`Failed to execute ${binaryPath}:`, result.error);
      process.exit(1);
    }
    process.exit(result.status ?? 1);
  })
  .catch((err) => {
    console.error(err.message);
    process.exit(1);
  });
