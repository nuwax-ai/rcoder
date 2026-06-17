#!/usr/bin/env node
"use strict";

const { platform, arch } = process;
const { execFileSync } = require("child_process");
const path = require("path");

const platformPackages = {
  "linux-x64": "rcoder-cli-linux-x64",
  "linux-arm64": "rcoder-cli-linux-arm64",
  "darwin-x64": "rcoder-cli-darwin-x64",
  "darwin-arm64": "rcoder-cli-darwin-arm64",
  "win32-x64": "rcoder-cli-win32-x64",
};

const key = `${platform}-${arch}`;
const pkgName = platformPackages[key];

if (!pkgName) {
  console.error(`error: unsupported platform: ${key}`);
  console.error(`supported platforms: ${Object.keys(platformPackages).join(", ")}`);
  process.exit(1);
}

const binName = platform === "win32" ? "rcoder-cli.exe" : "rcoder-cli";

try {
  const pkgDir = path.dirname(require.resolve(`${pkgName}/package.json`));
  const binPath = path.join(pkgDir, binName);
  execFileSync(binPath, process.argv.slice(2), { stdio: "inherit" });
} catch (e) {
  if (e.code === "MODULE_NOT_FOUND") {
    console.error(`error: platform package ${pkgName} not found.`);
    console.error(`please reinstall: npm install -g rcoder-cli`);
  } else {
    console.error(`error: ${e.message}`);
  }
  process.exit(1);
}
