# UserApp 项目与任务诊断接入

`POST /api/v1/userapp/build`、`POST /api/v1/userapp/dev/start` 和 `POST /api/v1/userapp/dev/restart` 在项目预检查失败时立即返回失败，同时提供可查询的终态诊断任务。调用方可以继续使用现有任务查询和 SSE 日志接口展示原因，不必等待构建 worker 启动。

预检查覆盖平台源码根、根 `workspace.manifest.toml`、一级服务目录的 `project.manifest.toml`、配置语法和语义。dev 启动/重启已有的 owner 预检查失败也进入同一诊断出口。纯 `/build` 不增加对运行 owner 可用性的依赖。

## 响应合同

容器 UserApp 接口沿用 **HTTP 200 + `HttpResult` 信封**。业务是否成功由 `success` 判断，不能仅检查 HTTP 状态。传输、鉴权及上游定位失败仍需按各自的 HTTP/错误合同处理。

- `success=true`：正常创建异步任务，按返回的 `task_id` 查询后续状态。
- `success=false`：保留原有 `code`、`message`；如已确认应用作用域，`data` 可携带真实的失败任务 ID 和诊断。
- 参数不合法、应用归属不匹配或权限拒绝：保留原错误，不创建不可查询的诊断任务。
- 任务表容量耗尽：`data.task_id=null`，并返回 `task_capacity` 诊断；不存在可供查询的任务，调用方不能自行生成 ID。
- worker 注册失败时已经创建的任务返回**原任务 ID**，不会另建一个替代任务。

失败数据的示意结构如下；路径和 ID 以实际响应为准：

```json
{
  "code": "ERR_WORKSPACE_NO_SERVICES",
  "message": "workspace configuration validation failed",
  "success": false,
  "data": {
    "task_id": "<服务端返回的真实任务 ID>",
    "status": "failed",
    "diagnostics": [
      {
        "code": "manifest_validation",
        "phase": "precheck",
        "repair_target": "project",
        "scope": "service",
        "workspace_root": "<平台源码根>",
        "detected_workspace_root": null,
        "file": "frontend/project.manifest.toml",
        "field": "build.artifact",
        "service_id": "frontend",
        "message": "build.artifact must be a safe relative path: /错误产物路径",
        "hint": "将产物路径改为服务目录内的相对路径。"
      }
    ]
  }
}
```

示例省略了信封的 `tid`。预检诊断任务直接以 `failed` 状态注册，不经过 Pending 队列，不占用构建名额或工作区执行租约，也不取消同应用的旧构建。若原执行任务上的显式 Stop/Cancel 已先提交，其错误响应中的 `status` 可为 `cancelled`；以真实任务快照为准。

`RuntimeRecovery` 错误原有的 `supervisor_id`、`generation`、`operation_id`、`phase`、`problem` 等数据保持在 `data` 顶层，并保留在可选的 `data.recovery` 中。`data.phase` 是原恢复状态，`diagnostics[].phase` 是诊断发生阶段，两者不能混用。运行操作 ID 与构建任务 ID 也不能互相替代。

## 诊断字段

共享合同为 [`UserAppDiagnostic` / `UserAppTaskFailureData`](../crates/shared_types/src/userapp/diagnostic.rs)。任务快照的 `diagnostics` 保留同类结构；单次最多保存 32 条诊断，消息和修复提示有长度上限，超出时以省略号 `…` 标明截断；不提供完整配置文件或环境变量集合。

| 字段 | 含义 |
| --- | --- |
| `code` | 稳定的机器可读原因，供程序分支；不要解析英文或中文 message 来判断类型 |
| `phase` | 失败发生阶段，独立于任务类型 `build` / `dev_start` / `dev_restart` |
| `repair_target` | `project` 表示项目源码/配置/命令问题；`platform` 表示运行配置、管理恢复、权限、容量等平台问题 |
| `scope` | `task` 为整体任务问题；`service` 为有明确服务身份的问题 |
| `workspace_root` | 已知的平台源码根，仅为诊断位置，不是执行或资源操作授权 |
| `detected_workspace_root` | 检测到的错误项目根，例如源码被多套在 `code/` 下；不会自动采用或搬迁 |
| `file`、`field` | 已知的配置文件和字段；无法确定时为 `null`，不会靠错误文本推测 |
| `service_id` | 真实服务 ID，不是显示名称或目录名；任务级问题为 `null` |
| `message`、`hint` | 可展示的原因和修复建议，调用方应保留 Unicode 内容 |

阶段取值：

| `phase` | 发生位置 |
| --- | --- |
| `precheck` | 源码根、manifest 读取、解析和配置校验 |
| `owner_preflight` | dev 构建前的运行所有者和管理恢复检查 |
| `admission` | 任务注册、worker 受理或等待构建名额 |
| `build` | 已取得构建名额后的构建准备、编译、产物校验与打包 |
| `start` | 构建后的运行激活、服务启动和启动结果确认 |

当前诊断码包括 `workspace_empty`、`workspace_root_mismatch`、`workspace_manifest_missing`、`workspace_io`、`no_services`、`manifest_parse`、`manifest_validation`、`owner_preflight`、`task_capacity`、`worker_admission`、`build_failed`、`start_failed`。界面遇到新增码时仍应展示 `message` 和 `hint`，不要丢弃整条失败数据。

OpenAPI 的构建响应使用 `HttpResult<BuildAdmissionData>`，启动/重启响应使用 `HttpResult<DevAdmissionData>`；各自包含成功载荷和 `UserAppTaskFailureData` 两种数据形态。任务查询仍使用 `HttpResult<BuildTaskSnapshot>`。注册入口见 [UserApp 路由与文档](../crates/file-server-userapp/src/routes.rs)。

## 获取失败任务并查询日志

接入步骤：

1. 先读取 `success`、`code` 和 `message`。
2. 无论成功还是失败，都保留响应中的真实 `data.task_id`；失败时同时展示并保存 `data.diagnostics` 和恢复信息。
3. ID 非空时查询任务或订阅 SSE。预检任务已是终态，仍可完整回放日志。
4. ID 为空时直接展示响应诊断，不发起任务查询。

下面示例需要将 `RCODER_URL`、`APP_ID` 设置为当前环境，并按部署方式补充认证头：

```bash
curl -sS -X POST "$RCODER_URL/api/v1/userapp/dev/start" \
  -H 'Content-Type: application/json' \
  -H "X-App-Id: $APP_ID" \
  --data "$(jq -nc --arg app_id "$APP_ID" '{app_id: $app_id}')" \
  > userapp-start-result.json

# success=false 时也可能有真实 task_id。
TASK_ID=$(jq -r '.data.task_id // empty' userapp-start-result.json)

if [ -n "$TASK_ID" ]; then
  curl -sS "$RCODER_URL/api/v1/userapp/tasks/$TASK_ID?app_id=$APP_ID" \
    -H "X-App-Id: $APP_ID"

  curl -N "$RCODER_URL/api/v1/userapp/tasks/$TASK_ID/logs/stream?app_id=$APP_ID&from_seq=0" \
    -H "X-App-Id: $APP_ID"
fi
```

任务 GET 的 `success=true` 只表示查询成功；任务结果看 `data.status`，失败原因看 `data.error` 和 `data.diagnostics`。`failed`、`cancelled`、`completed` 都是终态。

SSE 沿用现有事件结构：

```text
id: 0
event: log
data: {"event":"log","service":"workspace","line":"源码预检查失败：……；修复建议：……"}

id: 1
event: failed
data: {"event":"failed","error":"……"}
```

整体任务日志使用 `service="workspace"`，对应 `scope="task"`、`service_id=null`；已知服务问题使用真实 `service_id`。不要把 `workspace` 当成新生成的业务服务。构建失败、启动失败各按实际阶段记录；单个 `build_ok` 不能证明服务已经启动。

说明日志先于唯一终态事件发布，终态后流关闭。断线恢复时，`Last-Event-ID` 表示最后收到的事件 ID，服务端回放其后事件；`from_seq` 表示起始序号，包含该序号。任务快照的 `seq` 是下一条事件序号，可直接用作 `from_seq`，不能直接当作最后收到的 `Last-Event-ID`。

诊断任务即使没有可用的文件日志目录，也可使用任务 SSE。它们不会为保存日志而创建、迁移或覆盖项目目录。

## 保留时间与调用方持久化

任务表在 file-server 进程内存中，默认最多保留 1,000 个任务。终态按 24 小时 TTL 在后续任务注册时清理；容量达到上限时优先淘汰较早的终态任务，因此**不保证整整 24 小时都可查询**。进程重启、容器替换或容量淘汰后，旧任务 ID 可能不可查询。

调用方如需长期保留失败记录，应保存响应诊断及其与应用、请求、任务的关联。app-cli 的持久运行操作及恢复记录是另一套身份和保留机制，不因内存任务不存在就允许改用新操作重放未知结果。相关说明见 [dev 管理进程恢复](userapp-dev-owner-recovery.md) 和 [构建排队合同](userapp-build-queue.md)。

## Java / 前端接入边界

当前 Java 失败处理路径仍会丢失响应 `data`，失败 `task_id` 与应用任务、日志视图的关联仍需后续接入。本批不修改 Java 或前端，也不宣称用户界面已经完成展示。

后续接入至少需要：

- 在 `success=false` 分支保留 `data`，不要只抛出 `message` 后丢失任务和诊断。
- 使用服务端实际返回的 ID 关联任务，并允许为已经 Failed 的任务读取 GET/SSE。
- 展示阶段、修复目标、文件/字段和真实服务身份；保留原 Recovery 数据。
- 对 `task_id=null`、参数拒绝以及任务过期分别处理，不能伪造 ID 或无限重试查询。

Rust 的 TCP 透传回归覆盖 file-server-userapp 真实响应 → `forward_to_addr` → 任务 GET/SSE；它不替代 Java/前端接入、完整容器生命周期或 K8s 部署验收。
