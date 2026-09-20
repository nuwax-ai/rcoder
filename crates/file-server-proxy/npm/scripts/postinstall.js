#!/usr/bin/env node
"use strict";

// postinstall：npm install 时按宿主平台预下载二进制到 node_modules/.cache/...，
// 便于 electron-builder 随包打包、终端用户离线运行。下载失败明确失败；运行阶段禁止补下载。

const { prepareBinary } = require("../lib/index");

if (
  process.env.FILE_SERVER_PROXY_SKIP_DOWNLOAD === "1" ||
  process.env.npm_config_ignore_scripts === "true"
) {
  process.exit(0);
}

prepareBinary()
  .then(() => {
    process.stderr.write(
      `✓ file-server-proxy ${require("../package.json").version} ready.\n`,
    );
  })
  .catch((err) => {
    process.stderr.write(
      `⚠ file-server-proxy prepare failed: ${err.message}\n`,
    );
    process.stderr.write(
      `  Run the explicit prepare step before packaging or startup.\n`,
    );
    process.exitCode = 1;
  });
