"use strict";

const { join, dirname } = require("node:path");

const PKG = require("../package.json");
const VERSION = PKG.version;
const NAME = "file-server";
// 阿里云 OSS 公共读 CDN（与 nuwax-codex 同 bucket，前缀换成 file-server）。
// 产物路径约定: {base}/v{version}/file-server-{version}-{target}.{ext}
const OSS_CDN_BASE =
  "https://nuwa-packages.oss-rg-china-mainland.aliyuncs.com/file-server";

function detectLibcFamily() {
  try {
    return require("detect-libc").familySync();
  } catch {
    return null;
  }
}

// 把宿主 (platform, arch, libc family) 映射到 Rust target triple。
// 可选参数用于测试；family 仅 linux x64 需要区分 gnu/musl。
function getTargetTriple(platform, arch, family) {
  if (process.env.FILE_SERVER_TARGET) {
    return process.env.FILE_SERVER_TARGET;
  }
  const p = platform || process.platform;
  const a = arch || process.arch;
  if (p === "darwin") {
    return a === "arm64" ? "aarch64-apple-darwin" : "x86_64-apple-darwin";
  }
  if (p === "linux") {
    if (a === "arm64") return "aarch64-unknown-linux-gnu";
    if (a === "x64") {
      const f = family !== undefined ? family : detectLibcFamily();
      return f === "musl"
        ? "x86_64-unknown-linux-musl"
        : "x86_64-unknown-linux-gnu";
    }
    throw new Error(`Unsupported Linux arch: ${a}`);
  }
  if (p === "win32") {
    return a === "arm64" ? "aarch64-pc-windows-msvc" : "x86_64-pc-windows-msvc";
  }
  throw new Error(`Unsupported platform: ${p}`);
}

function getArchiveExt(platform) {
  return (platform || process.platform) === "win32" ? "zip" : "tar.gz";
}

function getBinaryName(platform) {
  return (platform || process.platform) === "win32"
    ? `${NAME}.exe`
    : NAME;
}

function downloadUrl(version, platform, arch, family) {
  const ver = version || VERSION;
  const target = getTargetTriple(platform, arch, family);
  const ext = getArchiveExt(platform);
  return `${OSS_CDN_BASE}/v${ver}/${NAME}-${ver}-${target}.${ext}`;
}

// 本包在 node_modules 内的绝对路径。
function packageDir() {
  return dirname(require.resolve("../package.json"));
}

// 缓存目录放在 node_modules 下（本包的同级 .cache），随包安装一起被
// electron-builder 打包；删除 node_modules 即彻底清理。
function cacheDir(version) {
  return join(packageDir(), "..", ".cache", NAME, version || VERSION);
}

function cachedBinaryPath(version, platform) {
  return join(cacheDir(version), getBinaryName(platform));
}

module.exports = {
  NAME,
  VERSION,
  OSS_CDN_BASE,
  getTargetTriple,
  getArchiveExt,
  getBinaryName,
  downloadUrl,
  cacheDir,
  cachedBinaryPath,
  packageDir,
};
