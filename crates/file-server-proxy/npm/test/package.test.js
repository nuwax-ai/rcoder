"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { createHash } = require("node:crypto");
const { ensureBinary, prepareBinary } = require("../lib/index");
const { hostTarget, cacheDir } = require("../lib/resolve");
const hash = (x) => createHash("sha256").update(x).digest("hex");

function fixture(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "proxy readonly package "));
  const binary = path.join(dir, process.platform === "win32" ? "file-server-proxy.exe" : "file-server-proxy");
  const receipt = { schema: 1, name: "file-server-proxy", version: "1.2.3", target: hostTarget(), archive_sha256: "a".repeat(64), binary_sha256: hash("fixture executable") };
  fs.writeFileSync(binary, "fixture executable");
  fs.writeFileSync(`${binary}.receipt.json`, JSON.stringify(receipt));
  t.after(() => { fs.chmodSync(dir, 0o700); fs.rmSync(dir, { recursive: true, force: true }); });
  return { dir, binary, receipt, options: { version: "1.2.3", directory: dir } };
}

test("startup validates readonly package offline without writing", async (t) => {
  const f = fixture(t);
  const old = global.fetch;
  global.fetch = () => { throw new Error("startup must not fetch"); };
  t.after(() => { global.fetch = old; });
  fs.chmodSync(f.binary, 0o444);
  fs.chmodSync(`${f.binary}.receipt.json`, 0o444);
  fs.chmodSync(f.dir, 0o555);
  const before = fs.readdirSync(f.dir);
  assert.equal(await ensureBinary(f.options), f.binary);
  assert.deepEqual(fs.readdirSync(f.dir), before);
});
test("missing package startup neither downloads nor creates directories", async () => {
  const dir = path.join(os.tmpdir(), `missing-proxy-${process.pid}-${Date.now()}`);
  await assert.rejects(ensureBinary({ directory: dir }), /prepare step/);
  assert.equal(fs.existsSync(dir), false);
});
test("corrupt executable and wrong receipt target/version fail before execution", async (t) => {
  const f = fixture(t);
  fs.writeFileSync(f.binary, "corrupt");
  await assert.rejects(ensureBinary(f.options), /checksum mismatch/);
  fs.writeFileSync(f.binary, "fixture executable");
  fs.writeFileSync(`${f.binary}.receipt.json`, JSON.stringify({ ...f.receipt, target: "wrong" }));
  await assert.rejects(ensureBinary(f.options), /version\/target/);
  fs.writeFileSync(`${f.binary}.receipt.json`, JSON.stringify({ ...f.receipt, version: "0.1.0" }));
  await assert.rejects(ensureBinary(f.options), /version\/target/);
});
test("prepare never overwrites an already published corrupt artifact", async (t) => {
  const f = fixture(t);
  fs.writeFileSync(f.binary, "active executable");
  await assert.rejects(prepareBinary(f.options), /checksum mismatch/);
  assert.equal(fs.readFileSync(f.binary, "utf8"), "active executable");
});
test("cache separates target ABI and rejects unsupported Darwin/Windows architectures", () => {
  assert.notEqual(cacheDir("1.2.3", "x86_64-unknown-linux-gnu"), cacheDir("1.2.3", "x86_64-unknown-linux-musl"));
  const { getTargetTriple } = require("../lib/resolve");
  assert.throws(() => getTargetTriple("darwin", "ia32"), /Unsupported/);
  assert.throws(() => getTargetTriple("win32", "ia32"), /unsupported_target/);
});
test("prepare consumes release manifest, publishes checked receipt and reuses immutable package", async (t) => {
  const { execFileSync } = require("node:child_process");
  const { OSS_CDN_BASE } = require("../lib/resolve");
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "proxy prepare "));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const target = hostTarget();
  const windows = target.includes("windows");
  const name = windows ? "file-server-proxy.exe" : "file-server-proxy";
  const ext = windows ? "zip" : "tar.gz";
  fs.writeFileSync(path.join(root, name), "fixture native artifact");
  const archivePath = path.join(root, `asset.${ext}`);
  if (windows) {
    execFileSync("powershell", ["-NoProfile", "-Command", "Compress-Archive", "-LiteralPath", path.join(root, name), "-DestinationPath", archivePath]);
  } else execFileSync("tar", ["-czf", archivePath, "-C", root, name]);
  const bytes = fs.readFileSync(archivePath);
  const archive = `file-server-proxy-1.2.3-${target}.${ext}`;
  const url = `${OSS_CDN_BASE}/v1.2.3/${archive}`;
  const manifest = { name: "file-server-proxy", version: "1.2.3", targets: { fixture: { rustTarget: target, archive, url, sha256: hash(bytes), size: bytes.length } } };
  const original = global.fetch;
  const calls = [];
  const signals = [];
  global.fetch = async (request, { signal }) => {
    calls.push(request);
    signals.push(signal);
    if (request === `${OSS_CDN_BASE}/manifest/1.2.3.json`) return { ok: true, json: async () => manifest };
    assert.equal(request, url);
    return { ok: true, arrayBuffer: async () => bytes };
  };
  t.after(() => { global.fetch = original; });
  const options = { version: "1.2.3", directory: path.join(root, "published", target) };
  const binary = await prepareBinary(options);
  assert.equal(fs.readFileSync(binary, "utf8"), "fixture native artifact");
  assert.equal(await ensureBinary(options), binary);
  assert.equal(await prepareBinary(options), binary);
  assert.equal(calls.length, 2);
  assert.ok(signals[0] instanceof AbortSignal);
  assert.equal(signals[0], signals[1], "metadata and archive share the same deadline");
  assert.deepEqual(fs.readdirSync(path.dirname(options.directory)), [target]);
  manifest.targets.fixture.sha256 = "0".repeat(64);
  const bad = { ...options, directory: path.join(root, "bad", target) };
  await assert.rejects(prepareBinary(bad), /archive checksum mismatch/);
  assert.equal(fs.existsSync(bad.directory), false);
  assert.deepEqual(fs.readdirSync(path.dirname(bad.directory)), []);
});

test("prepare metadata fetch has a bounded deadline and creates no package on timeout", async (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "proxy timeout "));
  const directory = path.join(root, "never-published");
  const original = global.fetch;
  // Keep a referenced timer: AbortSignal.timeout alone does not hold Node alive.
  const keepAlive = setTimeout(() => {}, 1000);
  t.after(() => { global.fetch = original; clearTimeout(keepAlive); fs.rmSync(root, { recursive: true, force: true }); });
  global.fetch = async (_url, { signal }) => new Promise((_resolve, reject) => {
    assert.ok(signal instanceof AbortSignal);
    signal.addEventListener("abort", () => reject(signal.reason), { once: true });
  });
  await assert.rejects(prepareBinary({ directory, timeoutMs: 20 }), { name: "TimeoutError" });
  assert.equal(fs.existsSync(directory), false);
});
