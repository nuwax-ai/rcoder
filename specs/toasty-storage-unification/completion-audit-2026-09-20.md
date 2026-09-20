# Toasty 完成度审计：组件证据与剩余验收

日期：2026-09-20；读取时 HEAD `196d1776`，另有并行未提交改动。
本轮仅更新 tasks.md 的证据状态，不修改实现、不运行编译或部署、不改写历史验证基线。
本文件引用 verification.md 已记录的实际命令和结果；当前源码存在不单独作为通过依据。

## 本轮更新的 12 项

| Tasks 项目 | 实现/验证依据 | 完成边界 |
|---|---|---|
| T0 CAS | [共用 UserApp CAS 实测记录](verification.md)；真实 PG 契约最终 26/26 含独立 owner 竞争；`common/concurrency_tests.rs`、`db/tests.rs::toasty_turso_rows_affected_preserves_cas_and_conflict_semantics` | 组件及真实独立 PG；不是 K8s 双副本部署 |
| T0 完整事务/取消 | [完整受理取消补验](verification.md)、[S10/S11](verification.md)；PG/Turso 各取消 1/1、写点 1/1（9 分支） | 真正四表受理进入事务后取消调用方，原身份可重放；不是只测 Probe |
| T0 ManagedDriver 关闭 | `db/tests.rs` 的阻塞工作/并发关闭/取消关闭等待者/最后 owner 释放、panic 反例；[owner 测试记录](verification.md)及最终存储 159/159 | 测试覆盖 runtime/连接退出前持锁；真实进程关机另见 CR06 |
| T0 PG 迁移 | [首次独立 PG schema 探针](verification.md)记录两个 owner 并发初始化、一条账本；本轮 `db/pg_schema_tests.rs` 服务端 DDL 故障、checksum/未来版本 1/1 | 两次证据分别保留原基线；不称测试集群已重建 |
| T0 PG session lock | 最终真实 PG 契约 26/26 包含 `dedicated_leader_session_closes_after_shutdown_cancel_disconnect_and_timeout`；精确目录在 `tests-e2e/tools/storage_contract_cases.py` | 专用 session 不归还业务池；涵盖取消、超时、断连及关闭 |
| T1 基础设施与 trait 隔离 | `db/{owner,driver,models,schema}.rs`；UserApp trait 使用 shared_types，ORM 封装在 common；[最终存储门禁](verification.md) | 默认/PG/all-features 既有集中门禁及最新 storage 门禁；不替代后续并行改动检查 |
| T1 共用 UserApp 事务 | `userapp_lifecycle/mod.rs` 中 PG/Turso 均导出同一 ToastyUserAppStore；`common/mod.rs` 实现完整 trait，scope 竞争契约与两后端本轮写点矩阵通过 | 实现统一及业务契约通过；不由此勾选全部 S01–S12 |
| T1 Preview | [共用 Preview 实现及验证](verification.md)；真实 PG 最终契约保留 `pg_preview_store_satisfies_contract` | 首创、端口互斥及旧身份拒绝属于契约；网络路由部署另验 |
| T1 ProjectStore | verification 中 ProjectStore 实际 PG 62/62、最终 PG 26 项必需断言，覆盖 repo/load/writer/sync/leader、代次、回源、flush；对应 `pg/project_store/` | 真实 PG 组件，不称远端整链通过 |
| T1 Activity | [生命周期活动记录](verification.md)及最终 PG `pg_activity_is_monotonic_and_lifecycle_bound`、Turso共享契约 | 实现以 lifecycle+单调时间写入；不恢复第二份 stopped/wake 权威 |
| T1 四份 SQL | 当前 `crates/rcoder-storage/schema/` 仅 userapp-turso/userapp-pg/project-pg/preview-pg 四份 v1 SQL；schema 初始化探针、目录漂移拒绝和最终编译门禁通过 | 旧 SQL 消费已切换；配套部署引用全量检查仍单独待办 |
| T5 聚焦组件 | 既有消费方 default/all-feature 集中回归；本轮 storage 159/159、11 环境项未在普通套件执行，新增 PG 三项已各自真实执行；all-features/PG-only Clippy exit 0 | 日志 `/tmp/rcoder-storage-audit-final-{nextest,clippy,pg-clippy,catalog}.log`；不覆盖后续其他模块新改动 |

## 保留未勾选及具体差额

- **T0 版本探针的完整产物清单**：发布源码 SHA、正式版本和独立探针已有记录；本轮未取得同一证据包内完整 Cargo tree/解析清单，保留原整项未勾，不能仅因版本正确替代所有交付物。
- **T0 工具链矩阵**：默认与全 features 已通过，远端构建也有记录；尚缺把 MSRV、TLS、驱动 features 与全部目标工具链逐项对齐的最终表。
- **T0 数据类型/约束矩阵**：模型、复合身份、唯一约束、JSON、NULL 有已有反例；没有找到 PG/Turso 每个时间/整数边界逐项完整覆盖表，保留整项。
- **T0 BEGIN/COMMIT/ROLLBACK**：提交 ACK 丢失是真实 PG 中继故障，不能扩写成三阶段所有故障都已验证；保留阶段/隔离对应表待核对。
- **T0 PRAGMA/跨进程恢复**：有回读、旧库保护和真实 CR06 重启证据；仍需统一列明 WAL/FULL/FK、提交后进程重启各项，不把正常 reopen 写成断电持久性。
- **T0 总矩阵及后端专用代码清单**：本审计不是完整 T0 可行性矩阵，保持待办。
- **T1 十二表全部约束**：schema 与模型已实现，但“所有完整身份 FK/请求命名空间”的穷举非法写入矩阵未在本轮核齐，整项保持未勾。
- **S01–S12 总矩阵**：S10/S11 写点已补，S12 PG 30 万历史规模实际计划及 Turso 1 万历史测试有证据；不能凭三项补验或测试总数直接勾整个矩阵。S01–S09 需要逐条映射已存在测试，缺口再补，避免无依据重跑全量。
- **旧接口/配套仓库消费者全量核查**：SQLx 清除已有独立勾选；publish_tasks/metadata 无消费者与配置工厂所有引用、build-agent-docker 最终变更仍需逐项收口，不能以 SQL 文件数代替。
- **T5 部署与发布**：完整 Compose 仍有进行中/失败待补场景；个人 PG 重建、当前快照 K8s UserApp/Chat/Preview/Gateway 及多副本场景尚未完整验收。空库并发初始化已有组件证据，但整项还包含最终持久重启验收，保持待办。

## CR06 证据单列

[CR06 SIGTERM 部署记录](cr06-sigterm-deployment-2026-09-20.md)给出真实 Docker/Turso 15/15：
关闭后 keep-alive 不再受理、等待在途操作、RecoveryRequired 落盘、目录锁释放及重启不重放未知命令。
它补强关机证据，但不能替代完整 Compose、PG/K8s 或 npm 原生组件关机验收。

所有原已勾选的 `e3959112` 检查继续保留原基线限定；本审计没有将历史通过改为当前全仓通过。

## 后续精确映射修订

参见 [T0 / S01–S12 证据映射](acceptance-matrix-2026-09-20.md)：BEGIN/COMMIT/ROLLBACK注入及Turso PRAGMA/进程重启已找到完整对应证据，Tasks追加勾选。Preview独立PG并发探针历史已经通过；现将它纳入正式PG目录，新入口等待验证，不是历史未实现。其余粗粒度差额以新矩阵逐项说明为准。
