# Toasty 与首次数据库 schema 改造实施提示词

在 RCoder 仓库实施 Toasty 全量替换 SQLx，并完成首次正式发布前的库表设计整理。先读 AGENTS.md、检查工作树及相关 diff，保留其他未提交改动。

## 必读

按顺序阅读本目录的：

1. [spec.md](spec.md)：已确认范围与非目标。
2. [plan.md](plan.md)：Toasty 正式版、事务、连接生命周期与分期。
3. [schema-design.md](schema-design.md)：最终表结构、事务顺序、S01–S12 反例。
4. [schema-baseline-and-test-reset.md](schema-baseline-and-test-reset.md)：15 个 SQL 收敛为 4 个初始化文件、统一版本账本及个人测试 PG 重建。
5. [tasks.md](tasks.md) 与 [verification.md](verification.md)：完成标准及实际验证状态。

## 已定范围

- 全部移除 SQLx：UserApp PG/Turso、Preview、ProjectStore、活动、选主、迁移、测试工厂都在范围内。
- 以 Toasty 0.10.0 发布源码为准，Turso 使用其正式依赖系列，初始目标 0.7.2；不用本地绝对路径依赖或浮动 main。
- 先完成 T0 真实双后端探针，重点是事务/CAS、FK/约束、取消收束、故障连接隔离、可等待关闭、PG 会话锁与迁移锁。若公开接口不能满足，报告具体证据并修订方案，不能伪装通过后批量迁移。
- 业务 trait 不暴露 ORM；UserApp 两后端共用业务事务算法。ORM 模型与业务类型集中转换。
- 采用新表设计，不保留整条 record JSON 作为第二权威；落实统一请求命名空间、dev/prod/Application 槽位、完整身份 FK、Preview 活跃端口唯一与分配锁、活动时间代次隔离、Container/Project/Session 代次与延迟写 CAS。
- 删除退役 publish_tasks、旧 metadata 表与导入路径；保留仍有业务消费者的 generic Computer user_id。
- 保持 PG 受 Kubernetes feature 门控；不开放 Compose PG，不扩大 Project/Preview/Activity 的持久化装配范围。
- 数据库尚未正式发布，无需 SQLx 旧开发表到新 schema 的生产升级链。历史 SQL 按最终设计整理成四个 0001，后续正式发布后的变更才追加新迁移。
- Turso 0.8 开发文件不得直接用 0.7 引擎打开；使用新测试目录。未知结果继续保留 RecoveryRequired/租约/物理身份保护，不以清锁绕过。

## 开发与验证

按 T0→T1→T2→T3→T4→T5 执行，实际结果同步 Tasks 与 verification。优先用 cargo nextest run --no-fail-fast，聚焦后验证默认、PG 和全 features；涉及独立 app-cli 时额外验证。Cargo 任务串行，不共用 target 并发执行。

修复缺陷先补能暴露旧行为的反例；实施并运行 S01–S12、初始化/关闭/取消/并发等契约。保留有效业务断言；仅针对已明确退役的旧开发迁移链调整测试，不把失败预期迎合错误代码。完成 Compose 完整业务回归与个人远端 K8s 验证。

用户已授权个人两节点 K8s 的 RCoder 专用 PG 库清空重建并测试，不用重复索要授权。主机、认证、namespace 和连接目标从未提交配置/Secret 读取，不写入源码或报告。先完成可部署版本与组件门禁，再按基线重置文档核对目标、停止所有旧写入者、处理/隔离旧运行资源归属、重建目标 database、部署新双副本。不能删除 agent PVC、共享存储或把 DB 消失当作旧 K8s 操作已结束。

remote-k8s 使用 verify 部署当前快照，之后对同一快照追加 test。明确验收使用哪个部署及 PG 库，避免清理一套环境、验证另一套。新 schema 初始化之后保留数据验证重启与恢复，不能每轮清库使回归通过。

## 交付

报告最终表/约束/索引清单、业务 trait 与调用方变更、SQLx 移除证据、实际依赖图、命令和退出码、每项测试结果、PG 重建及镜像身份、Compose/K8s 报告和剩余项。区分实现完成、测试通过、部署验收、正式发布；本任务不自动执行 npm/正式镜像发布或 push。
