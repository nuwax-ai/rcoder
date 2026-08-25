"use strict";

const { execSync } = require("node:child_process");
const { mkdirSync, writeFileSync } = require("node:fs");
const { dirname } = require("node:path");

// 流式下载归档到磁盘。返回字节数。
async function downloadArchive(url, outPath) {
  mkdirSync(dirname(outPath), { recursive: true });
  const res = await fetch(url, { redirect: "follow" });
  if (!res.ok) {
    throw new Error(
      `file-server-proxy download failed: HTTP ${res.status} ${res.statusText}\nURL: ${url}`,
    );
  }
  const buf = Buffer.from(await res.arrayBuffer());
  writeFileSync(outPath, buf);
  return buf.length;
}

// 用系统工具解压：tar.gz 用 tar（mac/linux/Win10+ 自带），zip 用 PowerShell Expand-Archive。
function extractArchive(archivePath, destDir, ext) {
  mkdirSync(destDir, { recursive: true });
  if (ext === "zip") {
    execSync(
      `powershell -NoProfile -Command "Expand-Archive -Force -LiteralPath '${archivePath}' -DestinationPath '${destDir}'"`,
      { stdio: "inherit" },
    );
  } else {
    execSync(`tar -xzf "${archivePath}" -C "${destDir}"`, {
      stdio: "inherit",
    });
  }
}

module.exports = { downloadArchive, extractArchive };
