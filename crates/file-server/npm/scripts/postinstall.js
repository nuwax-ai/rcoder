#!/usr/bin/env node
"use strict";

// postinstall：npm install 时按宿主平台预下载二进制到 node_modules/.cache/...，
// 便于 electron-builder 随包打包、终端用户离线运行。永不阻断安装——下载失败则留待首次运行再下。

const { ensureBinary } = require("../lib/index");

if (
  process.env.FILE_SERVER_SKIP_DOWNLOAD === "1" ||
  process.env.npm_config_ignore_scripts === "true"
) {
  process.exit(0);
}

ensureBinary()
  .then(() => {
    process.stderr.write(
      `✓ file-server ${require("../package.json").version} ready.\n`,
    );
  })
  .catch((err) => {
    process.stderr.write(`⚠ file-server pre-download skipped: ${err.message}\n`);
    process.stderr.write(
      `  The binary will be downloaded on first use instead.\n`,
    );
  });
