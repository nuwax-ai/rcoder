# 多服务文件日志

应用框架负责写文件和轮转，app-cli 只读。模板将 `APP_LOG_DIR` 作为目录，建议 JSONL、
按天或 100 MiB 轮转、保留 14 天。

每个 source 的 glob 相对于 `/app/logs/<service_id>`，禁止绝对路径、`..` 和符号链接。

外部 API：

```text
POST /api/v1/userapp/{app_id}/logs/sources/query
POST /api/v1/userapp/{app_id}/logs/query
POST /api/v1/userapp/{app_id}/logs/stream
```

内部 API 使用相同 body，路径为 `/v1/logs/...`。`sources/query` 与 `query` 的响应为
`HttpResult` 信封 `{code, message, data, tid, success}`（rcoder 透明转发，信封直达；
data = 日志源列表 / 日志快照对象 `logs`/`source_errors`/`cursor`/`cursor_reset`）；
流接口返回 SSE（豁免信封），浏览器必须使用 `fetch + ReadableStream`。

```json
{
  "selectors": [{"service_id":"backend-go","source_ids":["application"]}],
  "levels": ["WARN", "ERROR"],
  "keyword": "timeout",
  "since": "2026-07-29T10:00:00+08:00",
  "until": "2026-07-29T12:00:00+08:00",
  "tail": 100,
  "cursor": null
}
```

selectors 为空表示全部 enabled 服务和 source；tail 按 source 计算；cursor 优先于 tail。
非法 selector 整体 400，合法 source 的文件故障不会阻断其它 source，而是返回
`source_error`。SSE 事件包括 `log`、`source_error`、`cursor_reset`、`checkpoint` 和
`heartbeat`；断线后把 checkpoint cursor 放入下一次 POST body。

限制：64 服务、128 source、每 source tail 10,000、keyword 256 字节、cursor 64 KiB、
每 source 最多匹配 128 文件、单行 1 MiB。
