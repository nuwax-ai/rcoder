"use strict";
// PX-03: headless 宿主客户端——宿主进程（Electron main / Node 服务）读取
// native owner root 下 0600 的 credentials.json, 为文件 API 的 HTTP/SSE 请求
// 注入 X-Proxy-Token。凭据只进入宿主进程内存与请求头; 不进 renderer、状态
// 输出、URL、argv、日志或普通事件。
const { readFileSync } = require("node:fs");
const { join } = require("node:path");

function loadCredential(ownerRoot) {
  const raw = readFileSync(join(ownerRoot, "credentials.json"), "utf8");
  const parsed = JSON.parse(raw);
  if (typeof parsed.file_api_token !== "string" || !parsed.file_api_token) {
    throw new Error("credentials.json has no file_api_token");
  }
  return parsed.file_api_token;
}

function authorizedFetch(token) {
  return (url, init = {}) => fetch(url, {
    ...init,
    headers: { ...(init.headers || {}), "X-Proxy-Token": token },
  });
}

// Long-lived SSE: headers arrive, then events stream indefinitely. The helper
// keeps the reader open and yields parsed `event:`/`data:` lines; no total
// timeout is imposed on the stream.
async function readEventSource(url, token, onEvent, signal) {
  const response = await authorizedFetch(token)(url, { signal });
  if (!response.ok || !response.body) throw new Error(`SSE request failed: ${response.status}`);
  const decoder = new TextDecoder();
  let buffer = "";
  let event = "message", data = "";
  for await (const chunk of response.body) {
    buffer += decoder.decode(chunk, { stream: true });
    let index;
    while ((index = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, index).replace(/\r$/, "");
      buffer = buffer.slice(index + 1);
      if (line === "") {
        if (data !== "") onEvent({ event, data });
        event = "message"; data = "";
        continue;
      }
      if (line.startsWith(":")) continue;
      if (line.startsWith("event:")) event = line.slice(6).trim();
      else if (line.startsWith("data:")) data += (data ? "\n" : "") + line.slice(5).trim();
    }
  }
}

module.exports = { loadCredential, authorizedFetch, readEventSource };
