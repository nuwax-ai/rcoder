# 上传与 skill URL 安全配置

file-server 的 multipart 文件不会聚合到内存。文件字段逐块写入 `UPLOAD_PROJECT_DIR/temp` 下的排他临时文件，超过接口上限立即失败；请求取消、解析失败和正常完成后均由 `TempPath` 自动清理。multipart 文本字段另有 1 MiB 固定上限。

所有客户端上传入口统一使用 `UPLOAD_MAX_FILE_SIZE_BYTES`，默认且硬上限为 `1073741824` 字节（1 GiB）。可以通过环境变量调低，但不能调高；非法配置会在启动阶段失败。已废弃的 `UPLOAD_SINGLE_FILE_SIZE_BYTES` 不再读取，避免单文件、批量文件、附件和 ZIP 接口出现不同上限。批量上传中的每个文件都执行该限制。

Axum 全局请求体通过 `REQUEST_BODY_MAX_BYTES` 或兼容变量 `REQUEST_BODY_LIMIT` 配置，默认且硬上限同样为 1 GiB。multipart 边界和文本字段也计入请求体，因此单个文件的实际可用大小会略低于 1 GiB。任一上传文件仍会在流式写盘过程中执行独立大小检查。

项目 ZIP、Computer 导入、模板、skill ZIP、单文件、批量文件和附件接口均直接消费临时文件路径。ZIP 下载响应也从临时文件按 64 KiB 分块发送，不再读取整个 ZIP 到内存。

ZIP 解压同时限制为最多 100,000 个条目、单个解压文件最多 1 GiB、累计解压内容最多 4 GiB。限制按实际解压字节二次计算，不只依赖 ZIP central directory 声明值，用于阻止压缩炸弹耗尽 Pod 临时盘。

项目导出 ZIP 也使用 64 KiB 分块响应。Computer、dev-server 和构建失败日志采用有界读取，`LOG_READ_MAX_BYTES` 默认 64 MiB，防止长期运行后超大日志被一次性载入内存。

## skill URL 默认策略

- 默认允许明文 HTTP 与私网/保留地址：本产品以私有化部署为主，skill 通常托管在内网（`10.x`/`192.168.x`/集群内域名）且多为 HTTP。公网或互联网暴露的部署应设置 `SKILL_URL_ALLOW_HTTP=false` 与 `SKILL_URL_ALLOW_PRIVATE_NETWORKS=false` 重新锁紧。
- 禁止 URL 用户名和密码。
- DNS 在 reqwest 实际连接阶段通过安全 resolver 解析（防 DNS rebinding）；当关闭私网放行（`SKILL_URL_ALLOW_PRIVATE_NETWORKS=false`）时，resolver 还会拒绝任一私网、回环、链路本地、组播或保留地址。
- 禁用 reqwest 自动重定向；每一跳重新执行 URL、域名和 DNS 策略检查。
- 使用进程级共享 `reqwest::Client`，配置连接超时和请求总超时。
- 同时检查 `Content-Length` 和实际流式接收字节数。
- 限制单次请求的 URL 数量。

配置项：

| 环境变量 | 默认值 | 说明 |
| --- | ---: | --- |
| `SKILL_DOWNLOAD_MAX_BYTES` | `104857600` | 单个远程 skill ZIP 最大字节数 |
| `SKILL_DOWNLOAD_CONNECT_TIMEOUT_SECS` | `10` | 建连超时 |
| `SKILL_DOWNLOAD_TIMEOUT_SECS` | `60` | 单次请求总超时 |
| `SKILL_DOWNLOAD_MAX_REDIRECTS` | `3` | 最大重定向跳数 |
| `SKILL_URL_MAX_COUNT` | `20` | 单次接口最大 URL 数量 |
| `SKILL_URL_ALLOW_HTTP` | `true` | 是否允许明文 HTTP；公网部署建议设为 `false` |
| `SKILL_URL_ALLOW_PRIVATE_NETWORKS` | `true` | 是否允许私网/保留地址；公网/互联网暴露部署建议设为 `false` |
| `SKILL_URL_ALLOWED_HOSTS` | 空 | 可选域名白名单，逗号分隔；允许该域名及其子域名 |

K8s 部署时应给 `UPLOAD_PROJECT_DIR/temp` 所在卷设置足够的 ephemeral-storage request/limit，并监控临时盘使用量。内存上限不再决定大文件上传能力，但磁盘配额仍应与上传上限及并发量匹配。
