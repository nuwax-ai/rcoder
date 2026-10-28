"use strict";
// Real embedded file/Git/archive APIs against a fresh, test-owned workspace.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

async function exerciseFiles(address, token, stateDir) {
  const root = fs.mkdtempSync(path.join(stateDir, "files 中文 with spaces-"));
  const context = { workspaceType: "taskAgent", userId: "native-files-user",
    cId: "native-files-case", workspacePath: root };
  const original = "目录 space/中文 文件.txt";
  const renamed = "目录 space/改名 文件.txt";
  const initial = "first 中文 + 100%\n";
  const current = "second 中文 + 100%\n";
  const sentinel = "owned data must survive proxy Stop and Start\n";
  const checks = [];

  async function request(at, key, route, body, binary = false) {
    const response = await fetch(`http://${at}${route}`, {
      method: body === undefined ? "GET" : "POST",
      headers: { "X-Proxy-Token": key, ...(body === undefined ? {} : { "Content-Type": "application/json" }) },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.timeout(15000),
    });
    assert.equal(response.status, 200, `${route}: HTTP ${response.status}`);
    if (binary) {
      assert.match(response.headers.get("content-type") || "", /application\/zip/);
      return Buffer.from(await response.arrayBuffer());
    }
    const result = await response.json();
    assert.equal(result.success, true, `${route} did not succeed`);
    return result;
  }
  const json = (route, body) => request(address, token, route, body);
  async function update(files) {
    const result = await json("/api/computer/files-update", { ...context, files });
    assert.equal(result.filesCount, files.length);
  }
  async function read(filePath, ref = "worktree") {
    return (await json("/api/git/file-content", { ...context, filePath, ref })).content;
  }
  function physical(name) { return path.join(root, ...name.split("/")); }
  function check(name) { checks.push(name); }

  await update([{ operation: "create", name: original, contents: encodeURIComponent(initial) },
    { operation: "create", name: "sentinel.txt", contents: encodeURIComponent(sentinel) },
    { operation: "create", name: "discard.txt", contents: "temporary" }]);
  assert.equal(fs.readFileSync(physical(original), "utf8"), initial);
  assert.equal(await read(original), initial);
  check("Unicode and spaces file create matches real disk and HTTP read");

  const listing = await json("/api/computer/get-file-list?" + new URLSearchParams({ ...context, recursive: "true" }));
  assert(Array.isArray(listing.files) && JSON.stringify(listing.files).includes("中文 文件.txt"));
  const children = await json("/api/computer/fs/children?" + new URLSearchParams({ path: root }));
  assert(children.entries.some(entry => entry.name === "目录 space" && entry.isDir));
  check("file list and filesystem browser use the same native directory");

  await json("/api/git/init", context);
  const committed = await json("/api/git/commit", { ...context, files: [original, "sentinel.txt"],
    message: "native files baseline", authorName: "Native regression", authorEmail: "native@example.invalid" });
  assert.match(committed.commit, /^[0-9a-f]{40,64}$/);
  await update([{ operation: "modify", name: original, contents: encodeURIComponent(current) }]);
  const status = await json("/api/git/status?" + new URLSearchParams(context));
  assert(status.modified.includes(original));
  const diff = await json("/api/git/diff", { ...context, source: "worktree", paths: [original] });
  assert(diff.diff.includes("-first 中文 + 100%") && diff.diff.includes("+second 中文 + 100%"));
  assert.equal(await read(original, "HEAD"), initial);
  assert.equal(await read(original), current);
  check("real Git commit, worktree status, diff, and original HEAD remain consistent");

  await update([{ operation: "rename", name: renamed, renameFrom: original },
    { operation: "delete", name: "discard.txt" }]);
  assert(!fs.existsSync(physical(original)) && !fs.existsSync(physical("discard.txt")));
  assert.equal(fs.readFileSync(physical(renamed), "utf8"), current);
  assert.equal(await read(renamed), current);
  await json("/api/computer/fs/mkdir", { parentPath: root, dirName: "新目录 space" });
  await json("/api/computer/fs/rename", { path: path.join(root, "新目录 space"), newName: "目录 renamed" });
  assert(fs.statSync(path.join(root, "目录 renamed")).isDirectory());
  check("file modify, rename, delete and directory mutation reach the exact owned path");

  const expected = { [renamed]: current, "sentinel.txt": sentinel };
  async function verify(at, key) {
    for (const [name, content] of Object.entries(expected)) {
      assert.equal(fs.readFileSync(physical(name), "utf8"), content);
      const result = await request(at, key, "/api/git/file-content", { ...context, filePath: name, ref: "worktree" });
      assert.equal(result.content, content);
    }
    const head = await request(at, key, "/api/git/file-content", { ...context, filePath: original, ref: "HEAD" });
    assert.equal(head.content, initial);
    const archive = await request(at, key, "/api/computer/zip-workspace", context, true);
    const inspected = spawnSync("python3", ["-c", [
      "import io,json,sys,zipfile",
      "expected=json.loads(sys.argv[1])",
      "with zipfile.ZipFile(io.BytesIO(sys.stdin.buffer.read())) as z:",
      " assert z.testzip() is None, 'archive CRC mismatch'",
      " names=z.namelist()",
      " assert all('\\\\' not in name and not name.startswith('/') for name in names), names",
      " assert not any('.git' in name.split('/') or name=='discard.txt' for name in names), names",
      " for name,content in expected.items(): assert z.read(name).decode('utf-8')==content, name",
      " print(json.dumps({'expected_files':len(expected),'archive_valid':True}))",
    ].join("\n"), JSON.stringify(expected)], { input: archive, encoding: "utf8", timeout: 15000 });
    assert.ifError(inspected.error);
    assert.equal(inspected.status, 0, `real ZIP verification failed: ${inspected.stderr}`);
    assert.equal(JSON.parse(inspected.stdout).archive_valid, true);
  }
  await verify(address, token);
  check("real ZIP preserves UTF-8 names, exact bytes and CRC without Git or deleted files");
  console.log("E2E_NATIVE_FILES_OK", JSON.stringify({ checks, preserved_files: Object.keys(expected) }));
  return { verify };
}

module.exports = { exerciseFiles };
