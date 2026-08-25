"use strict";

const test = require("node:test");
const assert = require("node:assert");
const {
  getTargetTriple,
  getArchiveExt,
  getBinaryName,
  downloadUrl,
} = require("../lib/resolve");

// 清掉环境覆盖，避免污染断言。
function clearOverride() {
  delete process.env.FILE_SERVER_PROXY_TARGET;
}
clearOverride();

test("darwin maps to apple-darwin triples", () => {
  assert.strictEqual(getTargetTriple("darwin", "arm64"), "aarch64-apple-darwin");
  assert.strictEqual(getTargetTriple("darwin", "x64"), "x86_64-apple-darwin");
});

test("linux x64 splits gnu/musl by libc family", () => {
  assert.strictEqual(
    getTargetTriple("linux", "x64", "gnu"),
    "x86_64-unknown-linux-gnu",
  );
  assert.strictEqual(
    getTargetTriple("linux", "x64", "musl"),
    "x86_64-unknown-linux-musl",
  );
});

test("linux arm64 supported (gnu)", () => {
  assert.strictEqual(
    getTargetTriple("linux", "arm64"),
    "aarch64-unknown-linux-gnu",
  );
});

test("win32 maps to msvc triples", () => {
  assert.strictEqual(getTargetTriple("win32", "x64"), "x86_64-pc-windows-msvc");
  assert.strictEqual(
    getTargetTriple("win32", "arm64"),
    "aarch64-pc-windows-msvc",
  );
});

test("archive ext and binary name per platform", () => {
  assert.strictEqual(getArchiveExt("linux"), "tar.gz");
  assert.strictEqual(getArchiveExt("darwin"), "tar.gz");
  assert.strictEqual(getArchiveExt("win32"), "zip");
  assert.strictEqual(getBinaryName("linux"), "file-server-proxy");
  assert.strictEqual(getBinaryName("win32"), "file-server-proxy.exe");
});

test("downloadUrl follows OSS path convention", () => {
  const url = downloadUrl("1.2.3", "linux", "x64", "gnu");
  assert.strictEqual(
    url,
    "https://nuwa-packages.oss-rg-china-mainland.aliyuncs.com/file-server-proxy/v1.2.3/file-server-proxy-1.2.3-x86_64-unknown-linux-gnu.tar.gz",
  );
});

test("FILE_SERVER_PROXY_TARGET override wins over host detection", () => {
  process.env.FILE_SERVER_PROXY_TARGET = "aarch64-unknown-linux-gnu";
  assert.strictEqual(
    getTargetTriple("darwin", "arm64"),
    "aarch64-unknown-linux-gnu",
  );
  clearOverride();
});

test("unsupported platform/arch throws", () => {
  assert.throws(() => getTargetTriple("aix", "x64"), /Unsupported platform/);
  assert.throws(() => getTargetTriple("linux", "riscv64"), /Unsupported Linux arch/);
});
