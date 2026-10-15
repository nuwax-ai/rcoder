"use strict";

const { join, dirname } = require("node:path");

const PKG = require("../package.json");
const VERSION = PKG.version;
const NAME = "file-server-proxy";
const SUPPORTED_TARGETS = ["aarch64-apple-darwin", "x86_64-apple-darwin",
  "x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl",
  "aarch64-unknown-linux-gnu", "x86_64-pc-windows-msvc"];
function hostTarget() {
  return getTargetTriple(process.platform, process.arch, undefined, false);
}
// 阿里云 OSS 公共读 CDN（与 @nuwax-ai/file-server 同 bucket，前缀不同）。
// 产物路径约定: {base}/v{version}/file-server-proxy-{version}-{target}.{ext}
const OSS_CDN_BASE =
  "https://nuwa-packages.oss-rg-china-mainland.aliyuncs.com/file-server-proxy";

function detectLibcFamily() {
  try {
    return require("detect-libc").familySync();
  } catch {
    return null;
  }
}

// 把宿主 (platform, arch, libc family) 映射到 Rust target triple。
// 可选参数用于测试；Linux ABI 决定是否存在匹配产物。
function getTargetTriple(platform, arch, family, allowOverride = true) {
  if (allowOverride && process.env.FILE_SERVER_PROXY_TARGET) {
    const target = process.env.FILE_SERVER_PROXY_TARGET;
    if (!SUPPORTED_TARGETS.includes(target)) throw new Error(`unsupported_target: ${target}`);
    return target;
  }
  const p = platform || process.platform;
  const a = arch || process.arch;
  if (p === "darwin") {
    if (!["arm64", "x64"].includes(a)) throw new Error(`Unsupported Darwin arch: ${a}`);
    return a === "arm64" ? "aarch64-apple-darwin" : "x86_64-apple-darwin";
  }
  if (p === "linux") {
    if (a === "arm64") {
      const f = family !== undefined ? family : detectLibcFamily();
      if (f === "musl") throw new Error("unsupported_target: Linux ARM64 musl has no published artifact");
      return "aarch64-unknown-linux-gnu";
    }
    if (a === "x64") {
      const f = family !== undefined ? family : detectLibcFamily();
      return f === "musl"
        ? "x86_64-unknown-linux-musl"
        : "x86_64-unknown-linux-gnu";
    }
    throw new Error(`Unsupported Linux arch: ${a}`);
  }
  if (p === "win32") {
    // N09：发布矩阵只有 x86_64-pc-windows-msvc——ARM64 早拒绝（结构化
    // 错误，不下载不存在的包），矩阵扩展后放开
    if (a !== "x64") {
      throw new Error(
        `unsupported_target: Windows ${a === "arm64" ? "ARM64" : a} (${a}) has no published artifact; \
supported: darwin arm64/x64, linux arm64(gnu)/x64(gnu/musl), windows x64`,
      );
    }
    return "x86_64-pc-windows-msvc";
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
  const ext = target.includes("windows") ? "zip" : "tar.gz";
  return `${OSS_CDN_BASE}/v${ver}/${NAME}-${ver}-${target}.${ext}`;
}

// 本包在 node_modules 内的绝对路径。
function packageDir() {
  return dirname(require.resolve("../package.json"));
}

// 缓存目录放在 node_modules 下（本包的同级 .cache），随包安装一起被
// electron-builder 打包；删除 node_modules 即彻底清理。
function cacheDir(version, target = getTargetTriple()) {
  return join(packageDir(), "..", ".cache", NAME, version || VERSION, target);
}

function cachedBinaryPath(version, platform) {
  return join(cacheDir(version), getBinaryName(platform));
}

module.exports = {
  NAME,
  SUPPORTED_TARGETS,
  hostTarget,
  OSS_CDN_BASE,
  VERSION,
  getTargetTriple,
  getArchiveExt,
  getBinaryName,
  downloadUrl,
  cacheDir,
  cachedBinaryPath,
  packageDir,
};
