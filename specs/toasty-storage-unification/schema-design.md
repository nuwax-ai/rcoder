# 首次正式发布前的库表设计

日期：2026-09-19。状态：建议采用的设计基线，待实现与双后端验证后冻结为 v1。

本文件补充并修正 [plan.md](plan.md)：不再要求原样保留当前表形状。目标是在 9 月底首次数据库版本发布前，完成核心 schema 整理及 Toasty 迁移。修改范围是设计文档；本文不是已执行的数据库迁移。

## 1. 结论与边界

采用“控制字段关系化、可变载荷 JSON 化、业务事务共用、运行态身份独立”的设计。

- 保留 UserApp、Project/Session/Container、Preview 三个业务域。
- 不加通用 entity/property/EAV 表，不为所有对象建立通用事件溯源框架。
- UserApp 仍以 app_id 标识业务应用；dev/prod 是同一应用的两个独立运行环境，Application 操作可以同时保护两者。
- lifecycle_id 表示应用代次，不是 dev/prod；不能把 scope 当 lifecycle，也不能因相同 app_id 合并两个容器的操作锁。
- 上一正式版本没有数据库，无线上历史数据迁移、双写或旧 ORM 兼容需求。用户已授权个人测试 K8s 的 RCoder 专用 PG 清空重建，按 [基线切换方案](schema-baseline-and-test-reset.md) 执行；其他开发目录/schema 不自动覆盖和删除。
- PG/Kubernetes feature 关系不变。PG 域的 ProjectStore/Preview 不因本次统一 ORM 自动扩展为 Compose Turso 持久化。

## 2. 当前问题及处置

| 当前设计 | 本次决定 | 源码依据 |
|---|---|---|
| UserApp 核心身份/状态在整条 record TEXT 中 | 控制列独立；去掉整条权威 record 与 terminal 镜像字段 | migrations-userapp-turso/0001_init.sql:5；shared_types/src/userapp/lifecycle.rs:184,366 |
| active_operations 在 JSON 内 | 独立的三槽位行；FK、CHECK + 同事务维护 | shared_types/src/userapp/lifecycle.rs:207；userapp_lifecycle/domain.rs:153 |
| operation 子表分别引用 app 与 operation，未约束两者一致 | 复合外键绑定 app/lifecycle/operation | migrations-userapp-pg/0003_execution_inputs.sql:2 |
| request aliases 与 recreations 分表维护同一请求命名空间 | 合并成 userapp_requests，主键统一限制复用 | userapp_lifecycle/sql.rs:219,584 |
| activity 批量覆盖 last_accessed/stopped/wake_blocked | 只持久化带代次的活动时间；控制意图来自 lifecycle，物理 stopped 来自运行时 | pg/userapp/repo/activity_repo.rs:15；userapp_lifecycle/domain.rs:267 |
| Preview 活跃端口普通索引，先查后写无全局排他 | 活跃端口唯一索引 + PG 短事务分配锁 | migrations-preview-pg/0001_preview.sql:26；preview_lifecycle/postgres.rs:103 |
| Preview 首次创建无现存行可锁，upsert 可覆盖并发赢家 | 分配锁覆盖首次创建；重读再条件更新，不无条件覆盖 | preview_lifecycle/postgres.rs:110,170 |
| publish_tasks 无消费者 | 新 schema 不创建 | pg/userapp/mod.rs:11 |
| userapp_metadata 仅用于旧库导入 | 删除旧表与导入路径，元数据只保存在 userapps | userapp_lifecycle/postgres.rs:14；当前全仓调用检索 |
| sessions.container_name 冗余且当前回源从 project 查容器 | 删除持久列，内存视图按关系重建 | pg/project_store/repo/rows.rs:53；repo/store_repo.rs:375 |
| containers 用 created_at 比较判断新旧 | 引入显式 container_generation/predecessor；时间不授权代次覆盖 | pg/project_store/repo/store_repo.rs:28 |

源码路径默认前缀 `crates/rcoder-storage/`；shared_types 例外。行号为审查基线 d9e8fd32，实施前复核。上表描述源码事实与改造决定，不宣称已复现所有竞态。

## 3. 全局约定

### 3.1 类型、命名与权威字段

- ID 使用非空 TEXT，保留现有 app/project/session/operation ID 格式；不引入自增 ID 替代现有业务身份。大小写按现有契约处理，不自动 lowercase 用户 ID。
- revision/epoch 使用正数 i64；递增必须检查溢出。业务版本号不能用时间戳替代。
- 时间统一为 UTC Unix 微秒 i64，列名 `*_at_us`；外部 API 继续原 DateTime 表示。现有绝对期限保留 `deadline_ms` 单位，转换要显式且检查溢出。时间只用于观察/预算，不作为所有权凭据。
- 逻辑 BOOL 映射为 PG BOOLEAN / Turso INTEGER + CHECK IN(0,1)。整数映射 PG BIGINT / Turso INTEGER；不依赖隐式字符串转数字。
- 枚举使用稳定 snake_case TEXT + CHECK，不存 Rust enum 序号。UserApp operation 状态仅 `pending/running/waiting_retry/recovery_required/succeeded/failed`；不得直接套用 app-cli 的 Cancelled 状态。
- JSON 载荷首版可统一 TEXT，通过 serde 严格解码并记录 payload_version；不依赖 PG JSONB 特性完成控制筛选。未知有效载荷版本明确报错并保护运行态。
- 独立列是唯一权威来源；不得又在一个 record_json 中完整复制 state/revision/scope/identity。历史请求意图中的 expected_revision 是不可变请求事实，不是当前控制状态副本。
- SQL CHECK 遇 NULL 可能不拒绝，必填约束用 NOT NULL；可空复合身份需额外检查“同时为空或同时有值”。
- 不依赖 deferred FK、跨表 CHECK、触发器或生成列实现核心协议。Turso 0.7.2 不支持 PRAGMA defer_foreign_keys/foreign_key_check；使用普通即时约束和明确语句顺序，逐项探针验证。

### 3.2 schema 版本

一张 `rcoder_schema_migrations`：`(component, version)` 主键，包含 name、checksum、applied_at_us。component 固定为 userapp/project/preview，按启用的部署能力初始化；历史版本变更不得改原文件内容。

检查未知 component/未来版本、checksum 不一致、已启用域缺表和关键约束。不能仅看到迁移表存在就认定 schema 完整。PG 在迁移锁下检查并应用；Turso 先持目录锁。DDL + 版本记录同事务。

## 4. UserApp 表组（9 张业务表）

### 4.1 userapps：当前应用身份、元数据及已提交策略

| 列 | 约束/语义 |
|---|---|
| app_id | PK，业务应用身份，删除后保留此行 |
| lifecycle_id | NOT NULL；当前代次；与 app_id 组成 UNIQUE |
| lifecycle_epoch | >=1；仅显式 recreate 增加 |
| lifecycle_state | active/deleting/deleted |
| metadata_revision | >=1；元数据与已应用策略 CAS |
| name / tenant_id / space_id | nullable；无 user_id 字段 |
| recycle_enabled / wake_on_traffic | nullable BOOL，NULL 表示沿用已定义的运行时默认语义 |
| idle_timeout_seconds | nullable，非负范围及业务允许范围在输入校验中处理 |
| created_at_us / updated_at_us | 当前生命周期创建/更新时间；recreate 延续当前重置 created_at 的语义 |

不持久化物理容器是否 Running；运行时 API 才是该事实的来源。runtime_policy 对外结构由策略列重建。当前策略仅服务既有 prod 功能，不增加 dev 回收行为。

PG 所有同 app 的短控制事务先锁该行；Turso 单 worker + BEGIN IMMEDIATE。DB 锁仅覆盖数据库工作，不跨 Docker/K8s/HTTP 操作。

历史操作通过 `operations.app_id -> userapps.app_id` 关联稳定业务身份，不 FK 到 userapps 当前 lifecycle_id。否则 recreate 会被历史 FK 阻止。v1 不增加完整 lifecycle 历史快照表；历史代次保留在不可变操作身份、重建请求映射和资源绑定中。

### 4.2 userapp_operations：操作身份与状态

必填列：`operation_id PK, app_id, lifecycle_id, kind, scope, state, revision, request_fingerprint, step, created_at_us, updated_at_us`。

可空列：`origin_request_id, executor_id, error_code, error_message, terminal_at_us`。

JSON 列：`command_json, admitted_metadata_json, runtime_policy_on_success_json, checkpoint_json`，各载荷有清晰版本；checkpoint 不放凭据。原 operation.request_id 映射为 origin_request_id，表达首个请求；重放权威索引是 requests 表。

- FK app_id → userapps；UNIQUE(operation_id, app_id, lifecycle_id) 供子表绑定确切身份。
- kind 与 scope 的对应由当前 OperationKind::scope() 生成/校验固定映射，不由客户端选择；DDL CHECK 与映射全量一致性测试必须覆盖新增 kind。
- state/revision 独立列；`state in (succeeded,failed)` 当且仅当 terminal_at_us 非空。删除 terminal 字段，不能有两种终态判断。
- Running 必须有 executor_id；其他状态是否需要 executor 按原状态机验证，不机械套用泛化规则。
- 历史操作 identity 列不可修改；更新按 app/lifecycle/operation/executor/expected_revision/state 条件 CAS。
- scope 与 record state 一致性使用领域转换校验；未知/损坏记录不得视为无操作。

### 4.3 userapp_active_operations：当前三槽位

一行一个 app：`app_id PK, lifecycle_id, dev_operation_id?, prod_operation_id?, application_operation_id?`。

- FK(app_id,lifecycle_id) → userapps 同列 UNIQUE（只允许当前代次槽位）。
- 每个非空操作指针用 FK(operation_id,app_id,lifecycle_id) → operations。
- CHECK：Application 占位时 dev/prod 必须为空；dev/prod 可以同时非空；非空指针不能重复引用同一个操作。
- 槽位与操作 scope/非终态的对应无法仅用普通跨行 CHECK 表达，仍由统一事务逻辑校验，启动/扫描遇不一致保持保护并报错；不得删槽“自愈”。
- 新 operation、请求映射、执行输入、槽位受理同事务；终态、策略更新、清除自己的槽位同事务。
- RecoveryRequired 属于非终态，保留槽位。deadline/租约超时不删除槽位。

核心 CHECK 示意（逻辑 DDL，最终需要两引擎探针）：

```sql
CHECK (application_operation_id IS NULL OR
       (dev_operation_id IS NULL AND prod_operation_id IS NULL)),
CHECK (dev_operation_id IS NULL OR prod_operation_id IS NULL OR
       dev_operation_id <> prod_operation_id)
```

为什么独立一行而非三条 scope 行：一行可直接表达 Application 互斥，且读取可还原现有固定三槽位契约。单独 UNIQUE(app_id,scope) 不能防止 Application 与 dev/prod 并存。

### 4.4 userapp_requests：唯一幂等命名空间

主键 `(app_id,request_id)`；request_id 不超过当前 128 字节语义，非空；不加 scope 或 lifecycle 到主键，避免无意允许原本禁止的 request_id 跨环境/跨重建复用。

列：`target_kind(control/recreate), operation_id?, lifecycle_id?, previous_lifecycle_id?, new_lifecycle_id?, created_at_us`。

- control：operation_id/lifecycle_id 必填，previous/new 必须 NULL；FK(operation_id,app_id,lifecycle_id) → operations。指纹与参数比较读取所指 operation，避免再复制一套权威指纹。
- recreate：operation_id/lifecycle_id 必须 NULL，previous/new 非空且不同。保存已确认重建结果；重放仍验证请求预期旧代次和当前代次，不能自动再次重建。
- CHECK 确保两类字段形状互斥；app_id FK → userapps。
- EnsureBuilder 合流可有多个 request_id 指向同一操作，沿用现有 fingerprint/command/metadata 一致校验；RecoveryRequired 不允许合流绕过保护。
- operation_id 与 request_id 如果指向不同操作，明确拒绝。request_id=None 仍支持按 operation_id 的既有幂等语义。
- 合并替代 operation_requests 和 recreations；删除旧 operations 上 UNIQUE(app_id,request_id)，不再维护两套请求唯一机制。

### 4.5 userapp_operation_inputs

`operation_id PK, app_id, lifecycle_id, payload_version, payload, payload_digest, created_at_us`；复合 FK → operations。

包含敏感执行输入，不随 operation 查询返回。读取先核验执行 context，再核对 digest；成功/失败终态事务中删除不再需要的输入，RecoveryRequired 保留恢复必需内容。日志、普通数据库诊断和报告不得输出 payload。

### 4.6 userapp_operation_leases

`operation_id PK, app_id, lifecycle_id, executor_id, request_fingerprint, receipt_version, receipt_json, created_at_us`；复合 FK → operations。

executor/fingerprint 是绑定时的不可变凭据快照，不随 operation 的新执行者自动更新。receipt_json 保留 Docker device/inode/token 或 K8s namespace/name/uid/resource_version/token，不能用 app_id 批量删。

先确认对应外部租约已条件释放，再按确切凭据删除记录；DB 删除不等于释放外部锁。失败/未知保留记录。

### 4.7 userapp_operation_deadlines

`operation_id PK, app_id, lifecycle_id, deadline_ms`；复合 FK → operations。首次绑定不可变，重试读取原 deadline，不更新为 now+timeout。

保留独立表以强化 bind-once 权限和方法边界；不必为减少一张表把 deadline 混入频繁覆盖的 operation payload。

### 4.8 userapp_resource_bindings

主键 `(service_type,physical_uid)`；列 `app_id,lifecycle_id,adopted_by_operation,created_at_us`，通过复合 FK 引用收养操作。现有仅 builder 收养的服务类型限制继续执行，不虚报对任意资源的收养能力。

physical_uid 必须来自真实运行时身份，不能用名称/IP/端口代替。绑定不可覆盖，禁止级联删除；新 lifecycle 不能拿旧绑定替代新收养证据。

当前单运行时权威域的部署范围不变。如果将来一个 RCoder DB 管多个独立 Docker daemon/集群，需要增加 runtime_identity；此能力当前不启用，不临时用空串扩大身份范围。

### 4.9 userapp_activity

主键 `(app_id,lifecycle_id,scope)`，scope 为 dev/prod 的固定词表，首版消费方只接 prod；仅含 `last_accessed_at_us,updated_at_us`，不含 stopped/wake_blocked。

本期保持既有持久化装配范围：PG 使用该表，Compose 的活动追踪不因统一 ORM 自动切换为 Turso 持久化。两个后端可共享表模型定义，是否启用后台持久化由明确能力矩阵决定；不能把空表存在称为功能已启用。

- 关联 userapps 当前 `(app_id,lifecycle_id)`；每条 touch/flush 都携带捕获的 lifecycle，拒绝从当前 app_id 重新猜代次。
- 同代次活动时间只取较大值，旧批次不能把时间写回过去；新代次必须重新开始，不能继承旧批次。
- durable 控制权威来自 userapps 已提交策略与 active operations；物理 stopped 来自运行时身份匹配的观察；本地 stopped/wake flags 可保留为缓存，不能通过定时 flush 改写控制意图。
- 需要调整 shared_types::ActivityRow/ActivityPersistence 及采集缓存。删除也带 lifecycle；同 app 新旧代次的 dirty 项不能合并。
- 不能给每个 HTTP 请求增加同步查库：在已核验的代理路由目标中传递 lifecycle，或使用明确失效的路由身份缓存；取不到可信代次时不伪造 touch。缓存缺失要可观测并触发刷新，不能悄悄让回收判据永久失真。

当前 domain::advance 已在成功提交时更新 wake_on_traffic，所以优先复用该权威，不再额外增加一张重复的 wake 控制表。开始/停止期间的瞬态保护由操作槽位保证；显式拒绝保持旧策略；不确定结果保留槽位。

## 5. UserApp 关键事务顺序

### 创建/受理

1. 原子创建 userapps 根行；读 lifecycle/state/control_revision 后以条件 UPDATE 递增 control_revision。竞争失败整笔回滚后从原请求重读，最多三次；Turso 仍使用 BEGIN IMMEDIATE。创建根与空槽位同事务。control_revision 独立于 metadata_revision，重建不重置。
2. 校验当前 lifecycle、状态；按 requests/operation 两种身份重查幂等。
3. 读三槽位并执行 scope 矩阵与领域校验。
4. 新建 operation；可选写请求映射与私有输入；设置自己的槽位。
5. commit 后才允许业务执行者获取/绑定外部资源并发生副作用。

### 推进/终态

根控制版本 CAS → 重查 operation/context/revision → 验证领域状态转换 → CAS 更新 operation；若终态则同时应用策略、只清自己的槽位、删除允许清理的 inputs。外部租约收尾仍走原有 receipt 协议，不在事务内做网络 I/O。

### 重建

根控制版本 CAS → 检查 deleted 且三个槽位空 → 验证请求命名空间 → 精确删除旧代次的空槽位/activity → 更新 userapps 当前 lifecycle/epoch/metadata → 插入新空槽位与 recreate 请求映射 → commit。

历史 operations/requests/leases/bindings 不删除、不重新绑定。FK 立即生效时按上述顺序执行；不能改成 ON UPDATE CASCADE，把旧运行身份自动“升级”为新代次。

## 6. 查询与索引（按现有调用链建立）

| 表 | 首版索引/约束 | 服务的查询 |
|---|---|---|
| userapps | PK(app_id)、UNIQUE(app_id,lifecycle_id) | 当前身份、复合 FK、按 app_id keyset 分页 |
| operations | PK、UNIQUE(operation_id,app_id,lifecycle_id) | 精确查询和子表关联 |
| operations | (app_id,lifecycle_id,created_at_us,operation_id) | 单应用操作历史 |
| operations | (state,operation_id) | 单状态扫描；全非终态需按实测查询计划选择下项 |
| operations | 部分索引(operation_id) WHERE state IN 非终态 | 多种非终态合并的 keyset 扫描，必须在两引擎验证后采用 |
| requests | PK(app_id,request_id)、(operation_id) | 幂等、操作关联 |
| slots | PK(app_id) | 当前 scope 冲突、启动重建 |
| leases/inputs/deadlines | PK(operation_id)，关联方向按实际 join 补索引 | 精确 lookup、终态租约 join 扫描 |
| bindings | PK(service_type,physical_uid)、(app_id,lifecycle_id) | 所有权查询、单应用诊断 |
| activity | PK(app_id,lifecycle_id,scope) | 批量更新/启动恢复 |

不要同时机械建立冗余索引；T0 用接近实际历史/活动数据比例的样本与 EXPLAIN 确定 operations 两个扫描索引是否都需要。索引形状要匹配真实 predicate/order，不以“建了 state 索引”当作性能证明。

`list_control_snapshots` 用 userapps + slots + 三个 operation 左连接的单条一致快照，分页仍按 app_id；不依赖 JSON 路径。独立查询得到的拼接结果不能作为恢复授权。

## 7. Preview（2 张表，PG 模式持久化）

保留 preview_instances 和 preview_operations；不与 UserApp operation 共表，不把 Preview uncertain 等同 UserApp RecoveryRequired 枚举。

### 表结构修正

- instances 保留当前字段，时间遵循全局单位；`revision>=1`、有效端口范围 1..65535、必要字段非空。
- `instance_id` 全局唯一；preview_key 当前身份一行。历史操作新增明确的 instance_id 和请求 fingerprint/必要输入字段以诊断归属，不 FK 到只能保留当前实例的 `(preview_key,instance_id)`。
- operations.preview_key 可 FK 到稳定保留的 instances.preview_key；instances.operation_id 是当前操作指针，按“插入/更新实例占位 → 插入操作 → 写指针”处理，必要的暂态 nullable 不暴露到事务外。不强制引入双向非空 FK 循环。
- active 状态 starting/ready/stopping/unknown 必须占用非空 port；unknown 不能因为超时就释放。
- 保持当前路由约定：活跃 port 全局唯一，不擅自缩成 host_id+port。

```sql
CREATE UNIQUE INDEX preview_active_port_unique
ON preview_instances(port)
WHERE state IN ('starting','ready','stopping','unknown');
```

### 并发协议

PG 在 accept_start 的短事务开始取得固定命名空间的事务级端口分配 advisory lock，然后才读取 preview_key、端口集合并分配。它同时覆盖不同 key 争端口和同 key 首次创建；不跨进程启动/Ready 等待。所有新占用端口的入口都遵守该入口，不允许旁路写 starting。

锁内重读已有状态：已 running/starting 则返回既有或结构化冲突；新实例写入使用预期 instance/revision/state 条件，禁止无条件覆盖另一个已受理实例。唯一索引作为最终保护。冲突后的重试使用新事务（PG 错误事务必须 rollback），有限次尝试且共用 deadline。

端口释放由身份匹配的停止证据决定；清理与另一受理竞争以数据库提交顺序为准。停止失败保持 stopping/unknown，不能为了让测试分到端口提前置 failed。

## 8. Project、Session、Container（PG）

保留三张主表 + 三类 tombstone，不扩成所有对象共用 JSON 大表；现有内存镜像接口保留。

### containers

保留 container_name PK，增加 `container_generation`（非空、不可变的登记代次，UNIQUE(container_name,container_generation)）；container_id 保持 nullable，真实物理 ID 一旦绑定不得被旧占位快照清空。

container_generation 与真实 UID 用途不同：占位记录已经有登记代次，后续绑定 UID 需要 CAS 预期空值与同代次。已绑定 UID 换代必须新 generation + 明确 predecessor，不能用 created_at 较大作为依据。

保留 logical_id、规范化 service_type、IP/URL/端口/状态等路由快照；这些观察字段不是删除资源的授权。使用 row_revision 做字段更新 CAS；活动时间单独取 max，不随旧全量快照覆盖。

索引 `(service_type,logical_id)` 按实际归一键查询；container_id 非空的物理定位索引。不在尚未证明“一逻辑键永远一物理实例”的情况下加该逻辑键 UNIQUE。

### projects

保留 project_id PK、generation、user_id/pod_id/tenant_id/space_id/isolation_type、service_type、model_provider 配置、agent_status、request_id、时间与版本。

UserApp 退役 user_id 不代表通用 Computer 项目也要删除 user_id；保留其真实消费者。

- UNIQUE(project_id,generation)；与 sessions 形成复合 FK。
- 引用容器使用 `(container_name,container_generation)`，两者同时 NULL 或同时非空；FK → containers。不能只有名称就把旧 project 绑定到同名新容器。
- 删除/替换容器时先按确切 predecessor 和项目代次处理引用（解除关系或走已批准删除流程），再替换容器行；不使用 ON UPDATE CASCADE 自动迁移旧身份。
- `latest_session` 是“当前选定会话”业务字段，可保留；不是按 wall clock 猜最后一条。写入/删除 session 与调整该指针同事务，验证属于同 project/generation；启动加载保留既有 latest 最后回放语义。
- model_provider/agent_status 是合适的 JSON 载荷；含凭据的配置归为私有数据，不进入通用查询响应、SQL 参数日志或测试报告。

### sessions

保留 session_id PK、generation、project_id、project_generation、创建/访问时间；删冗余 container_name。

FK(project_id,project_generation) → projects；旧代次 session 不能指向新 project。会话查路由：session → 同 generation project → 同 generation container；DB 回源后可在内存形成派生视图，保持热路径性能。

### tombstones 与延迟写

project/session tombstones 保留 `(id,generation)`；container tombstone 扩展为能够保护登记 generation 与已知 physical_uid，未知 UID 的占位退役也不能复活。为避免变体 nullable 主键，可用 `(container_name,container_generation)` 主键 + nullable physical_uid 索引。

writer 的全量快照需要改为携带预期 generation/revision 的写意图。CAS 冲突不能简单将旧快照贴上新 revision 重试；重读后仅合并仍有效字段，过时代次明确 Superseded。活动 touch 与配置/routing 变更分开，不能用“刚访问过”授权旧配置覆盖。

保留批处理、poison 隔离、durable/降级、leader/sync/回源流程；只替换其数据契约和持久化实现，不删掉这些能力。

## 9. 保留与删除规则

- v1 不做按固定天数自动删除 operation/requests/tombstone/binding。先保住幂等与防重放；用有界分页、指标和容量告警管理增长。
- 私有 inputs 在确定终态后按现有契约清理；不确定操作保留恢复必需输入。
- lease 必须外部准确释放后删；binding 不随 compute/storage 删除级联消失。
- 重建只切换当前 app 代次，不复用旧请求身份，不让旧 dirty activity 更新新代次。
- 后续若增加历史清理，须一起定义幂等有效期、旧写最大存活时间、引用关系和历史查询行为，单独迁移/验收；不能先做 7 天 TTL 再补协议。
- 数据库不必永久不可变：正式发布后用有序迁移演进，禁止原地修改已发布初始化 schema/checksum。首次冻结的是身份、语义和升级纪律，不是假定以后永远不加字段。

## 10. 实施与发布门禁

1. 先按本文核对表/字段清单与消费者，生成新的 `0001` 逻辑 schema 和两后端 DDL。按 [SQL 基线方案](schema-baseline-and-test-reset.md) 将历史文件收敛为四个初始化文件。旧开发表不迁移到新 schema；测试 PG 重建，Turso 新目录。新的 app schema 与引擎版本标识明确。
2. T0 增加复合 FK、nullable CHECK、active port UNIQUE、端口并发、当前/历史 lifecycle、slot FK 和索引查询计划探针。Turso 必须真的拒绝反例，不是只接受 CREATE TABLE；每条新连接设置/回读 foreign_keys。
3. 迁移 UserApp 共用事务，重建对象投影，再迁移 Activity/Preview/ProjectStore 消费者；不能只改 DDL。
4. 删除旧 JSON 整记录写入、退役 metadata/publish 表和无消费者 trait/import；删除全部 SQLx 依赖。历史文档保留，现行规范更新。
5. 两后端共同契约、真实 PG 多副本、Compose 全套与 remote K8s 业务验证通过后，标记 schema v1 冻结并记录版本/DDL checksum/镜像身份。

### 必须有的反例

| 编号 | 反例与预期 |
|---|---|
| S01 | 跨 app/lifecycle 引用 input/lease/slot → DB 或统一事务拒绝，不留半写 |
| S02 | dev/prod 同时受理成功；Application 与任一占位互斥；RecoveryRequired 不放行 |
| S03 | 同 request_id 不同参数/跨 control-recreate 拒绝；合流请求都可重放 |
| S04 | recreate 后旧 operation 可查；旧 executor/touch/forget/retry 不能操作新代次 |
| S05 | 两 PG store 同 preview_key 首创只有一个赢家；不同 key 同端口只有一个占用 |
| S06 | unknown preview 持续占端口；确切停止后可再分配；旧 stop 不能清新实例 |
| S07 | 同代次乱序 touch 时间不回退；旧快照无法改变 wake policy |
| S08 | container 同名重建、空 UID 占位迟到、旧 project/session 队列重放不错误重绑 |
| S09 | CAS 失败后不能贴新 revision 覆盖新数据；无关字段 patch 不丢更新 |
| S10 | 事务中每个关键写点失败全回滚；schema 校验/约束缺失 fail-fast |
| S11 | 删除/重新建立 current slots 的中间失败回滚；旧历史 FK 不阻止合法 recreate |
| S12 | 控制域分页/终态租约扫描在大历史小活跃数据集有合理查询计划；不靠加载全表 |

本轮没有执行这些测试；schema 在全部必要门禁通过前保持“待验证”，不能因为文档齐全称为发布就绪。

## 11. 参考

- [PostgreSQL 约束说明](https://www.postgresql.org/docs/current/ddl-constraints.html)：普通 CHECK 只约束当前行；跨表关系用 FK；部分唯一性用 unique partial index。
- [Turso 0.7.2 兼容清单](https://github.com/tursodatabase/turso/blob/v0.7.2/COMPAT.md)：以目标版本验证，不用 main 的能力替代。

本文件是 RCoder 具体设计建议；以上参考只支撑数据库机制，不代表上游已经验证 RCoder 业务。


并发重构与外键取舍详见 [UserApp 并发控制](userapp-concurrency.md)。

## 2026-09-19 Activity 实现补充

- AppState 仅在 UserApp 后端明确解析为 PostgreSQL 时注入 ActivityPersistence，Compose/Turso 不扩展为活动时间持久化。
- 代理 touch 使用容量 10000、TTL 5 秒的 lifecycle 查询缓存，同 key 并发 miss 合流；命中不查库。key 含本地失效版本，bind 换代和 delete 清理更新版本。查询返回后在身份锁内再次比对版本与 lifecycle epoch，迟到结果不能重新登记已被本地删除的身份。跨副本变化在 TTL 后重查，缓存期间旧代次 touch 仅携带旧 lifecycle，持久层不允许改绑到新代次。
- 缓存 miss 记录 debug 刷新日志，失败记录 warn 并不缓存错误；触碰失败返回 None，保活接口不能伪造当前时间返回成功。下一次请求仍可重试刷新。
- 删除活动记录携带操作 lifecycle；迟到的旧生命周期删除完成事件不能清理本地已登记的新 lifecycle。
- 周期 flush 与关机最后一次 flush 复用同一实现。删除失败重排未完成项，写失败只对仍匹配原 lifecycle 的项重新标脏。所有生产者与操作收束后、控制存储关闭前进行最终 flush，失败不报告关机成功。
- 回收扫描合并其他副本活动时间时，在身份锁内检查 app/lifecycle/epoch 后单调合并，禁止把旧快照的时间写入重建后的应用。

这些变更仍需 app_manager、rcoder-proxy、rcoder 组件编译及相关回归；不要把本节的实现说明视为运行验证结果。

## Project 注册回执（实施补充，基线冻结前）

新增 `project_write_receipts(request_id, fingerprint, outcome, recorded_at_us)`，主键为内部注册请求身份。RegisterProject 将容器、项目及 session 放在同一个队列命令和事务中；同事务保存最终 committed/superseded 结果。客户端看不到 COMMIT 回包时，以原 request_id 和快照重试，不根据当前 revision 猜测上次提交结果。

摘要覆盖完整注册输入；不存储额外明文凭据/模型配置。相同身份但输入不同明确拒绝。回执保留到实体删除之后，返回原提交结果不代表实体当前仍存在，查询当前状态仍读取当前实体。暂不做按时间删除，避免删除回执后旧队列复活；后续如需回收，必须证明队列/写入者已越过不可重放的持久边界。

该表属于 Project 域，初始化文件仍是四个；Project 域从六张增为七张，不能只更新 ORM 模型遗漏正式初始化 DDL。
