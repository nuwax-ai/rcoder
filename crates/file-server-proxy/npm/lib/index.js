"use strict";

const fs = require("node:fs");
const path = require("node:path");
const { spawnSync } = require("node:child_process");
const { createHash } = require("node:crypto");
const { NAME, VERSION, cacheDir, getTargetTriple, hostTarget, OSS_CDN_BASE } = require("./resolve");
const { downloadArchive, extractArchive } = require("./fetch");
const digest = (bytes) => createHash("sha256").update(bytes).digest("hex");
const receiptPath = (binary) => `${binary}.receipt.json`;
const binaryName = (target) => target.includes("windows") ? `${NAME}.exe` : NAME;

function validateBinary(binary, version, target) {
  const receipt = JSON.parse(fs.readFileSync(receiptPath(binary), "utf8"));
  if (receipt.schema !== 1 || receipt.name !== NAME || receipt.version !== version || receipt.target !== target) {
    throw new Error("incompatible_artifact: binary receipt does not match requested version/target");
  }
  if (!/^[a-f0-9]{64}$/.test(receipt.archive_sha256) ||
      !/^[a-f0-9]{64}$/.test(receipt.binary_sha256) ||
      digest(fs.readFileSync(binary)) !== receipt.binary_sha256) {
    throw new Error("invalid_artifact: binary checksum mismatch");
  }
  return binary;
}

// Startup is strictly read-only/offline. A user-selected local build is probed,
// but is not claimed to be a checksum-verified official distribution.
function resolveBinaryPath(options = {}) {
  const version = options.version || VERSION;
  const target = getTargetTriple();
  if (target !== hostTarget()) throw new Error("incompatible_artifact: target override differs from host");
  const override = process.env.FILE_SERVER_PROXY_BINARY;
  if (override) {
    const probe = spawnSync(override, ["--version"], { encoding: "utf8", timeout: 10000, windowsHide: true });
    if (probe.error || probe.status !== 0 || !/^file-server-proxy \d+\.\d+\.\d+(?:[-+][\w.-]+)?\s*$/.test(probe.stdout || "")) {
      throw new Error("incompatible_artifact: explicit development binary failed its native version probe");
    }
    return override;
  }
  const binary = path.join(options.directory || cacheDir(version, target), binaryName(target));
  try {
    return validateBinary(binary, version, target);
  } catch (error) {
    throw new Error(`file-server-proxy package unavailable or invalid; run the explicit prepare step before startup: ${error.message}`, { cause: error });
  }
}
async function ensureBinary(options = {}) { return resolveBinaryPath(options); }

// Installation/packaging only. Never overwrite an existing version directory:
// concurrent prepares publish one complete directory; losers validate the winner.
async function prepareBinary(options = {}) {
  const version = options.version || VERSION;
  const target = getTargetTriple();
  const dir = options.directory || cacheDir(version, target);
  const binary = path.join(dir, binaryName(target));
  if (fs.existsSync(dir)) return validateBinary(binary, version, target);
  // One network deadline covers both metadata and archive/body reads.
  const signal = AbortSignal.timeout(options.timeoutMs ?? 300000);
  const response = await fetch(`${OSS_CDN_BASE}/manifest/${encodeURIComponent(version)}.json`, { signal });
  if (!response.ok) throw new Error(`manifest download failed: HTTP ${response.status}`);
  const manifest = await response.json();
  const assets = Object.values(manifest.targets || {}).filter((entry) => entry.rustTarget === target);
  if (manifest.name !== NAME || manifest.version !== version || assets.length !== 1) {
    throw new Error("invalid_manifest: name/version/target mismatch");
  }
  const asset = assets[0];
  const ext = target.includes("windows") ? "zip" : "tar.gz";
  const expectedArchive = `${NAME}-${version}-${target}.${ext}`;
  const expectedUrl = `${OSS_CDN_BASE}/v${version}/${expectedArchive}`;
  if (asset.archive !== expectedArchive || asset.url !== expectedUrl ||
      !/^[a-f0-9]{64}$/.test(asset.sha256) || !Number.isSafeInteger(asset.size) || asset.size <= 0) {
    throw new Error("invalid_manifest: archive metadata mismatch");
  }
  fs.mkdirSync(path.dirname(dir), { recursive: true });
  const staging = fs.mkdtempSync(path.join(path.dirname(dir), ".prepare-"));
  try {
    const archive = path.join(staging, `download.${ext}`);
    await downloadArchive(asset.url, archive, signal);
    const bytes = fs.readFileSync(archive);
    if (bytes.length !== asset.size || digest(bytes) !== asset.sha256) throw new Error("invalid_artifact: archive checksum mismatch");
    const unpacked = path.join(staging, "unpacked");
    extractArchive(archive, unpacked, ext);
    const prepared = path.join(unpacked, binaryName(target));
    if (!fs.statSync(prepared).isFile()) throw new Error("invalid_artifact: missing executable");
    const receipt = { schema: 1, name: NAME, version, target,
      archive_sha256: asset.sha256, binary_sha256: digest(fs.readFileSync(prepared)) };
    fs.writeFileSync(receiptPath(prepared), JSON.stringify(receipt));
    if (!target.includes("windows")) fs.chmodSync(prepared, 0o755);
    try { fs.renameSync(unpacked, dir); }
    catch (error) {
      if (!fs.existsSync(dir)) throw error;
      // A published destination is immutable, including when it is corrupt.
      validateBinary(binary, version, target);
    }
    return validateBinary(binary, version, target);
  } finally { fs.rmSync(staging, { recursive: true, force: true }); }
}
module.exports = { ensureBinary, resolveBinaryPath, prepareBinary,
  getTargetTriple, downloadUrl: require("./resolve").downloadUrl, VERSION };
