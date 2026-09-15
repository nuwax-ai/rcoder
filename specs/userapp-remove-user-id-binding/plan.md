# Plan：用户维度移除实施设计

## 1. 决策

- 资源键为 `(UserappStage, app_id)`，内部转换为 `(ServiceType, identifier=app_id)`；metadata/lifecycle 保留 app 级模型。
- 删除用户复合 identifier 和正常链路旧命名回退；保留物理执行和代次保护。
- 存储 helper 内固定 userapp 命名空间，不接任意分区字符串。
- header、JSON、query、multipart、URL 占位均不提供 UserApp 用户身份。
- 数据库与环境使用受控维护窗口切换，不承诺新旧 UserApp writer 混跑安全。

## 2. 调用链改造地图

下表为定位入口，开发时重读实际代码和 diff；初稿里的类型所属文件并不全部准确。

| 层 | 入口 | 要点 |
|---|---|---|
| 路径 | shared_types/src/paths.rs | dev/prod/data helper 只接 app_id，固定目录名 |
| key/验证 | shared_types/src/userapp/builder_instance.rs、validation.rs | 删除复合解析，统一字符/长度预算 |
| locator | shared_types/src/userapp/dev_locator.rs、dev_cleanup.rs | trait/caller 去掉用户参数；只读 probe 不触发 ensure |
| builder | rcoder/src/userapp_builder/{mod,creation,control,adoption,dev_locator,dev_cleanup}.rs | ensure、fingerprint、恢复、清理去掉 owner/协作者双轨 |
| 转发 | rcoder/src/userapp_forward/{forward,upstream,semantics}.rs | tasks/static/new endpoints/header 所有来源删除；computer_intercept 的 UserApp 分支覆盖 |
| 工具代理 | rcoder-proxy/src/router/userapp.rs、service/handlers/dev_app_proxy.rs、dev_terminal.rs 及 prod handler | pattern 不变；不读占位 user_id；HTTP/WS 全部核对 |
| 容器侧 | file-server-userapp/src/models/request.rs 及 handlers | DTO、multipart、task scope、日志、OpenAPI |
| 生产管理 | app_manager/src/models、handlers、service、ops、lifecycle | create/update/query/upload/storage/deploy 不再要求 owner；URL 固定占位 |
| 共享生命周期 | shared_types/src/userapp/{lifecycle,builder_control,resource_binding,metadata}.rs 等 | 删除用户维度，保留执行和物理绑定 |
| 存储 | rcoder-storage/src/pg/userapp/repo/metadata_repo.rs、src/userapp_lifecycle | 两后端、共享 SQL macro、JSON、alias/fingerprint、migration |
| Docker | docker_manager/src/agent_container_starter/mounts.rs、runtime/docker_app_mounts.rs 及查询清理 | UserApp bind/label/env/注册不使用用户；公共 Computer 字段不全局删 |
| K8s | runtime/k8s_agent_create.rs、k8s_agent_env.rs、k8s_agent_query.rs、k8s_pod.rs、k8s_service.rs、k8s_statefulset.rs、k8s_app_operation.rs、k8s_builder_deletion.rs、PVC 模块 | UserApp 无用户 selector/env/annotation/subPath，始终保留服务族 |

全仓搜索 user_id/owner/USER_ID/复合键，建立剩余命中 allowlist。目标不是全仓零 user_id：Computer、历史 migration、兼容测试可合法保留，逐项说明。

## 3. Header、DTO 与 Java

删除 UserApp explicit_user_id_from_headers 及无用 helper。请求确认属于 UserApp 后按名称移除 x-user-id，不读取值；rcoder-proxy 的上游请求构造不重新注入。普通 Computer 在专属清理之前分流，禁止全局 middleware 清空 header。

当前 USER_ID_HEADER 定义位于 UserApp forward_contract，shared_types 有 re-export。核对消费者后，无用途就删除；若普通 Computer 确需常量，移入通用/Computer 模块，不保留 UserApp header 契约。不能用“也许有人用”保留死代码。

删除 require_query_user_id、require_static_user_id、body_user/query_user 和 owner 回退。只解析原有 app_id/stage/其他必要字段。流式上传/下载/WS 保持原模式，不为删除用户字段把所有请求体缓冲成 JSON。

UserApp DTO 删除 user_id；旧字段兼容采用忽略未知值或窄范围废弃字段过滤，不能保留 String user_id 后读取但不用，也不能整体关闭其他 strict 校验。手工 multipart 删除 required 检查，旧字段丢弃。生产上传、AppOwnerQuery、存储请求、响应 metadata 全部覆盖；tenant/space、销毁确认参数、app_id 验证不变。

Java 接入约定：

1. UserApp 不传 x-user-id，body/query/form 不传 user_id。
2. app_id、stage 的既有来源及其他 header/认证规则保持；不要求每个接口新增 stage 字段。
3. 代理 URL 旧段位保留，新生成 URL 固定 `0` 占位。旧非空占位不读取、不校验。
4. UserApp 模式 `/api/computer/*` 同样无 header；普通 Computer 保持原契约。
5. Java 不再按用户缓存开发容器地址；同 app 的 stop/restart/clear 是共享环境操作。
6. Java 代码由负责同事修改；本仓只交付契约/示例及验收，不声称 Java 已上线。

## 4. 路径、命名与缓存

目标 helper：

```rust
pub fn userapp_dev_subpaths(app_id: &str) -> [String; 4];
pub fn userapp_prod_subpaths(app_id: &str) -> [String; 4];
pub fn userapp_prod_data_subpath(app_id: &str) -> String;
```

builder_instance_id 可删除或保留纯 app_id 包装，不再 rsplit_once('-') 猜用户。字符与路径穿越校验不变，不顺便放宽字符集。

目标 app_id 上限按原需求为 33，但当前公共常量是 22。统一核验 STS/Pod revision label、Service、PVC、锁、容器名等最严格预算，再调整全部入口。33 合法/34 拒绝只有在所有派生名合法后成立；遇更严格限制需记录证据并统一约束，不能各入口规则不同。

MountContext/ContainerCreateParams 等公共类型服务 Computer 时不全局删 user_id；UserApp 分支不填假用户、不从它派生挂载。Docker bind、K8s subPath、PGDATA/agent-store、存储统计/孤儿检测/clear/destroy 使用同一 helper。

运行时缓存、注册、锁、selector 保留服务族，防同 app 的 dev/prod 命中同一资源。保留 K8s 既有双键 selector；UserApp 不落入不带类型的旧 pod_id/user_id/project_id fallback。其他服务族根据原行为保留。

清理只针对捕获的新布局目录/物理身份，不能为了删协作实例扫描 dev/*/{app_id} 并跨用户清理。旧清理工具若保留需独立调用与明确归属，不在正常 purge 自动执行。

## 5. 生命周期和并发

ensure_identity 只接 app_id；删除 validate_owner 后保留 Active/lifecycle/revision 等条件。resource metadata/binding 不再注入 owner-id，但 application-id、service family、lifecycle、operation/executor 等继续验证。不能因旧资源缺字段就放宽接管。

ensure/probe/create/restart/stop/cleanup/recovery 统一 app 级 builder；删除 metadata owner 与协作者实例双路径。高层 metadata 缺失时按已有应用注册规则处理，不重新解析用户。

新 fingerprint 只包含有效业务输入、应用/服务族和执行上下文；header/body/query/URL user 占位不得改变摘要。旧 fingerprint 和请求 alias 按版本处理，不能直接认为与新摘要等价。

必须证明两个并发请求只建一套、旧 lifecycle 停止/删除不影响同 app 新物理实例、stop 与旧启动仍受保护。不把去掉用户校验当作简化物理保护的理由。

## 6. 数据库升级

### 覆盖面

1. PostgreSQL metadata user_id 列及 upsert/select/row 类型，实际存在的其他 metadata 后端也核对。
2. PostgreSQL/SQLite lifecycle、operation、input、resource binding、alias 等表和序列化记录；userapp_lifecycle/sql.rs 是共享实现，不只属于 SQLite。
3. 持久缓存、旧请求摘要、复合 identifier、资源 annotation。

历史 migration 不改，追加版本。保留名称、tenant/space、时间、generation 及无关数据。

### 迁移规则

- 维护窗口停 UserApp 写受理和旧恢复 worker，等待已受理操作结束；只停 HTTP 不足。
- 记录/备份 schema 版本及迁移前数据，用真实旧 schema/JSON fixture 验证，不只测空库。
- JSON 按已知版本/字段位置移除用户字段，再用新类型验证；禁止递归删全部 user_id key，嵌套用户应用配置可能合法使用该名称。
- 当前 UserAppResourceBinding 有 deny_unknown_fields；先迁移记录或提供受控旧版本 reader，再切严格新 reader，避免旧 user_id 导致反序列化失败。
- 不将 pending 改成功、不清 RecoveryRequired、不将旧 physical UID 改成新实例、不因转换字段而接管旧资源。
- 旧待执行输入/alias/fingerprint 标识为旧身份版本，不自动转换重放；保留历史证据。仅在旧生命周期已明确收束/退役后建立新生命周期。
- 旧执行停止无法确认时保持阻塞，不能删除记录解锁。
- DDL/记录版本更新有事务或可恢复边界，重复启动不重复损坏。两数据库分别测升级、中断和重启。

metadata DROP 列在旧 writer 退出后执行。可分为新代码不使用旧列、最终删除两步，但不承诺删列后旧代码还能运行。历史原始记录可保留备份/归档，新活跃模型不继续使用用户字段。

## 7. 环境切换：不迁移旧容器

1. 明确环境/应用清单，不自动操作已有用户应用或共享资源。
2. 停受理并排空旧 worker，捕获旧物理实例和数据路径。
3. 对已授权旧实例停止并确认；旧名字查不到不能证明全部执行已停止。
4. 升级数据库以及真正处理请求的 rcoder/app_manager/proxy/file-server/agent_runner 层，禁止新版转发给仍必填用户的旧服务。
5. 新版只生成新名字和新路径；旧路径不回退复用，新工作区重新初始化，不复制/合并旧用户数据。
6. prod 基础名即使与旧名相同，也要验证新 UID/lifecycle；不自动 adopt 旧容器/PVC 绑定。
7. 完成无 header、共享 dev、独立 prod、真实挂载与数据库验收后开放。
8. 旧数据/PVC 后续清理由独立明确动作管理，agent PVC/共享根保护不变。

回退需停新版 writer、确认实例停止、恢复兼容 schema/记录和明确配置；不能只回滚二进制。未知状态禁止同时拉两套 writer。

## 8. 与现有工作协调

保留正在进行的启动错误传播修复，特别 Child 监督、sender 释放、deadline 和取消保护；重叠文件按改动块整合。

本次身份简化先于运行态单一 serve 协议定型；新协议以 stage/app_id 加物理实例/代次为身份，不再引回用户。无需在此任务顺便实现完整 serve 架构。

现有 AGENTS、生命周期和并发规范中的执行保护继续有效；其中与用户复合键业务模型矛盾的旧描述，以本目录为新要求，并在实现时定向同步相关文档。
