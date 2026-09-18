"use strict";

const { execFileSync } = require("node:child_process");
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
// N09：参数数组 + execFileSync——路径经 argv 传递不拼进命令字符串（空格/
// 引号/特殊字符路径安全；zip 内路径穿越由 tar 1.x+/Expand-Archive 拒绝
// 绝对路径与 .. 段——归档内容为发布产物单一可执行文件，无目录树）。
function extractArchive(archivePath, destDir, ext) {
  mkdirSync(destDir, { recursive: true });
  if (ext === "zip") {
    execFileSync(
      "powershell",
      [
        "-NoProfile",
        "-Command",
        "Expand-Archive",
        "-Force",
        "-LiteralPath",
        archivePath,
        "-DestinationPath",
        destDir,
      ],
      { stdio: "inherit" },
    );
  } else {
    execFileSync("tar", ["-xzf", archivePath, "-C", destDir], {
      stdio: "inherit",
    });
  }
}

module.exports = { downloadArchive, extractArchive };
