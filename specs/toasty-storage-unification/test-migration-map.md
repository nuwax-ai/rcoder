# 旧 Turso 故障测试迁移表

2026-09-19。本表记录删除旧 direct-Turso 后端之前保留的行为覆盖。新测试运行在生产 Toasty 模块；不把新实现中的 helper 断言替代真实数据库事务。生产项目 `db/tests.rs` 与探针共用同一份测试源码，根 workspace 检查也会编译这些测试。

| 旧测试/行为 | 新测试位置与覆盖 |
|---|---|
| basic_lifecycle | common/local_tests.rs 同名测试，身份、受理及请求重放 |
| exclusive_directory | common/local_tests.rs 同名测试，第二实例被拒，显式 shutdown |
| legacy_sqlite_directory | common/local_tests.rs 同名测试，保留旧文件、不创建新库 |
| existing_new_db_with_legacy_sibling | common/local_tests.rs 同名测试，已确认的新格式库正常重开 |
| quarantine_failure_releases_worker_and_lock | damaged_recovery_rolls_back_prior_rows_and_releases_directory_lock；打开失败后重获生产目录锁，修复损坏数据后成功重开 |
| concurrent_shutdown_waits_for_same_completion | db/tests.rs shutdown_waiter_cancellation_keeps_lock_until_jobs_and_runtime_end；取消首个关闭 waiter、并发关闭共享最终结果、资源释放后才完成 |
| shutdown_repeats_share_result_and_panic_propagates | task_panic_is_unknown_and_every_shutdown_reports_failure；当前调用结果未知、关闭结果保持失败、停止新任务受理 |
| queue_full_fails_within_budget | full_queue_rejects_without_waiting_for_the_running_transaction；受控阻塞任务与排队任务填满容量后 200ms 内拒绝溢出，无依赖调度运气的 sleep 填队列 |
| checksum_mismatch | baseline_is_repeatable_and_rejects_checksum_future_and_catalog_drift；同时覆盖未来版本及实际约束/索引漂移 |
| failure_midway_rolls_back_entire_admission | failed_admission_rolls_back_every_row_and_same_request_can_retry；事务内完整执行真实受理（含私有输入）后注入错误，root/operation/slots/request/input 全部回滚，原请求可重试；另有跨 app operation_id PK 冲突反例 |
| control_snapshot_rejects_broken_operation_link | broken_slot_is_rejected_instead_of_reported_idle；在专用库中故意破坏 FK 后，控制面读取拒绝断链 |
| cancelled_caller_and_dangling_transaction_do_not_leak | cancelled_caller_does_not_cancel_admitted_transaction + abandoned_transaction_rolls_back_before_connection_reuse；已受理取消仍提交、提前返回错误的未提交事务回滚后连接可用 |
| restart_quarantines_only_interrupted_claims | common/local_tests.rs 同名测试；Running/WaitingRetry 转 RecoveryRequired 并 revision+1，Pending/终态保持不动 |
| invalid_interrupted_record_blocks_startup_without_partial_quarantine | damaged_recovery_rolls_back_prior_rows_and_releases_directory_lock；按 operation_id 顺序先处理正常行、后遇到损坏行，前者的隔离写也回滚 |
| execute_rows_affected + manual transaction sequence | toasty_turso_rows_affected_preserves_cas_and_conflict_semantics；INSERT、DO NOTHING、CAS 一/零行、DELETE 一/零行、显式 commit/rollback |
| worker_panic_shutdown_replays_error_without_hanging | worker_thread_panic_is_replayed_to_all_shutdown_waiters；独立 owner 线程收尾 panic，多次/并发 shutdown 有界返回失败 |

另外保留官方引擎旧字节禁止降级打开、外键 PRAGMA、模型映射、初始化失败释放资源、last-owner drop 排空等测试。

本轮实际执行：probe `nextest --all-features --no-fail-fast`，72 通过，3 个 PG 测试未启用；退出 0。没有运行修复前红例，不宣称红绿双证据。没有运行整个根 workspace、Project PG 契约或部署 E2E。

不再依赖 `lsof` 的平台专有输出判断释放：使用生产目录锁重新获取 + owner runtime/thread 退出后释放资源的测试共同验证。该组件结果不等同于断电持久性或三平台实机验收。
