# Spec：UserApp 按应用环境定位

## 1. 业务目标

UserApp 开发环境从每用户一套改成每应用一套。同一 app_id 的使用者共享 dev 容器、工作区、数据、日志和 agent-store；prod 仍为独立环境。

定位键是 `(stage, app_id)`，stage 为 dev/prod。内部 dev 对应 `ServiceType::UserappBuilder`，prod 对应 `ServiceType::Userapp`。应用 ID 唯一性沿用现有业务与存储契约；tenant_id/space_id 不在删除范围，不悄悄增加租户定位维度。

访问权限由既有认证授权链负责，本次不新增匿名访问语义，也不再将已移除的 user_id 当访问凭据。

## 2. 请求契约

### 2.1 删除 x-user-id

- Java 调用 UserApp 不发送 `x-user-id`。
- UserApp 不要求、不解析、不校验、不生成该 header，不以它决定路径、实例、任务范围或生命周期。
- header 缺失正常处理，不回退 metadata owner、body.user_id、query.user_id。
- 合法 HTTP 请求意外携带旧 header 时忽略其值；UserApp 转发边界按名称移除后继续转发，避免下游消费。移除不包含值解析，不因值违反旧 identifier 规则返回 400。
- HTTP 协议本身不合法的 header 仍可由 HTTP 栈拒绝，不绕过协议校验。
- UserApp OpenAPI、示例、注释、Java 接入说明均不再声明此 header；其他既有 header 规则不变。

### 2.2 删除 user_id 字段

- UserApp JSON、query、multipart 请求类型、响应业务元数据及 OpenAPI 不再声明 user_id。
- app_manager、file-server-userapp、rcoder UserApp 转发、rcoder-proxy 工具代理全部覆盖。
- 不注入固定假用户、空字符串用户或 user_id=userapp 来绕过旧逻辑，应移除调用依赖。
- 老请求多余的 user_id 作为废弃未知值忽略，不反序列化为用户类型、不校验、不回填业务状态。兼容处理要窄，不能为这一字段放弃其他严格验证。
- multipart 废弃字段直接丢弃，不建立用户业务变量；保留请求体既有资源约束。

### 2.3 URL 格式保留

现有代理 pattern 不改：

```text
/api/v1/userapp/proxy/{tool}/{stage}/{user_id}/{app_id}/{*path}
```

其中 user_id 仅是非空路径占位，不从 params 提取或校验，不进入资源名、业务日志维度或定位逻辑。路由框架匹配该段不是业务解析。

新生成 URL 固定用 `0` 占位；旧合法路径值仍可匹配。不能直接省略此段，也不能从 metadata.user_id 生成 URL。

```text
/api/v1/userapp/proxy/app/dev/0/79/
/api/v1/userapp/proxy/app/prod/0/79/
```

上述地址分别指向 app 79 的 dev 和 prod；dev/alice/79 与 dev/bob/79 命中同一 dev。未来缩短 URL 另立版本。

### 2.4 Computer 边界

普通 ComputerAgentRunner 的用户/header/定位逻辑不变。computer_intercept 中 X-Service-Type: userapp 分支必须遵守 UserApp 规则；非 UserApp 分支仍进入原 Computer 流程。

不全仓删除 user_id 或 header。公共常量若确有 Computer 消费者，可放入通用/Computer 契约；没有消费者则删除无用定义与导出，不继续作为 UserApp 对外契约。

## 3. 容器与路径

builder 基础名：`rcoder-app-builder-{app_id}`。K8s STS、Pod 后缀、Service、PVC 等按既有资源规则派生，不要求所有对象同名。

```text
userapp-workspace/dev/userapp/{app_id}/
userapp-workspace/dev/userapp/data/{app_id}/
userapp-workspace/dev/userapp/logs/{app_id}/
userapp-workspace/dev/userapp/agent-store/{app_id}/
userapp-workspace/prod/userapp/{app_id}/
userapp-workspace/prod/userapp/data/{app_id}/
userapp-workspace/prod/userapp/logs/{app_id}/
userapp-workspace/prod/userapp/agent-store/{app_id}/
```

userapp 是固定存储命名空间，不输出内部服务族字符串。容器内 `/home/user/{app_id}`、`/home/user/data`、`/home/user/logs`、`/home/user/.agent-store` 保持不变。

同一 stage/app_id 的缓存、互斥、去重、活动、挂载、查询一致；dev/prod 不能共用只以 app_id 为键的运行时缓存。已有 app 级生命周期互斥可继续统筹两族，不要求拆成两份应用 metadata。

## 4. 生命周期不变量

删除用户相等校验和 rcoder.io/owner-id 的用户注入/验证，保留：

- app_id、stage/服务族。
- lifecycle_id、operation_id、executor_id、request fingerprint。
- 物理容器 ID/Pod UID、generation、revision/resourceVersion。
- 操作租约、停止证明、资源绑定、RecoveryRequired、有条件删除。

不能整体删除 validate_identity/validate_active/资源绑定条件。并发 ensure 同一 app 只创建一个 builder；stop/restart/清理影响共享环境，旧启动和恢复不得迟到复活。

## 5. 存量边界

- 不查找、接管、恢复或通过旧复合键回退复用每用户容器，按新模型重建。
- 不自动合并旧工作区。新路径未初始化则按新工作区处理，不隐式选某个用户数据。
- 不授权自动删除旧宿主目录、数据库、PVC 或共享存储根；旧数据保留/清理由独立明确动作决定，agent PVC 规则继续有效。
- 不清空 SQL 操作/绑定或解锁未知执行。切换前收束旧 worker，在途结果未知保持保护。
- 数据库升级覆盖 PostgreSQL/SQLite 实际 schema、metadata 列、持久 JSON；保留无关字段和历史物理删除保护。

## 6. 非目标与验收

不在本轮实施完整单一 serve 架构、不改变 devrun/build、不重做 Computer/Web/Custom Page、不引入新多租户模型、不按端口杀进程、不自动清理旧用户数据。

验收要求：无 user_id/header 的完整 UserApp dev/prod 流程通过；旧业务用户值不影响结果；不同占位值同实例；dev/prod 隔离；并发单实例；旧操作不能影响新物理实例；普通 Computer 不变。

源码、组件、SQLite/真实 PostgreSQL、Compose/K8s 和发布证据分别报告，不用旧报告或 skip 代替。
