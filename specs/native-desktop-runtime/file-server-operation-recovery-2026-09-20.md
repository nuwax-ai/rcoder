# file-server 开发服务操作恢复与调用方接入

## 行为与边界

file-server 登记一个 app-cli owner 仅表示持有该 owner 的身份信息，不证明业务已启动。开发启动任务不再凭 `dev/list` 存在记录返回成功。外部 owner 路径必须经过身份核验和控制协议；本地旧 spawn 路径的快速返回必须同时确认仍持有同一存活子进程、当前 release、管理状态 Running、Ready 且无待恢复操作。有新 PG 输入时禁止走快速返回。

每个开发构建任务将原 `task_id` 传入 `request_context`。发送控制请求之前，file-server 将原 operation ID、instance、revision、profile、项目规范路径及请求摘要原子写入本地状态文件。凭据和 owner token 不写入该文件。响应丢失后保留意图；后续不同任务不能将旧意图当作自己的成功结果，也不能覆盖它或在其前重新编译。返回 `ERR_CONFLICT` 并包含需要恢复的原 operation ID。

状态文件使用短文件锁和原子替换，锁文件不删除；同进程登记发布和收束按 processes mutex → try 文件锁统一顺序执行，落盘成功后才改变内存，锁不跨 await。读取损坏、写入失败或无法证实的身份变化均保持保护。该机制用于容器内/宿主机 file-server 与唯一 app-cli owner 的请求恢复，不替代 RCoder 控制存储的跨副本生命周期事务。

## 恢复接口

`POST /api/v1/userapp/dev/operations/{operation_id}/recover`

RCoder 主服务显式注册此路径，按 body `app_id` 定位现有 dev builder；即使携带 `X-App-Stage: prod` 也不会切到 prod。若同时给 `X-App-Id`，它必须与 body 一致。容器不在时返回 `ERR_CONTAINER_NOT_FOUND`，不自动创建新物理实例执行旧操作。沿用 UserApp 路由及上层鉴权/应用访问控制。请求字段沿用 `DevOpBody`：

```json
{"app_id":"123"}
```

`operation_id` 必须是原操作；`app_id` 必须与原项目相符。可选 `pg` 格式与 dev start/restart 一致，包含原操作所用的凭据，仅用于严格匹配并重放原请求。`base_path` 在恢复接口无作用。没有 `user_id` 维度。

处理顺序：

1. 按项目和原 ID 读取持久意图，核验规范项目路径。
2. 不带 token 查询 owner 身份，验证应用、workspace、源目录。原 runtime instance 一致才允许后续同请求重放；仅进程已重启时进入历史回执只读分支。
3. 核验成功后读取本地 token，查询原操作。
4. 原操作存在时返回其当前状态；只有仍在途意图且 owner 明确返回 404，才使用原 ID、原 revision、原 profile 重放原请求。
5. 如原请求包含 PG 而调用方未再提供，允许查询已受理操作；404 时明确要求原配置，不能猜默认值。提供不同 PG 必须拒绝，即使旧操作已成功。
6. 已确认 Succeeded / Failed / Cancelled 后收束在途意图并保留完成回执；完成回执只允许查询。owner 历史清理后的 404 不会重新执行历史操作。
7. 新 owner 可以提供原 ID + 原 instance + kind + 请求摘要一致的持久终态历史。仅 Succeeded / Failed / Cancelled 可 CAS 收束旧意图并退役旧登记，不向新 owner 重放任何写请求。404、RecoveryRequired、断连、无法验证的身份变化或未知响应保留保护。Stop 明确成功后仅按 registration_operation_id + runtime instance 条件移除对应登记；旧 Stop 的迟到成功不能清理后来 Restart 的登记。

响应使用已有 UserApp `HttpResult` 信封，data 包含 `task_id`（原构建任务 ID，如存在）和 `operation`（结构化状态）。信封成功表示本次查询或重放请求已得到结果，**不代表业务成功**；必须检查 `operation.state`。响应不包含 PG 密码或 owner token。

## Java / 调用方接入

- 继续保存 dev start/restart 返回的 task ID 并轮询/SSE。
- 收到含原 operation ID 的冲突时，提示存在待恢复操作，调用上述接口；不要不断创建新构建任务。
- file-server 重启后内存任务历史可能不存在。`GET tasks/{task_id}` 如能匹配持久意图/回执，会返回包含原 operation ID 和恢复路径的明确错误；不会伪造新任务或声称原任务完成。
- 恢复接口复用原运行控制请求，不重新执行 build，不把当前源码/新配置混入旧操作。
- 原操作 Failed / Cancelled 的恢复响应仍保留失败状态；调用方可在确认后发起一次新的显式启动，app-cli 的恢复保护仍有最终裁决权。
- owner 进程重启后，可通过显式 recover 读取旧操作终态历史；成功收束仅清理原 operation ID + 原 instance 的本地登记，不接管新实例。新 owner 无匹配终态证据时继续按原 lifecycle / 资源身份执行显式恢复处置。

## 本轮反例与验证边界

新增核心反例：相同 task 重试保留原 ID/revision；不同 task 或匿名新 ID 不能替换待恢复请求；匿名重放必须提供原 ID。新增真实 handler 调用反例覆盖请求响应丢失（受理前/受理后）、file-server 重建后原 ID 查询或重放、完成后再次查询、owner 清历史后禁止重放、跨 app 拒绝，以及外层启动协调器不能凭 owner 登记返回假成功。

主线程本轮 `file-server + file-server-userapp --all-features` 共运行 439 项，437 通过，2 项为新增 DTO 位置和 OpenAPI summary 工程契约失败；真实 handler 失响应/重启恢复、完成历史查询、核心 task context 与 Stop CAS 反例通过。两项契约已按现有规范修复，主线程复跑 439/439 通过，0 skip；Clippy exit 0，发现测试模块布局和无用常量两项 warning，已做无业务变化清理，待默认 features + Clippy 最终检查。证据：`/tmp/rcoder-recovery-handlers-nextest-final.log`。先前 348 项 file-server 通过仅对应上一轮 durable-intent 实现。Compose、remote K8s 与跨平台完整回归另行记录，不以 HTTP fixture 替代部署验收。

完成回执目前保留在本地状态文件中，尚未定义自动历史淘汰期；高频长期操作的回执压缩需单独确定保留契约。owner 已清理的历史只会明确不可查询，不会重放。

主服务接线追加真实 Router/forward handler 反例：无 app header 的 body 定位命中 dev runtime，prod header 不改变环境，缺失容器不 ensure，冲突 header/body 在容器查询之前拒绝。共享 RuntimeOperationView 的 kind/state 文档已列出全部 wire 枚举值。该批接线反例待统一编译执行。


## owner 重启后的终态收束补充

旧操作已持久化终态，但响应丢失且 owner 随后重启时，新 instance 不再导致无条件永久阻塞。当前 owner 的同项目身份与本地 token 核验后，仅查询原记录；核对原 operation ID、原 instance、kind 及共享协议请求摘要。调用方携带 PG 时仍先核对本地完整私有请求摘要，不能用不含 PG 的协议摘要替代。收束登记按原 registration_operation_id + instance 比较，后来操作的登记保留。

新增 HTTP fixture 反例覆盖三个允许终态、404、RecoveryRequired、旧 instance/kind/digest 不匹配、workspace 不匹配及新登记已创建的交错，均断言 POST 数量为零。当前补充尚待主线程统一组件验证，不代表部署回归完成。

## macOS 宿主机实际验证与 Restart 修正

个人 Mac Mini 使用独立当前源码快照、真实 Vue/Vite 模板、app-cli、Pingap 与 embed file-server-proxy 完成验证。透明代理只丢一次已转发 POST 的响应，不修改业务终态。第一次实测暴露恢复后显式 Restart 的先 Stop 门禁：旧登记已退役，新 owner 虽合法却无法进入 Restart。已修复为 UserApp Restart 先核验 owner 并提交单一 Restart；无 owner 时才走本地进程 stop/start。不同项目仍拒绝；新显式操作沿用新构建 task context，不复用旧请求身份。

组件反例 2/2 通过。补充快照 `332c669ed5220a5ca26c3f719365b0f4cab467c7e09b2be7bcfccc0c9512b0bc` 在 Mac Mini 原生构建 app-cli 与 file-server-proxy 后，case `recovery-real-ec8a6b09ee` 完整通过（脚本 exit 0）：

- 原 operation 真实 Succeeded，但丢响应使 file-server 保留原 pending。
- owner 正常退出后重启，instance 确认不同；恢复原终态不增加 POST，旧意图与登记收束。
- 随后显式 dev/restart 产生不同 operation ID，任务 completed；全程仅原请求与新 Restart 两个 POST。
- 测试进程退出后，3018 / 9080 / 5756 / 9081 真实 bind 验证端口释放。

二进制摘要：app-cli `f1419edbc1df4071fb977dcdbc899d047889a5217cc3b40c8da08663a18a49d7`；file-server-proxy `08c9fdd4031db721a0e8b72eccd6110b145d7573418144c33c4fbaff1a32921c`。

证据 `/tmp/rcoder-native-recovery-real-report.md`、`/tmp/rcoder-native-recovery-real-restart-fixed.log`、`/tmp/rcoder-native-restart-build.log`。首次失败和允许 Start 路径的历史通过均保留在报告，未改写为修复后结果。本轮只证明上述 macOS 原生链，不替代 Compose、K8s 与其他平台验收。
