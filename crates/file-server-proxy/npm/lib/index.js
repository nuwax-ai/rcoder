"use strict";

const { existsSync, mkdirSync, chmodSync, unlinkSync } = require("node:fs");
const { join } = require("node:path");
const {
  NAME,
  VERSION,
  cacheDir,
  cachedBinaryPath,
  downloadUrl,
  getArchiveExt,
} = require("./resolve");
const { downloadArchive, extractArchive } = require("./fetch");

// 用户可用环境变量指向自带/自定义二进制，跳过下载（调试或离线分发；
// 本地开发冒烟也用它指向 cargo build 产物）。
function overrideBinary() {
  return process.env.FILE_SERVER_PROXY_BINARY || "";
}

// 同步：若二进制已就绪（环境变量覆盖或已缓存）返回路径，否则返回 null。不触网。
function resolveBinaryPath() {
  const override = overrideBinary();
  if (override) return override;
  const cached = cachedBinaryPath();
  return existsSync(cached) ? cached : null;
}

// 异步：确保二进制存在（缺失则从 OSS 下载解压），返回其路径。
async function ensureBinary() {
  const override = overrideBinary();
  if (override) return override;

  const cached = cachedBinaryPath();
  if (existsSync(cached)) return cached;

  const url = downloadUrl();
  const dir = cacheDir();
  mkdirSync(dir, { recursive: true });

  const ext = getArchiveExt();
  const archivePath = join(dir, `${NAME}.${ext}`);

  process.stderr.write(
    `Downloading file-server-proxy ${VERSION} for ${process.platform}/${process.arch} …\n  ${url}\n`,
  );
  await downloadArchive(url, archivePath);

  process.stderr.write("  Extracting …\n");
  extractArchive(archivePath, dir, ext);

  try {
    unlinkSync(archivePath);
  } catch {
    /* best-effort */
  }

  const out = cachedBinaryPath();
  if (!existsSync(out)) {
    throw new Error(
      `file-server-proxy extraction finished but binary not found at ${out}`,
    );
  }
  if (process.platform !== "win32") {
    chmodSync(out, 0o755);
  }
  return out;
}

module.exports = {
  ensureBinary,
  resolveBinaryPath,
  getTargetTriple: require("./resolve").getTargetTriple,
  downloadUrl: require("./resolve").downloadUrl,
  VERSION,
};
