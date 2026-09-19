# Toasty 统一存储规范

状态：规划草案，未开始业务实现。日期：2026-09-19。

## 1. 已确认决策

- 用户决定采用 Toasty，并将本地 Turso 从 `0.8.0-pre.11` 对齐到 Toasty 使用的正式版依赖系列。
- 范围是全部替换 RCoder 的 SQLx 使用，包括 UserApp、Preview、ProjectStore、活动、选主、迁移和测试支持；元数据并入 UserApp 当前身份，退役旧 metadata 表与导入路径。只完成 UserApp 不算整体完成。
- 用户确认上一正式版本使用内存，没有上线数据库后端。因此不开发线上 SQLx 历史库迁移、双写或滚动混用两种实现的兼容层。
- 在首次数据库版本发布前重设计表结构并合并历史 SQL，形成分业务域的 v1 初始化基线；正式发布后使用增量迁移，不再改写已发布的初始化文件。
- 用户明确个人两节点 K8s 测试环境的目标 PG 库全部属于 RCoder，允许清空重建并测试。本次授权适用于该测试库，实施时核对实际连接目标后执行，无需再次请求同一授权；主机、认证信息从未提交配置读取，不写进文档。
- 本地其他开发数据不自动删除；Turso 使用新目录，不让 0.7 引擎直接打开现有 0.8 生成的 DB/WAL 文件。
- 保留 `rcoder-pg -> kubernetes` 和 PG 初始化门控；不开放 Compose PG，不新增部署模式。

## 2. 目标

1. 所有数据库访问经 Toasty 及其官方驱动，根 workspace 的有效依赖图不再包含 SQLx。
2. UserApp 的 Turso/PG 共用业务事务流程、校验和结果映射，避免只统一库名却继续维护两份生命周期算法。
3. 存储实现内部使用 Toasty 模型；业务 trait、共享业务类型、HTTP/gRPC 不暴露 ORM、连接、事务或驱动类型。
4. 保持生命周期身份、dev/prod/Application 作用域、幂等、receipt、deadline、RecoveryRequired 写保护的语义。
5. 保持 ProjectStore 的内存镜像、PG 持久化与回源语义，保持 Preview 端口分配和身份约束。
6. 提供有界接收、显式事务完成、故障连接隔离和可等待的关闭。
7. 核心控制字段、幂等命名空间、资源代次、并发端口分配及活动时间有明确唯一权威；约束与事务配合保障一致性，具体表结构见 [库表设计](schema-design.md)。

## 3. 非目标

- 不将目前内存模式的所有 Project/Preview 数据改存 Turso；统一访问技术不等于扩大各部署模式的持久化范围。
- 不启用 Turso Cloud、同步复制或 MVCC concurrent_writes。
- 不重写生命周期状态机，不改变外部资源操作的所有权规则。
- 不引入通用 Repository 框架，不把全部数据强行拆成关系字段，不为纯粹采用 ORM 改变业务协议。
- 本轮规划不执行发布、集群改配置或数据库清理。

## 4. 完成标准

- UserApp 的两后端运行同一套契约测试；PG 并发用独立实例/连接验证。
- Preview、PG write-behind、durable 写、回源、选主、flush/关闭全部迁移并验证。
- 所有新数据库初始化和迁移检查都在启动业务生产者前完成。
- 新表结构的 S01–S12 反例通过；空库初始化、多副本并发初始化、checksum/未来版本拒绝、缺失约束检测与重启不丢数据均有实测证据。
- 个人测试 PG 的重建按 [基线与重置方案](schema-baseline-and-test-reset.md) 执行，旧运行资源与新生命周期的处置有记录；测试期间不得反复清库绕过缺陷。
- 默认 feature、PG/Kubernetes feature、全 feature 均通过相应检查；Compose 与 remote K8s 覆盖受影响业务。
- 源码、依赖清单、测试工具不再主动使用 SQLx，依赖图核查无 `sqlx`/`sqlx-*`。历史文档可保留原名，不机械改写历史。
- 版本与发布源码证据、实际命令、失败与未运行项目分别记录。

配套：[技术计划](plan.md)、[库表设计](schema-design.md)、[SQL 基线与测试库重置](schema-baseline-and-test-reset.md)、[任务清单](tasks.md)、[可行性与验证记录](verification.md)。
