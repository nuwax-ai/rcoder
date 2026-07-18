#!/usr/bin/env node

import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { lstat, readFile, readdir, readlink, writeFile } from "node:fs/promises";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const toolDir = path.dirname(fileURLToPath(import.meta.url));
const runtime = path.join(toolDir, "runtime");
const ports = { ts: Number(process.env.TS_PORT || 61000), rust: Number(process.env.RUST_PORT || 61001) };
const roots = { ts: path.join(runtime, "ts"), rust: path.join(runtime, "rust") };
const report = { startedAt: new Date().toISOString(), cases: [], filesystem: [], git: [], summary: {} };

function stable(value) {
  if (Array.isArray(value)) {
    const values = value.map(stable);
    if (values.every((item) => item && typeof item === "object" && !Array.isArray(item))) {
      return values.sort((a, b) => JSON.stringify(a).localeCompare(JSON.stringify(b)));
    }
    return values;
  }
  if (!value || typeof value !== "object") return value;
  return Object.fromEntries(Object.keys(value).sort().map((key) => [key, stable(value[key])]));
}

function normalizeString(input) {
  let value = input;
  for (const implementation of ["ts", "rust"]) value = value.split(roots[implementation]).join("$SERVICE_ROOT");
  value = value.replace(/\b[0-9a-f]{40}\b/gi, "$GIT_HASH");
  value = value.replace(/\b[0-9a-f]{64}\b/gi, "$HASH");
  return value;
}

const volatileKeys = new Set(["timestamp", "uptime", "pid", "memory", "nodeVersion", "platform", "requestId"]);
function normalize(value) {
  if (typeof value === "string") return normalizeString(value);
  if (Array.isArray(value)) return stable(value.map(normalize));
  if (!value || typeof value !== "object") return value;
  const result = {};
  for (const [key, item] of Object.entries(value)) {
    if (!volatileKeys.has(key)) result[key] = normalize(item);
  }
  return stable(result);
}

async function request(implementation, spec) {
  const body = typeof spec.body === "function" ? spec.body() : spec.body;
  const response = await fetch(`http://127.0.0.1:${ports[implementation]}${spec.path}`, {
    method: spec.method || "GET",
    headers: spec.headers,
    body,
    signal: AbortSignal.timeout(spec.timeout || 30_000),
  });
  const contentType = response.headers.get("content-type") || "";
  const payload = contentType.includes("json") ? await response.json() : await response.text();
  return { status: response.status, contentType: contentType.split(";")[0], payload };
}

async function pair(name, spec, project = normalize) {
  let ts;
  let rust;
  try {
    ts = await request("ts", spec);
    rust = await request("rust", spec);
  } catch (error) {
    const item = { name, equal: false, transportError: String(error?.stack || error) };
    report.cases.push(item);
    console.error(`FAIL ${name}: ${item.transportError}`);
    return;
  }
  const tsComparable = project(ts);
  const rustComparable = project(rust);
  const equal = JSON.stringify(tsComparable) === JSON.stringify(rustComparable);
  report.cases.push({ name, equal, ts, rust, tsComparable, rustComparable });
  console.log(`${equal ? "PASS" : "DIFF"} ${name}`);
}

const json = (value) => ({
  method: "POST",
  headers: { "content-type": "application/json" },
  body: JSON.stringify(value),
});

await pair("health contract", { path: "/health" }, (result) => ({ status: result.status, serviceStatus: result.payload?.status }));
await pair("root", { path: "/" });
await pair("create project validation", { path: "/api/project/create-project", ...json({ templateType: "react" }) });
await pair("create React project", { path: "/api/project/create-project", ...json({ projectId: "parity-page", templateType: "react" }) });
await pair("read project content", { path: "/api/project/get-project-content?projectId=parity-page&proxyPath=%2Fproxy" });
await pair("create Vue project", { path: "/api/project/create-project", ...json({ projectId: "parity-vue", templateType: "vue3" }) });
await pair("read Vue project content", { path: "/api/project/get-project-content?projectId=parity-vue&proxyPath=%2Fproxy" });

await pair("specified file create", {
  path: "/api/project/specified-files-update",
  ...json({
    projectId: "parity-page",
    codeVersion: "1",
    files: [{ operation: "create", name: "src/parity.txt", contents: encodeURIComponent("你好，file-server\n") }],
  }),
});
await pair("page static file", { path: `/api/page/static/parity-page/src/${encodeURIComponent("parity.txt")}` });

await pair("single file upload", {
  path: "/api/project/upload-single-file",
  method: "POST",
  body: () => {
    const form = new FormData();
    form.set("projectId", "parity-page");
    form.set("codeVersion", "2");
    form.set("filePath", "public/single.txt");
    form.set("file", new Blob(["single upload\n"], { type: "text/plain" }), "single.txt");
    return form;
  },
});
await pair("batch file upload", {
  path: "/api/project/upload-batch-files",
  method: "POST",
  body: () => {
    const form = new FormData();
    form.set("projectId", "parity-page");
    form.set("codeVersion", "3");
    form.append("filePaths", "public/batch-a.txt");
    form.append("filePaths", "public/batch-b.txt");
    form.append("files", new Blob(["batch A\n"], { type: "text/plain" }), "a.txt");
    form.append("files", new Blob(["batch B\n"], { type: "text/plain" }), "b.txt");
    return form;
  },
});

await pair("attachment upload", {
  path: "/api/project/upload-attachment-file",
  method: "POST",
  body: () => {
    const form = new FormData();
    form.set("projectId", "parity-page");
    form.set("fileName", "parity-note.txt");
    form.set("file", new Blob(["attachment parity\n"], { type: "text/plain" }), "源文件.txt");
    return form;
  },
});

const hooksConfig = JSON.stringify({
  PreToolUse: [{ matcher: "Bash", hooks: [{ type: "command", command: "echo ok" }] }],
  UserPromptSubmit: [{ hooks: [{ type: "http", url: "https://example.com/hook", timeout: 20 }] }],
});
await pair("computer workspace with hooks", {
  path: "/api/computer/create-workspace-v2",
  method: "POST",
  body: () => {
    const form = new FormData();
    form.set("userId", "parity-user");
    form.set("cId", "parity-computer");
    form.set("mcpServersConfig", JSON.stringify({ filesystem: { command: "npx", args: ["-y", "@fs/mcp"] } }));
    form.set("hooksConfig", hooksConfig);
    form.set("permissionsConfig", JSON.stringify({ allow: ["Bash(echo:*)"], deny: [] }));
    form.set("hookScripts", JSON.stringify([{ path: "hooks/check.sh", content: "#!/usr/bin/env bash\necho check\n" }]));
    return form;
  },
});
await pair("computer files update", {
  path: "/api/computer/files-update",
  ...json({
    userId: "parity-user",
    cId: "parity-computer",
    files: [{ operation: "create", name: "README.md", contents: encodeURIComponent("# parity\n") }],
  }),
});
await pair("computer file list", { path: "/api/computer/get-file-list?userId=parity-user&cId=parity-computer&proxyPath=%2Fproxy" });
await pair("computer static file", { path: "/api/computer/static/parity-user/parity-computer/README.md" });

const gitBase = { workspaceType: "taskAgent", userId: "parity-user", cId: "parity-computer" };
await pair("git init", { path: "/api/git/init", ...json(gitBase) });
await pair("git status before commit", { path: `/api/git/status?${new URLSearchParams(gitBase)}` });
await pair("git commit", {
  path: "/api/git/commit",
  ...json({ ...gitBase, message: "parity commit", files: ["README.md"], authorName: "Parity", authorEmail: "parity@example.com" }),
});
await pair("git status after commit", { path: `/api/git/status?${new URLSearchParams(gitBase)}` });
await pair(
  "git log",
  { path: `/api/git/log?${new URLSearchParams({ ...gitBase, limit: "10" })}` },
  (result) => {
    const comparable = normalize(result);
    for (const commit of comparable.payload?.commits || []) delete commit.date;
    return comparable;
  },
);
await pair("git branch create", { path: "/api/git/branch-create", ...json({ ...gitBase, branchName: "parity-branch" }) });
await pair("git branches", { path: `/api/git/branches?${new URLSearchParams(gitBase)}` });
await pair("git tag create", { path: "/api/git/tag-create", ...json({ ...gitBase, tagName: "parity-v1", message: "parity tag" }) });
await pair("git tags", { path: `/api/git/tags?${new URLSearchParams(gitBase)}` });

const excluded = new Set([".git", "node_modules", ".tmp", "logs", "cache"]);
async function snapshot(root, relative = "") {
  const absolute = path.join(root, relative);
  const entries = await readdir(absolute, { withFileTypes: true });
  const result = [];
  for (const entry of entries.sort((a, b) => a.name.localeCompare(b.name))) {
    if (excluded.has(entry.name)) continue;
    const child = path.join(relative, entry.name);
    const stats = await lstat(path.join(root, child));
    const mode = (stats.mode & 0o777).toString(8).padStart(3, "0");
    if (entry.isDirectory()) {
      result.push({ path: child, type: "dir", mode });
      result.push(...await snapshot(root, child));
    } else if (entry.isSymbolicLink()) {
      result.push({ path: child, type: "symlink", mode, target: normalizeString(await readlink(path.join(root, child))) });
    } else if (entry.isFile()) {
      const bytes = await readFile(path.join(root, child));
      let comparable = bytes;
      if (!bytes.includes(0)) {
        let text = normalizeString(bytes.toString("utf8"))
          .replace(/^# 自动生成于 .*$/m, "# 自动生成于 $GENERATED_AT");
        if (entry.name.endsWith(".json")) {
          try {
            text = `${JSON.stringify(stable(JSON.parse(text)), null, 2)}\n`;
          } catch {
            // 非法或非标准 JSON 按原始文本比较。
          }
        }
        comparable = Buffer.from(text);
      }
      result.push({ path: child, type: "file", mode, size: comparable.length, sha256: createHash("sha256").update(comparable).digest("hex") });
    }
  }
  return result;
}

function acceptedSecureModeDifference(tsSnapshot, rustSnapshot) {
  if (tsSnapshot.length !== rustSnapshot.length) return false;
  const rustByPath = new Map(rustSnapshot.map((entry) => [entry.path, entry]));
  let foundDifference = false;
  for (const tsEntry of tsSnapshot) {
    const rustEntry = rustByPath.get(tsEntry.path);
    if (!rustEntry) return false;
    const { mode: tsMode, ...tsRest } = tsEntry;
    const { mode: rustMode, ...rustRest } = rustEntry;
    if (JSON.stringify(tsRest) !== JSON.stringify(rustRest)) return false;
    if (tsMode !== rustMode) {
      foundDifference = true;
      const relative = tsEntry.path.split("/").slice(2).join("/");
      const isSensitiveConfig = relative === ".claude/settings.json"
        || relative === ".codex/hooks.json"
        || relative === ".mcp.json"
        || relative.startsWith(".opencode/plugins/");
      if (!isSensitiveConfig || tsMode !== "644" || rustMode !== "600") return false;
    }
  }
  return foundDifference;
}

for (const subtree of ["project_workspace", "computer-project-workspace", "project_nginx"]) {
  const ts = await snapshot(path.join(roots.ts, subtree));
  const rust = await snapshot(path.join(roots.rust, subtree));
  const equal = JSON.stringify(ts) === JSON.stringify(rust);
  const accepted = !equal && acceptedSecureModeDifference(ts, rust);
  report.filesystem.push({
    subtree,
    equal,
    accepted,
    acceptedReason: accepted ? "Rust writes sensitive agent configuration with mode 0600 instead of TS mode 0644" : undefined,
    ts,
    rust,
  });
  console.log(`${equal ? "PASS" : accepted ? "ACCEPT" : "DIFF"} filesystem ${subtree}`);
}

function gitSemantic(implementation) {
  const cwd = path.join(roots[implementation], "computer-project-workspace", "parity-user", "parity-computer");
  const run = (...args) => execFileSync("git", ["-C", cwd, ...args], { encoding: "utf8" }).trim();
  return {
    status: run("status", "--porcelain=v1"),
    branches: run("branch", "--format=%(refname:short)"),
    log: run("log", "--format=%s|%an|%ae"),
    trackedFiles: run("ls-files"),
  };
}
try {
  const ts = gitSemantic("ts");
  const rust = gitSemantic("rust");
  const equal = JSON.stringify(ts) === JSON.stringify(rust);
  report.git.push({ name: "computer workspace semantic state", equal, ts, rust });
  console.log(`${equal ? "PASS" : "DIFF"} git semantic state`);
} catch (error) {
  report.git.push({ name: "computer workspace semantic state", equal: false, error: String(error?.stack || error) });
}

const all = [...report.cases, ...report.filesystem, ...report.git];
report.summary = {
  total: all.length,
  passed: all.filter((item) => item.equal).length,
  accepted: all.filter((item) => !item.equal && item.accepted).length,
  different: all.filter((item) => !item.equal && !item.accepted).length,
};
report.finishedAt = new Date().toISOString();
await writeFile(path.join(runtime, "report", "latest.json"), `${JSON.stringify(report, null, 2)}\n`);
console.log(`report: ${path.join(runtime, "report", "latest.json")}`);
console.log(`summary: ${report.summary.passed}/${report.summary.total} equal, ${report.summary.accepted} accepted, ${report.summary.different} differences`);
if (report.summary.different > 0) process.exitCode = 1;
