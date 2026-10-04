"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs"), os = require("node:os"), path = require("node:path");
const hostClient = require("../lib/host-client");

function credentialDir(token) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "proxy-cred-"));
  fs.writeFileSync(path.join(dir, "credentials.json"), JSON.stringify({ file_api_token: token }));
  return dir;
}

test("loadCredential reads the scoped file API token from the owner root", () => {
  const dir = credentialDir("secret-token-value");
  assert.equal(hostClient.loadCredential(dir), "secret-token-value");
  fs.rmSync(dir, { recursive: true, force: true });
});

test("loadCredential rejects credentials without a token", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "proxy-cred-"));
  fs.writeFileSync(path.join(dir, "credentials.json"), "{}");
  assert.throws(() => hostClient.loadCredential(dir), /no file_api_token/);
  fs.rmSync(dir, { recursive: true, force: true });
});

test("authorizedFetch injects the token as a hop header without leaking into URLs", async () => {
  const call = hostClient.authorizedFetch("tok-1");
  let captured;
  globalThis.fetch = async (url, init) => { captured = { url: String(url), init }; return new Response("{}"); };
  try {
    await call("http://127.0.0.1:60000/api/v1/userapp/get-file-list?app_id=a");
    assert.equal(captured.init.headers["X-Proxy-Token"], "tok-1");
    assert.ok(!captured.url.includes("tok-1"), "token must never appear in the URL");
  } finally {
    delete globalThis.fetch;
  }
});

test("readEventSource parses SSE events and keeps the stream open-ended", async () => {
  const events = [];
  const encoder = new TextEncoder();
  const body = (async function* () {
    yield encoder.encode("event: log\ndata: {\"line\":\"one\"}\n\n");
    yield encoder.encode("data: {\"line\":\"two\"}\n\n: keep-alive\n");
  })();
  globalThis.fetch = async () => new Response(body, { status: 200 });
  try {
    await hostClient.readEventSource("http://127.0.0.1:60000/stream", "tok", (event) => events.push(event));
    assert.deepEqual(events, [
      { event: "log", data: "{\"line\":\"one\"}" },
      { event: "message", data: "{\"line\":\"two\"}" },
    ]);
  } finally {
    delete globalThis.fetch;
  }
});
