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

一旦请求已经受理，改为查询自己的原 `operation_id`；`holder_operation_id` 只说明谁阻塞本次请求，不表示新的重启已经受理。调用方应将结构化 `data` 完整透传到最终响应，保留操作身份与重试证据。

Prod 镜像滚动重启进入恢复后，若运行时正向确认目标及残留计算资源均已不存在，原操作以 `Failed`、`error_code=ERR_RUNTIME_ABSENT` 终态收束并释放操作槽。探测失败或仍有残留时继续恢复，不按缺席收束；Dev builder 不使用 Prod 运行时的缺席结果。查询该失败操作仍返回正常的操作状态信封，重放原请求保留原失败结果；重新启动须通过新的 Start 请求，使用当前发布配置重建运行时。

`POST /api/v1/userapp/{app_id}/restart` 在业务受理前遇到有明确进度证据的正常流量唤醒、部署或重启占用时，每 200ms 重核生命周期、持久 intent 和物理租约。两层及所有受理前只读准备共用同一个单调时钟 deadline，默认 30 秒；可用 YAML `app_manager.restart_admission_wait_secs` 或环境 `RCODER_USERAPP_RESTART_ADMISSION_WAIT_SECS` 配置，显式 YAML 值优先，选中的非法输入直接报配置错误。等待耗尽保留原 blocker/data，不受理另一项业务操作。物理 Stop/Restart 控制、未知 holder、`recovery_required` 与陈旧 `running` 立即拒绝；外部 Stop 仍快失败，Start 与内部回收器保持原排队语义。

等待期间 HTTP 断开会取消未受理请求。派发租约获取或持久受理写入后若结果未知，由原协调器继续核实，不能因为客户端已断开就重新派发；取得但未受理的租约只释放本次精确身份，并持续记录未确认清理。已成功 request ID 的重放返回原结果。30 秒等待预算不改变业务 Stop 的宽限或部署执行预算，客户端超时应覆盖等待与实际执行时间。

`ERR_APP_NOT_FOUND` 的字符串保持不变，只用于权威应用不存在的结果。只读查询中，已登记应用的物理容器或 Deployment 缺失返回既有 `CONTAINER_NOT_FOUND`；源码常量名为 `ERR_CONTAINER_NOT_FOUND`，实际响应字符串没有 `ERR_` 前缀。数据库、容器查询失败或超时不能转换成 `ERR_APP_NOT_FOUND`。两种不存在错误的标准响应均可为 HTTP 404，已有正式 UserApp 接口仍按既有中间件包装成 HTTP 200 错误信封。调用方必须按结构化 `code` 区分，不得因 HTTP 404 直接新建应用身份。

Docker prod 应用业务流量具有独立的恢复语义：确认物理容器缺失后，由生命周期协调器核验原 Active 应用及当前私有运行配置，复用原应用与数据重建计算资源。readiness、普通状态查询以及原工具使用入口不因此变为创建入口。运行时连接失败、超时、外属资源和已删除应用均不能按物理缺失推定授权；已经受理的重建在请求断开后仍由原执行者继续收束。

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

### Computer 聊天与进度订阅

`POST /computer/chat` 在 Agent Runner 已受理任务后，只追加该执行实例的会话登记，不重发原消息。同一共享容器的其他项目登记或普通版本更新不表示容器已被替换；K8s 工作负载确实被替换时，由原入口重新观察当前实例、归属与工作区后恢复映射，保留已有会话标识和数据卷。

容器注册门禁以数据库为唯一裁决依据（containers 行 + `container_tombstones`），内存镜像只是缓存：本系统 stop/cleaner 已退役的绑定（tombstone 在案）允许同名重建的自我换代直接注册，跨副本镜像残留不产生额外拒绝。数据库绑定仍在而真实 workload 已被外部替换时，派发前返回 `ERR_COMPUTER_OWNER_CONFLICT`（HTTP 409），`error_detail.reason_code=CONTAINER_WORKLOAD_REPLACED`、`stage=computer.registration.before_dispatch`、`retryable=false`，提示先停止该项目释放旧绑定；容器行与 tombstone 俱无的异常状态同样拒绝并记录告警，不静默放行。

任务已经受理但会话登记未确认时，响应保留原 `data.session_id`：登记结果未知返回 `ERR_SERVICE_UNAVAILABLE`，执行身份已被替换返回 `ERR_CONFLICT`；沿既有 `HttpResult` 契约使用 HTTP 200 + `success=false`，保留会话数据。派发前的标准 `AppError` 则分别使用 503/409。`error_detail.stage` 为 `chat.session_registration`，`task_id` 为原会话标识，`retryable=false` 指原消息不能自动重发。调用方应保留会话标识，仅重连 `GET /computer/progress/{session_id}`；本地待提交缓存不构成持久化确认。已有 Agent Runner 业务失败继续保留原失败码和数据。

准备中的请求尚无 ACP 会话标识，准备状态与真实会话登记分开保存。旧版本遗留的 `pending` 仅按缺失恢复提示处理，不参与会话恢复、反查或新登记；历史原文保留，不要求用户清库。若旧 Runner 成功响应却返回占位值或空标识，主控返回英文 `ERR_AGENT_ERROR`，使用 `chat.session_identity` 阶段、`data=null`，不生成虚假 `task_id`，也不自动重发消息。错误响应中的占位标识同样不发布，原业务错误码和原因保留。此时应查看项目状态和日志，不能使用占位值重连进度。真实会话仍保留跨项目归属保护。

普通 Computer 会话的进度查询在存储读取失败时返回可重试的 503，只有确认找不到会话时才返回 404。进度入口仍优先解析 UserApp 会话，按该领域原有错误契约传播失败；不能将查询异常伪装成会话不存在。连接建立后，仍通过既有 SSE 事件报告后续错误。

`error_detail.retryable` 表示在当前证据下，使用原身份和相同输入重试是否安全。不能仅因 HTTP 502–504、出现“timeout/starting”或中文提示就重试。

- 明确的只读或派发前失败可以标记可重试；配置、凭据错误需修正输入后发起新请求。
- 已受理操作先按原 `operation_id` 查询结果；重复提交不得变成另一项操作。
- 已派发写入但结果未确认时，`retryable=false`。不能因当前对象存在、端口拒连或一个后续查询成功，就把原写入判为成功或无影响。
- 冲突、清理未确认和需要恢复的操作继续保持相应保护。后续请求重新核验，不以历史标签永久拒绝管理操作。
- 无 `error_detail` 的旧响应仅按已知保守错误码处理；未知错误码默认不自动重试。

诊断查询和业务 readiness 失败不授权停止或重启容器。SSE 保留原 gRPC 错误码、`error_detail` 中的 cause/stage 和已捕获 request 身份。传输错误的主 `message` 使用友好提示；只有诊断的物理 Pod UID/Docker CID 与该流已解析的实例相同，且 OOM 终止时间落在当前 stream/PromptStart 开始至本次有界诊断观测时刻的窗口内（不接受未来时间戳）时，主提示才明确呈现 OOM。历史、替代实例或缺少身份/时序证据的状态仅作 `observed_container_state` 补充，不归因于新执行。诊断共用原 2 秒预算，失败或超时降级通用提示，不增加启动、路由或 SSE 准入门禁；权限、参数、模型限流等非传输错误保持原提示。

调用方应保留本页定义的结构化错误字段、原操作身份和重试语义，避免仅透传 message 或通过文案判断错误类型。
