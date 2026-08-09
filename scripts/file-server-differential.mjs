#!/usr/bin/env node

const tsBase = process.env.TS_FILE_SERVER_URL || "http://127.0.0.1:60000";
const rustBase = process.env.RUST_FILE_SERVER_URL || "http://127.0.0.1:60001";
const missingId = `file-server-diff-missing-${process.pid}`;

const probes = [
  {
    name: "root greeting",
    path: "/",
    select: ({ status, body, contentType }) => ({
      status,
      text: body.__nonJsonBody,
      contentType,
    }),
  },
  {
    name: "health contract",
    path: "/health",
    select: ({ status, body }) => ({
      status,
      statusValue: body.status,
      fields: Object.keys(body).sort(),
      memoryFields: Object.keys(body.memory ?? {}).sort(),
      fieldTypes: {
        timestamp: typeof body.timestamp,
        uptime: typeof body.uptime,
        version: typeof body.version,
        platform: typeof body.platform,
        nodeVersion: typeof body.nodeVersion,
        pid: typeof body.pid,
        memory: typeof body.memory,
        env: typeof body.env,
      },
    }),
  },
  {
    name: "not-found error protocol",
    path: "/__file_server_diff_missing__",
    select: errorContract,
  },
  {
    name: "malformed JSON rejection",
    path: "/api/project/create-project",
    init: {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: "{invalid",
    },
    select: errorContract,
    allowDifference: (ts, rust) =>
      ts.status === 500 &&
      ts.type === "SYSTEM_ERROR" &&
      rust.status === 400 &&
      rust.type === "VALIDATION_ERROR",
  },
  {
    name: "project validation",
    path: "/api/project/create-project",
    init: {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ projectId: "", templateType: "react" }),
    },
    select: errorContract,
  },
  {
    name: "missing Git workspace",
    path: `/api/git/branches?workspaceType=pageApp&projectId=${missingId}`,
    select: errorContract,
  },
  {
    name: "build error parser",
    path: "/api/build/parse-build-error",
    init: {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        projectId: missingId,
        errorMessage: "Error: Cannot find module 'react-dom'",
      }),
    },
    select: ({ status, body }) => ({ status, success: body.success, message: body.message }),
  },
];

function errorContract({ status, body }) {
  return {
    status,
    success: body?.success,
    code: body?.code,
    type: body?.error?.type,
  };
}

async function request(base, probe) {
  const response = await fetch(`${base}${probe.path}`, {
    signal: AbortSignal.timeout(10_000),
    ...probe.init,
  });
  const text = await response.text();
  let body;
  try {
    body = JSON.parse(text);
  } catch {
    body = { __nonJsonBody: text };
  }
  return {
    status: response.status,
    body,
    contentType: response.headers.get("content-type"),
  };
}

let differences = 0;
for (const probe of probes) {
  let ts;
  let rust;
  try {
    [ts, rust] = await Promise.all([request(tsBase, probe), request(rustBase, probe)]);
  } catch (error) {
    console.error(`[ERROR] ${probe.name}: ${error.message}`);
    differences += 1;
    continue;
  }
  const tsSelected = probe.select(ts);
  const rustSelected = probe.select(rust);
  if (JSON.stringify(tsSelected) === JSON.stringify(rustSelected)) {
    console.log(`[PASS] ${probe.name}`);
    continue;
  }
  if (probe.allowDifference?.(tsSelected, rustSelected)) {
    console.log(`[IMPROVED] ${probe.name}: Rust returns typed client validation error`);
    continue;
  }
  differences += 1;
  console.error(`[DIFF] ${probe.name}`);
  console.error(`  TS:   ${JSON.stringify(tsSelected)}`);
  console.error(`  Rust: ${JSON.stringify(rustSelected)}`);
}

if (differences > 0) {
  console.error(`\n${differences} differential probe(s) failed.`);
  process.exitCode = 1;
} else {
  console.log(`\nAll ${probes.length} differential probes passed.`);
}
