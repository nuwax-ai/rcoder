# file-server 开发服务操作恢复与调用方接入

## 行为与边界

file-server 登记一个 app-cli owner 仅表示持有该 owner 的身份信息，不证明业务已启动。开发启动任务不再凭 `dev/list` 存在记录返回成功。外部 owner 路径必须经过身份核验和控制协议；本地旧 spawn 路径的快速返回必须同时确认仍持有同一存活子进程、当前 release、管理状态 Running、Ready 且无待恢复操作。有新 PG 输入时禁止走快速返回。

每个开发构建任务将原 `task_id` 传入 `request_context`。发送控制请求之前，file-server 将原 operation ID、instance、revision、profile、项目规范路径及请求摘要原子写入本地状态文件。凭据和 owner token 不写入该文件。响应丢失后保留意图；后续不同任务不能将旧意图当作自己的成功结果，也不能覆盖它或在其前重新编译。返回 `ERR_CONFLICT` 并包含需要恢复的原 operation ID。

状态文件使用短文件锁和原子替换，锁文件不删除；同进程登记发布和收束按 processes mutex → try 文件锁统一顺序执行，落盘成功后才改变内存，锁不跨 await。读取损坏、写入失败、身份变化均保持保护。该机制用于容器内/宿主机 file-server 与唯一 app-cli owner 的请求恢复，不替代 RCoder 控制存储的跨副本生命周期事务。

## 恢复接口

`POST /api/v1/userapp/dev/operations/{operation_id}/recover`

RCoder 主服务显式注册此路径，按 body `app_id` 定位现有 dev builder；即使携带 `X-App-Stage: prod` 也不会切到 prod。若同时给 `X-App-Id`，它必须与 body 一致。容器不在时返回 `ERR_CONTAINER_NOT_FOUND`，不自动创建新物理实例执行旧操作。沿用 UserApp 路由及上层鉴权/应用访问控制。请求字段沿用 `DevOpBody`：

```json
{"app_id":"123"}
```

`operation_id` 必须是原操作；`app_id` 必须与原项目相符。可选 `pg` 格式与 dev start/restart 一致，包含原操作所用的凭据，仅用于严格匹配并重放原请求。`base_path` 在恢复接口无作用。没有 `user_id` 维度。

处理顺序：

1. 按项目和原 ID 读取持久意图，核验规范项目路径。
2. 不带 token 查询 owner 身份，验证应用、workspace、源目录及原 runtime instance。
3. 核验成功后读取本地 token，查询原操作。
4. 原操作存在时返回其当前状态；只有仍在途意图且 owner 明确返回 404，才使用原 ID、原 revision、原 profile 重放原请求。
5. 如原请求包含 PG 而调用方未再提供，允许查询已受理操作；404 时明确要求原配置，不能猜默认值。提供不同 PG 必须拒绝，即使旧操作已成功。
6. 已确认 Succeeded / Failed / Cancelled 后收束在途意图并保留完成回执；完成回执只允许查询。owner 历史清理后的 404 不会重新执行历史操作。
7. RecoveryRequired、断连、身份变化、未知响应保留保护。Stop 明确成功后仅按 registration_operation_id + runtime instance 条件移除对应登记；旧 Stop 的迟到成功不能清理后来 Restart 的登记。

响应使用已有 UserApp `HttpResult` 信封，data 包含 `task_id`（原构建任务 ID，如存在）和 `operation`（结构化状态）。信封成功表示本次查询或重放请求已得到结果，**不代表业务成功**；必须检查 `operation.state`。响应不包含 PG 密码或 owner token。

## Java / 调用方接入

- 继续保存 dev start/restart 返回的 task ID 并轮询/SSE。
- 收到含原 operation ID 的冲突时，提示存在待恢复操作，调用上述接口；不要不断创建新构建任务。
- file-server 重启后内存任务历史可能不存在。`GET tasks/{task_id}` 如能匹配持久意图/回执，会返回包含原 operation ID 和恢复路径的明确错误；不会伪造新任务或声称原任务完成。
- 恢复接口复用原运行控制请求，不重新执行 build，不把当前源码/新配置混入旧操作。
- 原操作 Failed / Cancelled 的恢复响应仍保留失败状态；调用方可在确认后发起一次新的显式启动，app-cli 的恢复保护仍有最终裁决权。
- owner 已换代时旧意图不自动接管新实例，应按原 lifecycle / 资源身份执行显式恢复处置。

## 本轮反例与验证边界

新增核心反例：相同 task 重试保留原 ID/revision；不同 task 或匿名新 ID 不能替换待恢复请求；匿名重放必须提供原 ID。新增真实 handler 调用反例覆盖请求响应丢失（受理前/受理后）、file-server 重建后原 ID 查询或重放、完成后再次查询、owner 清历史后禁止重放、跨 app 拒绝，以及外层启动协调器不能凭 owner 登记返回假成功。

主线程本轮 `file-server + file-server-userapp --all-features` 共运行 439 项，437 通过，2 项为新增 DTO 位置和 OpenAPI summary 工程契约失败；真实 handler 失响应/重启恢复、完成历史查询、核心 task context 与 Stop CAS 反例通过。两项契约已按现有规范修复，主线程复跑 439/439 通过，0 skip；Clippy exit 0，发现测试模块布局和无用常量两项 warning，已做无业务变化清理，待默认 features + Clippy 最终检查。证据：`/tmp/rcoder-recovery-handlers-nextest-final.log`。先前 348 项 file-server 通过仅对应上一轮 durable-intent 实现。Compose、remote K8s 与跨平台完整回归另行记录，不以 HTTP fixture 替代部署验收。

完成回执目前保留在本地状态文件中，尚未定义自动历史淘汰期；高频长期操作的回执压缩需单独确定保留契约。owner 已清理的历史只会明确不可查询，不会重放。

主服务接线追加真实 Router/forward handler 反例：无 app header 的 body 定位命中 dev runtime，prod header 不改变环境，缺失容器不 ensure，冲突 header/body 在容器查询之前拒绝。共享 RuntimeOperationView 的 kind/state 文档已列出全部 wire 枚举值。该批接线反例待统一编译执行。
