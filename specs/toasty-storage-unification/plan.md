# Toasty 全量替换 SQLx 技术计划

状态：待实施；T0 技术门槛通过前不批量改写业务。需求以 [spec.md](spec.md) 为准。

## 1. 基线与依赖选择

- RCoder 审查基线 `a31b0e52`，工作树有并行开发改动，实施前重新记录 SHA/diff。
- 库表设计补充审查基线 `d9e8fd32`，SQL 整理复核时 HEAD 为 `2786632a`。原审查记录保留；最新设计见 [schema-design.md](schema-design.md)，不再要求维持旧表形状。
- 本地 Toasty HEAD `552e5b5e446f87ec9276b63bbf35bd4c3782cded` 不是发布提交。
- crates.io `toasty 0.10.0` 发布包 `.cargo_vcs_info.json` 指向 `f3411327b6b57fb03deac9e49f7021d1448176be`。以发布源码为实现依据。
- registry 已确认 `toasty-driver-turso 0.10.0` 依赖 `turso ^0.7`；`0.7.2` 是已发布、未撤回的非预发布版本。初始解析目标为 Toasty 0.10.0 + Turso 0.7.2，以根 Cargo.lock 记录实际版本。
- 不用本机绝对 path 依赖或浮动 Git main。不通过额外添加新版本 turso 造成两个引擎版本共存。生产代码没有直接驱动需求时删除直接 turso 依赖。
- 对照发布包核查 Toasty 所有子包版本、Rust 1.95 最低要求、Docker/CI 工具链、TLS、平台编译与依赖默认 features。Toasty 的 Turso 驱动开启引擎默认 features，不等价于当前 `default-features=false`，需记录二进制/构建影响。
- 不能把“非预发布版本”写成“已证明本项目生产可靠”。

## 2. 结构

```text
HTTP / coordinator / recovery / existing services
                  ↓ 现有业务契约
UserAppLifecycleStore / PreviewLifecycleStore / ProjectStore 等
                  ↓ rcoder-storage
共用业务事务 + 私有 Toasty 持久化模型
                  ↓
ManagedDatabase / ManagedDriver / migration runner
          ├── Toasty Turso：本地独占、单业务 worker
          └── Toasty PostgreSQL：连接池、数据库锁、多副本
```

建议增加内部 `db/` 模块：`open`、`managed_driver`、`transaction`、`migration`、`models`、`error`。最终拆分按规模调整，不新增公共通用数据库 trait。

保留 `shared_types` 中的现有契约和 `UserAppStoreControl` 权限隔离。UserApp 统一成一份内部事务实现；后端差异仅允许出现在连接初始化、事务开始/锁、特殊查询和 schema DDL 中。禁止两个大 impl 复制受理和恢复流程。

## 3. 业务事务与模型

### 3.1 UserApp

- 持久化实体留在 rcoder-storage，采用 [库表设计第 4 节](schema-design.md#4-userapp-表组9-张业务表) 的 9 张业务表。身份、状态、scope、revision、策略等控制字段独立建模；仅 command/checkpoint/执行输入/receipt 等载荷使用版本化 JSON。不再复制整条权威 record JSON；对外业务对象由列与载荷集中重建。
- 首次数据库版本直接采用新的初始化基线，不开发旧开发表的迁移链。复合唯一键、外键、CHECK、索引由审核后的 DDL 保证；不得假设 derive 宏会自动生成所有约束。
- requests 表统一 control/recreate 请求命名空间，active_operations 独立三槽位行；保留 dev/prod 可并行、Application 互斥、RecoveryRequired 保持占位的语义。历史操作引用稳定 app 根，只有当前 slots/activity 引用当前 lifecycle，避免 FK 阻止合法重建。
- 常规读写使用模型 API。UserApp 禁止以 `SELECT FOR UPDATE` 作为并发正确性前提；根控制版本 CAS、跨表一致性查询、条件写可经 Toasty raw SQL；其他模块的 advisory lock 按独立协议审查；集中在有限模块，不以 raw SQL 重建一套 SQLx 包装库。
- `ensure_identity` 与 `admit_with_input`：PG 原子插入占位/读已存在身份，再锁目标 app 行；Turso 在完整业务任务中 `BEGIN IMMEDIATE`。读身份、幂等映射、冲突判断、写操作/输入/active slot 必须同一事务。
- 继续调用纯 `domain` 校验/转换，不改 app/lifecycle/operation/executor/revision/scope 检查。成功返回必须晚于 commit。
- 条件写必须确认影响行数/返回记录数，不得“读出对象后无条件保存”。先验证 Toasty 模型 update 的返回语义，不满足时使用集中 raw SQL。
- 不为追求跨 app 并发重做现有 PG 锁粒度；本次仅保持既有业务语义与短事务范围。
- `list_control_snapshots` 使用单条 join 或明确一致快照事务，不能用多个普通 READ COMMITTED 查询拼一页。扫描结果仍需重新 CAS 才能执行恢复。

### 3.2 Preview

迁移 `preview_lifecycle/postgres.rs` 和带 `sqlx::FromRow` 的行类型。当前活跃端口仅有普通索引，首次创建没有现存行可锁，不能视为已具备并发分配保护。补活跃端口部分唯一索引、PG 事务级分配锁和身份条件写；锁覆盖同 key 首创及不同 key 抢端口，限制在短数据库事务内。保持物理实例身份、旧代次拒绝、unknown 占端口保护，详见库表设计第 7 节。

### 3.3 ProjectStore 与 activity，退役旧 metadata

迁移 `pg/project_store/{repo,writer,load,sync,leader}`、`pg/userapp/` 和相关工厂、测试支持。

- 保持 memory mirror + write-behind 架构，不把全部内存读取改成数据库查询。
- 保持批处理原子性、poison 操作隔离、重试预算、generation/tombstone 检查、按既定顺序取锁。
- durable API 保留现有成功/降级语义；回源与跨副本读取不能因为 ORM 模型缓存而读旧值。
- containers 增加登记 generation；projects/session/container 关联绑定确切代次，延迟写携带预期 generation/revision，不能用 created_at 或最后访问时间决定覆盖授权。session 去掉冗余 container_name，按精确关系回源。现有 generic Computer 的 user_id 保留。
- activity 仅保存带 lifecycle 的单调活动时间；控制意图来自生命周期策略，stopped/wake_blocked 不再通过批量快照写回成为第二权威。同步调整共享 trait、采集缓存、加载/flush/删除和代理路由身份传播，保持热路径无每请求同步查库。
- 删除无消费者的 publish_tasks 表及旧 userapp_metadata 导入路径；核对 trait/工厂/测试消费者后删除退役接口，不能只删 DDL。
- 时间统一 UTC 微秒 i64，deadline_ms 保留既有单位；JSON、nullable、整数宽度与溢出明确映射，不引入多个模块各自转换。

## 4. 连接生命周期：本次必须补齐的基础设施

发布版 Toasty 的 Db clone 共享池；Connection Drop 是归还池，非关闭物理连接。pool 没有公开可等待 close，ConnectionHandle 内部持有 JoinHandle。仅 drop Db 不能作为“后台连接已退出”的验收证据。

优先采用官方公开 Driver/Connection trait 的薄包装，不修改上游算法：

1. `ManagedDatabase` 统一接收门，Closing 后拒绝新工作，记录已受理的完整业务任务。
2. `ManagedDriver::connect` 包装真实连接，为每条连接注册生命周期计数/完成通知；完整转发 capability、migration 等接口，不隐式降级功能。
3. `ManagedConnection` 在底层连接实际 drop 后才发布退出通知；用 `Option` 显式 take/drop，不能在其内层字段析构之前错误通知。
4. connection_lost、rollback 失败、不能确认干净状态的连接标记不可复用，`is_valid=false`，利用 Toasty worker 的 invalid 分支退出。Turso 当前默认 is_valid=true，不能仅相信错误名称会自动淘汰。
5. 必要时在包装层记录活动事务状态；commit 错误作为结果未知，不自动新身份重试。业务层保留恢复保护。
6. 关闭顺序：停 HTTP 新请求/后台生产者 → 等待受理业务任务与 writer flush → 显式完成事务 → 丢弃内部所有 Db/Connection 引用 → 等待底层连接退出确认 → 本地 owner runtime/thread 退出 → 释放目录锁。
7. 关闭状态独立持有，首个 shutdown 调用者取消不得取消关闭过程；并发调用等待同一结果。超时必须返回失败并保留仍在使用的资源所有权，不能释放文件锁后继续写库。

目录锁必须由本地实际资源所有者持有，包含初始化失败/取消路径。Turso ORM/驱动对象不逃逸本地 owner。T0 实测包装能覆盖所有创建/关闭路径；如官方接口不足以做到，不得用 sleep 或探测一次端口替代，须报告具体缺口并修订设计。

## 5. 事务取消、错误与预算

- Turso 继续有界单 worker；一个任务覆盖整个业务事务。HTTP future 消失不取消已开始的事务，worker 收束后再处理下一项。
- PG 的已受理业务事务由受跟踪任务执行；请求取消与 DB 事务收束分离。调用方拿不到结果时按已有 request/operation 身份重查。
- 正常失败显式 await rollback。Toasty 的 Transaction Drop 是 fire-and-forget 回滚，只作为兜底；回滚失败隔离连接，返回原错误及清理上下文。
- 设置队列接收、pool wait/create、PG statement_timeout 和总 deadline，重试不重置预算。不能用 Tokio timeout 丢弃 future 就宣布数据库语句已停止。
- 错误通过 Toasty typed error 与业务上下文映射；未知错误不能等价 NotFound。不要按第三方错误字符串做授权或冲突归类。
- Toasty Turso 驱动存在 generic error 包含 conflict 的分类逻辑；不能据此无条件重试业务事务。本期关闭 MVCC，并用幂等、明确回滚与总预算约束允许的重试。
- 所有日志隐藏 DSN 密码、执行输入和令牌；检查第三方 Debug/query tracing 配置，不能直接打印 driver。

## 6. PostgreSQL 选主

现有 leader 使用 session advisory lock，并将 SQLx 连接 detach；直接替换为 Toasty pooled Connection 后 drop 会把持锁会话送回池，这是禁止的。

计划使用 leader 独占的 ManagedDatabase（max_pool_size=1），固定持有一条 Connection；不得共享给业务事务。退出主动释放 advisory lock；网络不确定时标记失去 leader 身份、关闭独占连接并等待释放路径完成。重新获主必须重新取锁，不能相信本地布尔值。

必须验证失联、探测超时、任务取消和进程退出；另一副本只能在 PG 实际释放锁后获主。业务写入仍保留既有数据库事务锁/代次约束，选主身份不能替代写入身份校验。

## 7. schema 初始化与引擎版本

- 没有线上旧库：采用审核后的新初始化 schema，不开发 SQLx migration 表到 Toasty 的历史转换。15 个历史 SQL 整理为 UserApp PG/Turso、Project PG、Preview PG 四个初始化文件，目录与流程见 [基线与测试库重置方案](schema-baseline-and-test-reset.md)。这是按最终设计重写 CREATE DDL，不是串接历史 ALTER。
- 使用一张 rcoder_schema_migrations，以 (component,version) 标识版本；checksum、已启用域及关键约束均验证。废弃四套旧 bookkeeping 和 SQLx migration 调用。
- 个人测试 K8s 的 RCoder 专用 PG 已获准清空重建。实施完成并通过组件门禁后，停止所有旧写入者，再重建核验后的目标 database，部署新版本并验收；不改成无条件启动自动清库。其他无新版本标识的开发库明确拒绝，不自动覆盖。
- 保留本项目有序版本、checksum、拒绝未来版本的机制，经 Toasty 执行。Toasty 自带 MigrationSet 按 ID 跳过，不能直接替代此契约；禁止生产启动自动 push_schema/reset_db。
- PG 使用数据库迁移锁。拿锁后再检查历史，在相同事务中执行迁移和版本记录；多副本冷启动单胜者。migration 元表首次创建同样受锁保护。
- Turso 先目录独占，再 BEGIN IMMEDIATE；DDL 与版本记录同事务。WAL、synchronous 和必要 PRAGMA 在每条新连接上初始化并回读验证，不能只设置第一条池连接。
- raw SQL 不假设支持多语句执行；初始化脚本按审核后的语句列表执行，禁止简单用分号切割 SQL。
- `0.8.0-pre.11 → 0.7.2` 是引擎降版本，不直接复用原 DB/WAL 文件。默认新环境验证；若之后需要保留开发数据，另做离线逻辑导出/导入方案，未经验证不实施。

## 8. 实施阶段

1. **T0：正式包双后端探针**。确认编译、模型映射、CAS/行数、复合约束、事务/取消、故障隔离、连接关闭、PG 选主、迁移并发与 Turso durability。未通过不批量迁移。
2. **T1：公共基础设施**。ManagedDatabase/Driver、迁移、错误/预算、私有持久化模型、测试契约。
3. **T2：UserApp 双后端合并**。一份业务事务 + 两种内部数据库策略，运行全部共同契约。
4. **T3：其余 PG 模块替换**。Preview、ProjectStore、activity、writer/sync/leader、装配及测试；删除旧 metadata/publish 表及无消费者路径。
5. **T4：删除旧实现与依赖**。移除 SQLx 代码/宏/迁移调用/依赖及旧 Turso 执行包装；相关工具和镜像配置同步。不能删除业务 trait、真实测试或数据来完成清理。
6. **T5：部署验收**。默认/PG/全 feature、本地 Compose 全套、个人测试 PG 一次性重建、remote K8s UserApp/Chat/Preview/多副本与重启恢复；按影响补 Gateway 路径。新基线初始化成功后继续保留数据验证重启/升级；正式发布另行执行。

全程不同时启动两个实现指向同一数据库；各批可独立提交，但整体完成必须到 T5，不能留下永久双实现。
