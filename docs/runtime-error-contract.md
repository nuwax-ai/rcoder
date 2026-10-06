# 运行时错误与安全重试

RCoder 的错误响应保留 `code`、`message`、`data`、`tid`、`operation_id` 和 `blocker`。可选 `error_detail` 提供机器可读的原因与阶段，以及具体说明和修复建议。成功响应不带 `error_detail`。

## 错误分类

| code | 标准错误响应 HTTP 状态 | 含义 |
|---|---:|---|
| `ERR_RUNTIME_CONFIGURATION` | 500 | 平台运行时配置无效或缺少能力 |
| `ERR_RUNTIME_UNAVAILABLE` | 503 | 无法连接或查询运行时 |
| `ERR_RUNTIME_TIMEOUT` | 504 | 当前阶段未在 deadline 内完成 |
| `ERR_CONTAINER_CREATE_FAILED` | 500 | 创建容器失败 |
| `ERR_CONTAINER_START_FAILED` | 500 | 启动容器失败 |
| `ERR_CONTAINER_STOP_FAILED` | 500 | 停止容器失败 |
| `ERR_CONTAINER_EXEC_FAILED` | 500 | 容器命令执行通道失败 |
| `CONTAINER_NOT_FOUND` | 404 | 已知应用的物理容器或 Deployment 缺失，包括查询与捕获之间目标消失 |
| `ERR_APP_NOT_FOUND` | 404 | 权威应用身份确实不存在 |
| `ERR_NOT_FOUND` | 404 | 指定的操作记录等资源不存在；不表示应用身份缺失 |
| `ERR_CONTAINER_ADDRESS_NOT_READY` | 503 | 受管容器地址尚未就绪 |
| `ERR_DATABASE_NOT_READY` | 503 | 数据库就绪检查未通过 |
| `ERR_DATABASE_COMMAND_FAILED` | 500 | 数据库命令未成功完成 |
| `ERR_USERAPP_WAKE_FAILED` | 500 | 唤醒失败且没有更具体的错误分类 |
| `ERR_OPERATION_OUTCOME_UNKNOWN` | 500 | 已派发写入的结果无法确认 |
| `ERR_OPERATION_IN_PROGRESS` | 409 | 另一应用操作占用当前请求的操作槽或物理租约，本次请求尚未受理 |

已有明确的冲突、恢复、资源和业务错误继续使用原错误码。`ERR_CONTAINER_ERROR` 仅用于无法分类的剩余情况。

### 应用操作占用

`ERR_OPERATION_IN_PROGRESS` 只区分应用操作互斥的两层占用：持久 intent 当前绑定的非终态操作，以及运行时物理租约。生命周期不匹配、资源版本 CAS 冲突、请求重放指纹不一致、唤醒目标被替换及关闭准入仍使用原 `ERR_CONFLICT`，不能因文案出现“锁”或“进行中”改码。

正式 UserApp 接口继续返回 HTTP 200 + `success=false`；标准 `AppError` 响应为 HTTP 409。成功 `data` 和其他失败的任务/恢复 `data` 不变。此码的 `data` 示例：

```json
{
  "code": "ERR_OPERATION_IN_PROGRESS",
  "message": "另一操作正在执行（start），预计等待 20 秒，请稍后重试。",
  "data": {
    "holder_operation_id": "95a2d059-50ee-4ac7-b315-0a4ba7929f96",
    "holder_kind": "start",
    "holder_traffic_wake": true,
    "holder_state": "running",
    "holder_step": "traffic_wake_observing",
    "retryable": true,
    "retry_after_seconds": 20
  },
  "tid": "original-request-trace",
  "success": false
}
```

`holder_operation_id/kind/state/step` 来自同一份真实持久 holder 记录，含普通操作表与独立计算控制记录；kind/state 使用 snake_case，例如 `start_deployment`、`restart_deployment`、`hot_deploy`。已有顶层 `blocker` 保持原字段及枚举格式，操作 scope 仍在该对象中。租约已经获取但尚未持久受理、holder 已释放的竞态或无法核实持有者时，这四个 holder 字段均为 `null`，`holder_traffic_wake=false`、`retryable=false`、`retry_after_seconds=0`；不把租约上的随机 legacy UUID 当成真实操作号。

重试证据优先排除 `recovery_required`、已派发写入结果未知、查询失败和陈旧 `running`（超过 5 分钟没有权威进度）等保护状态。有效且正在推进的 `start` + `traffic=true` 建议 20 秒；部署与重启类建议 45 秒。建议时间不是执行成功保证，也不授权抢锁、取消 holder 或重放未知写入。

前端按以下分支处理，避免解析 message：

- 新码 + `retryable=true` + `holder_traffic_wake=true`：按钮置灰，在约 30 秒总预算内静默重试；保持本次尚未受理请求的 request ID 和输入。
- 新码 + `retryable=true` + 非流量唤醒：展示正在执行的 `holder_kind`，在约 45 秒总预算内重试，参考 `retry_after_seconds`。
- 新码 + `retryable=false` 或结构化 data 缺失：查询 holder/应用状态并提示排查，不自动重试。
- 其余 `ERR_CONFLICT`：保留现有错误处理，不转成自动重试。

一旦请求已经受理，改为查询自己的原 `operation_id`；`holder_operation_id` 只说明谁阻塞本次请求，不表示新的重启已经受理。Java 浏览器响应链目前仍需保留此 data，详见交接说明。

`POST /api/v1/userapp/{app_id}/restart` 在业务受理前遇到有明确进度证据的正常流量唤醒、部署或重启占用时，每 200ms 重核生命周期、持久 intent 和物理租约。两层及所有受理前只读准备共用同一个单调时钟 deadline，默认 30 秒；可用 YAML `app_manager.restart_admission_wait_secs` 或环境 `RCODER_USERAPP_RESTART_ADMISSION_WAIT_SECS` 配置，显式 YAML 值优先，选中的非法输入直接报配置错误。等待耗尽保留原 blocker/data，不受理另一项业务操作。物理 Stop/Restart 控制、未知 holder、`recovery_required` 与陈旧 `running` 立即拒绝；外部 Stop 仍快失败，Start 与内部回收器保持原排队语义。

等待期间 HTTP 断开会取消未受理请求。派发租约获取或持久受理写入后若结果未知，由原协调器继续核实，不能因为客户端已断开就重新派发；取得但未受理的租约只释放本次精确身份，并持续记录未确认清理。已成功 request ID 的重放返回原结果。30 秒等待预算不改变业务 Stop 的宽限或部署执行预算，客户端超时应覆盖等待与实际执行时间。

`ERR_APP_NOT_FOUND` 的字符串保持不变，只用于权威应用不存在的结果。已登记应用的物理容器或 Deployment 缺失返回既有 `CONTAINER_NOT_FOUND`；源码常量名为 `ERR_CONTAINER_NOT_FOUND`，实际响应字符串没有 `ERR_` 前缀。数据库、容器查询失败或超时不能转换成 `ERR_APP_NOT_FOUND`。两种不存在错误的标准响应均可为 HTTP 404，已有正式 UserApp 接口仍按既有中间件包装成 HTTP 200 错误信封。调用方必须按结构化 `code` 区分，不得因 HTTP 404 或物理资源缺失直接创建应用；先核验原应用身份、生命周期和操作状态。

HTTP 状态并不代替业务结果：原有 `HttpResult` 入口和正式 UserApp 包装继续使用 HTTP 200 + `success=false`；标准 `AppError` 入口按上表返回状态。已建立的 SSE 通过原事件类型输出错误，不改变已发送的 HTTP 状态。

文件服务保留既有 `error.type/message/timestamp/requestId` 包装。工作区运行时诊断的顶层 `code` 使用真实共享错误码，并按证据追加 `error_detail/operation_id/blocker/tid`；其他旧文件错误继续使用 `UNKNOWN_ERROR`。例如工作区连接查询失败返回 HTTP 503 + `ERR_RUNTIME_UNAVAILABLE`，不再切换 Local 目录后创建项目。Docker 明确返回无聚合路径时仍按既有 Local 契约处理。首次查询失败允许既有 `ensure` 流程重新核验，随后持续查询失败在原有 30 次预算内返回原原因；派发 `ensure` 后结果未知返回 `ERR_OPERATION_OUTCOME_UNKNOWN` 且不可自动重试。

```json
{
  "success": false,
  "code": "ERR_RUNTIME_UNAVAILABLE",
  "error": {
    "type": "SYSTEM_ERROR",
    "message": "workspace_resolve: Connection error: PV query unavailable",
    "timestamp": "2026/10/06 11:30:00",
    "requestId": "current-file-request"
  },
  "error_detail": {
    "reason_code": "ERR_RUNTIME_UNAVAILABLE",
    "stage": "workspace_resolve",
    "detail": "workspace_resolve: Connection error: PV query unavailable",
    "hint": "Check runtime connectivity and permissions; preserve the original operation identity.",
    "retryable": true
  }
}
```

## 诊断字段

```json
{
  "code": "ERR_OPERATION_OUTCOME_UNKNOWN",
  "message": "Database command completion was not confirmed",
  "data": null,
  "tid": "trace-original",
  "operation_id": "operation-original",
  "success": false,
  "error_detail": {
    "reason_code": "ERR_RUNTIME_UNAVAILABLE",
    "stage": "create_database",
    "detail": "Connection closed before the result arrived",
    "hint": "Inspect the original operation before retrying.",
    "retryable": false
  }
}
```

`reason_code` 和 `stage` 来自错误产生处，不从中英文文案推断。已知时可携带真实 `task_id` 和 `service_id`；未登记任务、未知模块不生成替代 ID。操作 ID、任务 ID、请求 ID 和 trace ID 各有用途，不能互相代替。任务失败 `data`、恢复载荷及原阻塞信息继续保留。

已受理操作的错误绑定原父操作身份；下游任务及 blocker 保留各自身份。成功操作之后的状态查询失败仍关联原操作，但不改写已确认的成功记录。相同请求身份重放失败时保留持久记录的原错误码、阶段和操作身份，不改成通用后端错误，也不重新执行。

`message`、`detail` 和 `hint` 在输出及对应错误日志前脱敏并限制长度。凭据执行通道必须提供不含命令和密码的安全摘要，不能依赖通用脱敏识别所有 shell 编码。已有持久操作记录和第三方原因保留其安全的原信息；共享提示按本次请求语言返回，未提供语言时使用英文。语言选择不修改全局状态。

部署预算保持已有选择规则：启用 `app_manager` 且 YAML 未显式指定 `deploy_budget` 整段时，读取 `RCODER_USERAPP_DEPLOY_*` 环境配置。显式预算整段使用其自身字段和默认值，环境不逐字段覆盖；禁用模块不解析无关预算。已选择的非法数值或非 Unicode 输入返回 `ERR_RUNTIME_CONFIGURATION` 和 `deploy_budget_configuration` 阶段，不打印原输入值。修正后的新加载重新读取当前输入，默认配置生成不会将环境预算固化为显式段。

## 重试与结果未知

`error_detail.retryable` 表示在当前证据下，使用原身份和相同输入重试是否安全。不能仅因 HTTP 502–504、出现“timeout/starting”或中文提示就重试。

- 明确的只读或派发前失败可以标记可重试；配置、凭据错误需修正输入后发起新请求。
- 已受理操作先按原 `operation_id` 查询结果；重复提交不得变成另一项操作。
- 已派发写入但结果未确认时，`retryable=false`。不能因当前对象存在、端口拒连或一个后续查询成功，就把原写入判为成功或无影响。
- 冲突、清理未确认和需要恢复的操作继续保持相应保护。后续请求重新核验，不以历史标签永久拒绝管理操作。
- 无 `error_detail` 的旧响应仅按已知保守错误码处理；未知错误码默认不自动重试。

诊断查询和业务 readiness 失败不授权停止或重启容器。SSE 后续观测可以补充 OOM、CrashLoop 等信息，但不能替换原失败原因和身份。

Java 对接说明见 [Java 错误契约交接](development/java-error-contract-handoff.md)。
