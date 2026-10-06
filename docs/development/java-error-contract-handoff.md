# Java 错误契约交接

本轮仅修改 RCoder，Java 项目保持只读。以下源码核对基于 `agent-platform` 提交 `dcd954f8e0ef400700c9fd465204f48ef1b0c891`；供 Java 负责人评估和实施，不表示 Java 已完成接入。

RCoder 仍按可信内网服务部署，服务 key 默认关闭，Java 负责用户身份与业务归属。无需新增 Rust 用户、租户或 JWT 协议。接口契约见 [运行时错误与安全重试](../runtime-error-contract.md)。

## 错误保留

- `UserAppSandboxClient.java:525` 的 `toResult` 保留 `code/message/data`，但未保留 `tid/operation_id/blocker/error_detail`；建议结果 DTO 增加这些可选字段，整条应用服务及页面响应链继续保留。
- `UserAppSandboxClient.java:544` 已解析 4xx/5xx 的 `code/message`，不能把它描述成完全丢失错误体；同样需要保留新增诊断及原操作身份。
- `UserAppSandboxClient.java:563` 将无业务 code 的裸 HTTP 404 转成 `ERR_APP_NOT_FOUND`。建议保留原结构化 code，并区分权威应用不存在、物理容器缺失、路由不存在和代理错误；裸 HTTP 404 不能作为应用不存在或再次创建的依据。
- `UserAppApplicationServiceImpl.java:347` 的停止后删除逻辑单独识别 `ERR_APP_NOT_FOUND`。RCoder 保留该字符串，但查询故障、超时和未知写入结果不会使用此码，Java 不应将新错误归并成不存在。
- `SandboxAgentClient.java:684` 的 chat 非成功 HTTP 分支向调用方仅输出状态，日志才有响应体；建议解析并传递结构化错误和安全原因。
- `SandboxAgentClient.java:780` 的 SSE 错误分支最终创建 `AgentException("ERROR", message)`，且部分停止/清理动作取决于英文关键词。建议保留原 code、request/session/operation 身份及诊断，按结构化类型判断。普通连接错误及 `ready=false` 不自动授权 Stop。

已知应用的物理容器或 Deployment 缺失现在保留既有响应字符串 `CONTAINER_NOT_FOUND`（源码常量 `ERR_CONTAINER_NOT_FOUND`），标准错误响应为 HTTP 404；查询与捕获之间目标消失也使用该码。它不表示应用身份不存在，不能归并为 `ERR_APP_NOT_FOUND`，也不能触发创建或误删除。Java 对 `ERR_APP_NOT_FOUND` 的既有单独识别可以保留；只有明确的权威应用不存在结果才使用该分支。应先核验原应用的生命周期和操作结果，随后按对应恢复或新请求流程处理。

HTTP 200 也可能是业务失败，应看 `success/code`；已建立的 SSE 继续消费原错误事件。`error_detail` 是可选对象，不复用既有字符串 `error` 字段。页面可以展示 message、阶段、detail、hint 和原操作身份；不向用户输出密码或完整环境。

已知应用下的指定操作记录缺失使用共享 `ERR_NOT_FOUND`，不触发 `ERR_APP_NOT_FOUND` 分支。已受理操作的错误关联原父 `operation_id`，下游任务和 blocker 保留各自身份。Stop/Start/Restart 已确认成功之后，状态查询失败不会改写原成功记录；按原操作查询结果再处理状态读取。相同请求身份的失败重放保留原错误分类，不重新执行。

文件/创建项目接口继续保留 `error.type/message/timestamp/requestId` 对象。工作区运行时错误的顶层 `code` 使用共享错误契约，包含既有 `CONTAINER_NOT_FOUND`，可另带 `error_detail` 和真实操作/阻塞身份；其余旧文件错误维持 `UNKNOWN_ERROR`。Java 不应假定所有文件错误都只有 `UNKNOWN_ERROR`，也不能将工作区查询失败视为项目不存在后再次创建。

## 安全重试

明确的 `error_detail.retryable=false`、结果未知、恢复保护或冲突禁止自动重放。`retryable=true` 也应保持原 request/operation 身份与相同输入；已受理操作优先查询原结果。没有该字段的旧响应或未知错误码按保守策略处理，不能按中英文关键词或所有 502–504 自动重试。

`ERR_OPERATION_IN_PROGRESS` 是针对另一操作占用应用操作槽或物理租约的明确分支。本次请求未受理时，依据其 `data.retryable/holder_traffic_wake/retry_after_seconds` 决定重试；其余 `ERR_CONFLICT` 保持原处理。不要把持有者的 `holder_operation_id` 当成本次新重启已经受理的操作号。详细字段、未知 holder 的 null 规则和前端四个分支见 [公共契约](../runtime-error-contract.md#应用操作占用)。

2026-10-07 对同一 Java 基线追加只读核对：`UserAppSandboxClient.java:525-540` 的正常 HTTP 200 解析确实保留 Map/List data；`parseErrorResult(:544-568)` 的 4xx/5xx 分支只取 code/message，未保留 data。随后 `UserAppTaskApplicationServiceImpl.java:538-539` 与 `UserAppTaskDomainServiceImpl.java:251-255` 的 `downstreamError` 只构造 `BizException(code, message)`；`BizException.java` 当前无 data 字段。`UserAppRuntimeController.java:224-233` 的 prod restart 捕获异常后调用 `ReqResult.error(code,message)`，该工厂显式使用 `data=null`。因此“RPC DTO 可以解析 data”不等于浏览器已经收到 data，新码可以保留但前端暂时拿不到安全重试证据。

请 Java 负责人为该失败链保留结构化 data 及可选 `operation_id/blocker/error_detail/tid`，并实测 `code/displayCode` 均为新码、data 七个字段完整、未知 holder 不伪造 ID；标准 409 与正式接口 HTTP 200 两类响应都需覆盖。`ReqResult.error(code,message)` 当前让 displayCode=code，但异常链是否被其他拦截器改写仍须实际联调确认。本轮没有修改或运行 Java，不宣称透传实测通过。

restart 的受理前等待预算默认为 30 秒，部署执行仍有自己的父预算。Java 客户端超时应覆盖等待预算和实际执行时间（执行可能超过 30 秒），不能仅设为 30 秒；等待期间尚未受理时断连可放弃，已受理后断连不表示操作取消，先查询原结果。Java 与前端还需保持稳定 request ID 和相同输入，不能每次自动重试生成另一项操作。

建议 Java 回归覆盖：权威应用缺失时 `ERR_APP_NOT_FOUND` 的既有分支；应用仍已登记而物理目标缺失时 `CONTAINER_NOT_FOUND` 不触发创建或误删除；裸 HTTP 404 不冒充应用不存在；运行时查询错误阻止误删除；HTTP 200/4xx/5xx 与 SSE 的字段保留；数据库写入后响应丢失不重放；凭据失败修正后新请求成功。

## 可选服务凭据

Java → RCoder：显式开启 RCoder 服务 key 时，经 RCoder 的控制面、文件、工作区、chat 和 SSE 调用带既有 `serverApiKey`；默认关闭行为保持。Gateway 调 RCoder 控制面同样使用配置中的控制服务 key。

RCoder → 容器内文件代理 `:60000`：该目标沿用已有 `FILE_SERVER_PROXY_TOKEN` / `X-Proxy-Token` 契约，不实现主 RCoder 的 `X-Api-Key` 协议。RCoder 从现有文件代理配置、开发服务环境或生产容器实际环境解析 token；主控制 key 不转发给文件代理或 `agent_runner:8086`。文件调用及数据库 HTTP 执行使用同一 token 来源；凭据缺失或错误按真实目标返回失败，不自动生成凭据。Java 仅在其直接访问启用 token 的文件代理时按该独立部署契约装配 `X-Proxy-Token`，不能把两个凭据混用或把用户输入头当作内部服务凭据来源。

`ComputerFileClient` 当前已经保留完整 `SandboxServer` 配置，不能沿用旧镜像中“只有 URL”的归因；但目录浏览调用及多处文件/工作区调用仅设置路由头或 Content-Type，需统一补服务凭据。WebSocket、multipart、Range 和流式请求也应覆盖，保留现有请求语义，不强制转成普通 JSON。

Java 负责人可复用现有配置和请求装配，避免引入新的用户身份字段。RCoder 侧默认关闭服务 key 的行为保持；相关 Java 改动和联调由 Java 项目负责人完成。

## 生产重启的版本与控制语义

2026-10-06 对上述 Java 提交追加只读核对：`UserAppActionReq` 已包含可选 `releaseId`，但 `UserAppRuntimeController.java:224` 仅将 `appId` 传给 `prodRestart`；`UserAppTaskApplicationServiceImpl.java:162` 再以 `releaseId=null` 调用 `prodAction`。该方法在 `:438` 解析版本，而 `resolveVersion(:526)` 在版本为空时选择 `latestBuildVersion()`。因此用户提交的明确版本当前会被忽略，实际重新部署最新构建。Java 负责人应将可选版本贯穿控制器、服务接口与实现，保留空值选择最新版本的既有行为，并覆盖指定版本、空值及不存在版本三个分支；本轮未修改 Java。

Java 的 `prodRestart` 是业务制品部署：`prodAction(:448)` 构造包 URL、release ID 和数据库输入，`UserAppSandboxClient.java:187` 向 RCoder `POST /api/v1/userapp/{app_id}/restart` 发送 `deployBody(:451)`。该 RCoder 入口调用 `restart_app_enhanced`，受普通 Prod/Application 操作槽及资源租约保护；真正的另一操作占用使用 `ERR_OPERATION_IN_PROGRESS`，其 `data` 和已有 `blocker` 指向原阻塞操作，其余冲突仍为 `ERR_CONFLICT`。这都不能当成新重启已经受理，更不能用 HTTP 200 或重新查询到端口连通推断成功。修复包内容也不会自动解除一个仍在执行或结果未知的原操作。

若产品需要“重启计算容器并保留工作卷”，应单独建模计算控制，使用 RCoder `POST /computer/pod/restart` 的 `app_id`、`app_stage="prod"`、可选 `lifecycle_id` 和稳定 `request_id`，按 HTTP 202 返回的原 `operation_id/status_url` 查询。它走独立持久计算协调器，普通业务槽不阻止其受理，仍须核验生命周期、原物理目标及租约；在途 Stop、共享物理写入未知或计算恢复保护不会被绕过。计算重启不携带包 URL，不替代指定制品部署，也不应通过在业务重启失败后静默切换接口来实施。

对于 `Start / Running / traffic_wake_observing`，单靠标签不能确定为僵死门禁。RCoder 当前执行者在确认启动写入后遇到只读就绪观察超时，会记录诚实的 Failed 并释放自己的原租约；未知启动写入保留恢复保护。历史 Running 的恢复还需要原 `start_write_acknowledged`、捕获目标与完整上下文、持久 deadline 及历史宽限，并按完整原记录 CAS 收束、随后条件释放原租约；不得按时间、端口或数值 PID 删除状态或杀进程。本地源码已将历史 Running 观察恢复接入既有常驻恢复扫描器，独立于全局/单应用回收开关及近期流量；恢复任务先复核权威应用 Active、同生命周期及原 Prod 操作槽，再复用上述完整证据与原租约路径。修复前实际启动扫描器与 Turso 的三个反例确认了旧入口缺口，五类保护场景也实际保留了原操作与租约；修复后的正例、保护反例及本轮默认/全功能组件回归均已通过。平台历史唤醒恢复尚未做真实 K8s 验收，也不证明线上特定阻塞操作已恢复；线上状态、checkpoint、原租约和实际运行时回执仍待取证。
