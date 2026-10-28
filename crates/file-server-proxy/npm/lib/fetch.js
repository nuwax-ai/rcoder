"use strict";

const { execFileSync } = require("node:child_process");
const { mkdirSync, writeFileSync } = require("node:fs");
const { dirname } = require("node:path");

// 流式下载归档到磁盘。返回字节数。
async function downloadArchive(url, outPath, signal = AbortSignal.timeout(300000)) {
  mkdirSync(dirname(outPath), { recursive: true });
  const res = await fetch(url, { redirect: "follow", signal });
  if (!res.ok) {
    throw new Error(
      `file-server-proxy download failed: HTTP ${res.status} ${res.statusText}\nURL: ${url}`,
    );
  }
  const buf = Buffer.from(await res.arrayBuffer());
  writeFileSync(outPath, buf);
  return buf.length;
}

// 用系统工具解压：tar.gz 用 tar，zip 用 Windows .NET 的 ZIP API。
// PowerShell -Command reparses its arguments, even with execFileSync. Keep
// the script fixed and pass paths through the environment so spaces, quotes
// and shell metacharacters remain literal data. Expand-Archive also interprets
// brackets in its destination internally; the .NET API keeps both paths literal.
function extractArchive(archivePath, destDir, ext) {
  mkdirSync(destDir, { recursive: true });
  if (ext === "zip") {
    execFileSync(
      "powershell",
      [
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "$ErrorActionPreference = 'Stop'; Add-Type -AssemblyName System.IO.Compression.FileSystem; [System.IO.Compression.ZipFile]::ExtractToDirectory($env:NUWAX_PROXY_ARCHIVE, $env:NUWAX_PROXY_DESTINATION)",
      ],
      {
        stdio: "inherit",
        env: {
          ...process.env,
          NUWAX_PROXY_ARCHIVE: archivePath,
          NUWAX_PROXY_DESTINATION: destDir,
        },
      },
    );
  } else {
    execFileSync("tar", ["-xzf", archivePath, "-C", destDir], {
      stdio: "inherit",
    });
  }
}

module.exports = { downloadArchive, extractArchive };
