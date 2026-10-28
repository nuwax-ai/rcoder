"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { execFileSync } = require("node:child_process");
const { extractArchive } = require("../lib/fetch");

test("Windows ZIP extraction treats path characters as literal data", { skip: process.platform !== "win32" }, (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "proxy extract "));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const fixture = path.join(root, "fixture.exe");
  const fixtureArchive = path.join(root, "fixture.zip");
  fs.writeFileSync(fixture, "verified archive fixture");
  execFileSync("powershell", ["-NoProfile", "-NonInteractive", "-Command", "$ErrorActionPreference = 'Stop'; Compress-Archive -LiteralPath $env:NUWAX_TEST_BINARY -DestinationPath $env:NUWAX_TEST_ARCHIVE"], {
    env: { ...process.env, NUWAX_TEST_BINARY: fixture, NUWAX_TEST_ARCHIVE: fixtureArchive },
    stdio: "pipe",
  });
  for (const name of ["plain", "space name", "single'quote", "semi;colon", "amp&ersand", "dollar$sign", "brackets[0]", "中文路径"]) {
    const source = path.join(root, name);
    fs.mkdirSync(source);
    const archive = path.join(source, "archive.zip");
    fs.copyFileSync(fixtureArchive, archive);
    const destination = path.join(source, "extracted");
    extractArchive(archive, destination, "zip");
    assert.equal(fs.readFileSync(path.join(destination, "fixture.exe"), "utf8"), "verified archive fixture", name);
  }
});
