# T0 / S01–S12 证据映射

2026-09-20，基于 `196d1776` 及当前工作树只读复查。测试名称指向实际函数；历史结果保持原源码基线。
本轮不重新执行编译/部署。仅执行 `cargo metadata --no-deps --format-version 1`（退出 0，输出 `/tmp/rcoder-toasty-audit-metadata.json`），它不等于完整依赖 tree 或编译通过。

## 证据索引

- **E1**：[综合验证记录](verification.md)“第二轮 T0”：独立探针 9/9、PG owner/schema 实测；发布 Toasty 0.10.0 源码 SHA 在首节。
- **E2**：[真实 Preview 探针日志](evidence/2026-09-19-common-preview/preview-pg.log)：末行明确完整契约、同 key 并发首创和跨 key 争端口通过。
- **E3**：[综合验证记录](verification.md)“真正提交后确认丢失”：独立 PG 26/26，原 operation/request/input/slot 完整；原报告 `/tmp/rcoder-commit60d8ea505dc64ab5/pg-contract/assertions.json`。
- **E4**：[综合验证记录](verification.md)“S10/S11 写点矩阵”：PG/Turso 各 9 分支通过、存储 159/159、11 环境项不计通过；新增 PG 三项另有真实 fixture 结果。
- **E5**：[综合验证记录](verification.md)“S12 查询计划”：PG 30 万终态/200 活跃/300 租约的实际 EXPLAIN 与前后集合比较；Turso 1 万历史索引测试。
- **E6**：[CR06 真实进程关机](cr06-sigterm-deployment-2026-09-20.md)：Docker/Turso 15/15。
- **E7**：[依赖版本记录](evidence/2026-09-19-owner-tests/dependency-summary.json)：Toasty 0.10.0、Turso 0.7.2。

源码前缀：下表 `storage/` = `crates/rcoder-storage/src/`；`coordinator/` = `crates/preview-coordinator/src/`。

## S01–S12

| ID | 实际反例与实现入口 | 已有证据 / 尚缺 |
|---|---|---|
| S01 | `storage/db/tests.rs::normalized_schema_roundtrip_and_identity_constraints`；`userapp_lifecycle/tests.rs::{private_execution_input_contract,operation_lease_contract,physical_binding_is_atomic_and_cannot_cross_lifecycles}`；`common/local_tests.rs::broken_slot_is_rejected_instead_of_reported_idle` | E3/E4 覆盖业务身份与坏槽位。**未找到两个后端分别直接写入“有效但属于其他 app/lifecycle 的 operation”作为 input/lease/slot FK 的完整三类反例**；现直接 SQL 仅 missing operation 等。补共享非法行矩阵，失败后查询无残留；不要把不存在引用当跨身份引用。 |
| S02 | `userapp_lifecycle/tests.rs::{cross_scope_admission_is_independent,dev_uncertainty_does_not_block_prod,scoped_terminals_clear_own_slots_and_blocker_is_structured}`；`common/concurrency_tests.rs::independent_pg_owners_enforce_scope_cas_and_bound_lock_waits` | PG 主契约明确调用前两 shared helper；Turso各自入口。E3/E4，通过。 |
| S03 | `userapp_lifecycle/tests.rs::{request_identity_spans_recreation_and_control,control_command_is_durable_and_part_of_deduplication,joined_request_identity_remains_idempotent_after_completion}` | 前两 helper 在 PG 主契约执行；PG 主契约还直接验证两个合流 alias 完成后返回原 ID。Turso对应入口，E3/E4，通过。 |
| S04 | `userapp_lifecycle/tests.rs::{paginated_scan_and_identity_contract,operation_lease_contract,physical_binding_is_atomic_and_cannot_cross_lifecycles}`；`common/activity_tests.rs::contract`；`common/transaction_fault_tests.rs::contract` | 原历史、旧身份、活动和槽位有 E3/E4；**recreate 后旧 executor/touch/forget/retry 的完整交叉顺序未统一覆盖**，尤其旧 lease forget 与新代次租约同时存在的明确反例需补/确认。 |
| S05 | `tools/toasty-probe/src/owner_pg.rs::preview_contract` 473–514：两个独立 PG store，两个 tokio::join；源码经 `#[path]` 引用生产 backend | E2 已真实通过，不是缺实现。现迁入 `storage/preview_lifecycle/pg_tests.rs::independent_pg_preview_owners_serialize_creation_and_port_allocation` 并加入 PG catalog；**新正式入口尚未运行**，历史证据不改写。 |
| S06 | `coordinator/contract_suite.rs::{unknown_requires_evidence,stop_flow_and_reaccept,late_writer_cannot_overwrite_new_instance,generation_bound_recovery_and_stop}` | PG `pg_preview_store_satisfies_contract` 调用完整 run；内存 Preview也共用（Compose本来不持久化Preview）。E2/E3，通过；不要求不存在的Turso Preview后端。 |
| S07 | `storage/userapp_lifecycle/common/activity_tests.rs::{turso_activity_is_monotonic_and_lifecycle_bound,pg_activity_is_monotonic_and_lifecycle_bound}`；`userapp_lifecycle/tests.rs::runtime_policy_commits_only_with_success` | 乱序时间、错误lifecycle/epoch、旧delete及控制策略独立保护，E3/E4，通过。 |
| S08 | `storage/pg/project_store/lifecycle_tests.rs::{lifecycle_contract_old_remove_preserves_replacement_and_no_resurrection,lifecycle_contract_delayed_clear_and_remove_preserve_reused_session,lifecycle_contract_reload_and_cross_replica_sync_preserve_identity,lifecycle_contract_container_delete_preserves_changed_association}` | 既有真实PG ProjectStore/适配器62/62记录；PG-only域，不虚构Turso ProjectStore。空UID占位路径需与下行具体夹具继续核对。 |
| S09 | `storage/userapp_lifecycle/tests.rs::metadata_cas_preserves_unmentioned_fields_and_noop_revision`；`pg/project_store/lifecycle_tests.rs::lifecycle_contract_registration_receipt_replays_without_reapplying`；`common/concurrency_tests.rs` | Turso metadata专用例已在E4；**PG主契约没有直接调用metadata_cas_preserves_unmentioned_fields_and_noop_revision**，不能仅靠共用代码认定PG该反例运行。可提为shared helper接入PG主契约。 |
| S10 | `storage/userapp_lifecycle/common/transaction_fault_tests.rs::contract` 五受理写点；`db/tests.rs::baseline_is_repeatable_and_rejects_checksum_future_and_catalog_drift`；`db/pg_schema_tests.rs::pg_baseline_rejects_tampering_and_rolls_back_partial_ddl` | E4、PG schema 1/1、E1 PG删除Preview唯一索引拒绝通过；每个注入错误有命中断言。 |
| S11 | 同 `transaction_fault_tests.rs::contract` 四recreate写点，真实DeleteApplication完整证据后再重建，保留旧snapshot/历史/slots并同请求重试 | E4，两后端各9分支中的4分支；通过。 |
| S12 | `storage/db/tests.rs::unfinished_recovery_scan_uses_partial_index_and_catalog_rejects_wrong_predicate`；`tools/test_storage_recovery_scan.py` | E5，通过。真实PG规模不冒充Turso规模或部署压力测试。 |

## T0 逐项判定

| T0 项 | 证据及具体差额 |
|---|---|
| 正式依赖探针 | E1/E7与现Cargo.lock一致；独立工具仍在`tools/toasty-probe/`。尚缺归档完整Cargo tree及MSRV对照，不能仅靠本轮no-deps metadata勾全部。 |
| 默认/PG/双后端/工具链 | 既有默认2320/2320、全features2484/2484；最新storage159/159及PG-only Clippy。`db/pg_tls_tests.rs::postgres_verify_full_uses_tls_and_rejects_wrong_ca_and_hostname`存在，但本轮未定位该测试独立PASS日志；TLS实测结果需补定位而非猜测。MSRV没有最小版本编译证据。 |
| 类型映射 | `normalized_schema_roundtrip_and_identity_constraints`有NULL配置、复合关系与CHECK；codec使用checked u64→i64和微秒时间转换。**没有找到i64边界/u64超范围、非法时间戳、坏JSON跨两后端的完整反例**。优先补纯codec边界+真实两后端合法极值/非法持久行读取，不能只补CREATE测试。 |
| CAS | E1 PG八并发、E3独立store竞争，E4 Turso条件写；已完成。 |
| 完整事务/取消 | E4五写点、真正admit事务取消，两后端原请求完整回放；已完成。 |
| BEGIN/COMMIT/ROLLBACK | `db/driver.rs::failed_begin_commit_rollback_and_savepoint_are_quarantined`逐阶段断言is_valid=false且后续exec拒绝；`db/tests.rs::abandoned_transaction_rolls_back_before_connection_reuse`真实Turso；E3真实PG提交ACK丢失。可勾**故障注入/隔离合同**，不宣称每阶段都做过真实网络故障。 |
| ManagedDriver关闭 | `shutdown_waiter_cancellation_keeps_lock_until_jobs_and_runtime_end`、last_owner_drop、task/worker panic；E4及E6；已完成。 |
| 并发迁移/漂移/半DDL | E1两owner初始化单账本，E4 PG event trigger整段rollback，checksum/future拒绝；已完成。 |
| PG session lock | PG catalog明确`dedicated_leader_session_closes_after_shutdown_cancel_disconnect_and_timeout`，E3；已完成。 |
| Turso PRAGMA/重启/旧库 | driver.rs逐连接回读WAL/FULL/FK；`policy_enforces_foreign_keys_on_fresh_connection`、`turso_instance_directory_is_exclusive_and_restart_preserves_data`、旧版本拒绝测试，E4/E6。有**进程重启证据而非断电证据**；可勾。 |
| T0矩阵/专用代码 | 本表完成显式映射；后端专用代码如下。完整T0仍被类型边界/MSRV与TLS证据差额阻断，不宣称整体可行性验收结束。 |

## 后端专用代码边界

- 共用：`db/{owner,models,schema}.rs`、`userapp_lifecycle/common/{ops,repo,codec,configuration,activity}.rs`；业务trait无ORM泄漏。
- PG：`db/postgres.rs`连接/TLS/session预算，`pg/project_store/`与`preview_lifecycle/postgres.rs`；PG advisory锁属于短事务/专用session。UserApp业务算法不单独复制。
- Turso：`userapp_lifecycle/common/local_format.rs`、`exclusive_directory.rs`、`db/driver.rs` PRAGMA及本地引擎策略；单进程目录排他，Compose不据此支持共享多副本。
- DDL四基线：UserApp分别PG/Turso，Project/Preview仅PG；不存在Turso ProjectStore/Preview部署需求，不应补虚假后端矩阵。

新正式Preview入口尚未执行；本表不把catalog注册视为通过。其他生产源码未改，完整Compose/K8s/发布状态保持未验收。

## S04 / S09 正式反例补齐（当前未运行）

- S09：原 Turso 测试提取 `metadata_cas_contract`，使用独立 app；已接入 `postgres_real_transactions_and_restart_contract`，原 Turso 入口保持全部断言。
- S04：新增共享 `old_lease_cleanup_and_retry_preserve_recreated_lifecycle`；Turso 入口为 `turso_old_lease_cleanup_and_retry_preserve_recreated_lifecycle`，PG 同样接入既有主契约。真实应用删除完整阶段后重建，在新 Running 操作持有新租约期间执行旧租约 forget/重放、伪混合身份 forget、旧执行者 advance/reserve 和旧请求重试；检查新 app/slot/op/lease及旧历史不被修改。合法的旧终态租约清理允许执行，不错误要求历史清理永远禁止。
- 精确 catalog 新增 Turso 入口，PG沿原fixture无需额外目标。仅测试重构/新增；rustfmt和diff检查通过。未编译/运行，不能据此新增通过勾选。

## 后续实测补充：2026-09-20 阶段提交前后

本节补充后续证据，前文“未运行”保留其审查时点。

- 独立 PG 正式合约复跑 **31/31**，退出 0。报告目录：`/var/folders/y6/g5lk3d750833hz_rn5h3y6nh0000gn/T/rcoder-storage-new-pg-recheck-rihq24jy/pg-contract`；执行日志 `/tmp/rcoder-storage-new-pg-recheck.log`。
- S01 两后端跨身份 input/lease/slot 非法行矩阵、S04 旧租约清理和重建后的重试、S09 PG metadata CAS、正式 Preview 两独立连接竞争均已接入并运行。Turso 聚焦四项 4/4，日志 `/tmp/rcoder-storage-extra-contracts.log`。
- PG 首轮 30/31 失败来自新增测试留下 Running 操作，污染后续分页断言；已在测试断言完成后将该操作正常收束并清理绑定，未降低断言或修改生产行为。
- 以上不替代仍未完成的 T0 类型边界、完整 Compose/K8s 与发布验收。
