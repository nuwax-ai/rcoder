# userApp 持久化回归映射

本表固定行为不变量和验收层级；用例源码存在不等于执行通过。每轮实际结果以严格启动器生成的 run ID 报告为准。

| ID | 行为不变量 | 固定 SQLite 用例（`userapp_lifecycle::tests::`） | 当前层级 |
|---|---|---|---|
| UA-S01 | 元数据与受理原子提交，失败不留下半条身份 | `metadata_changes_commit_with_admission_and_never_before_rejection`、`turso_failure_midway_rolls_back_entire_admission`、`rejected_admission_does_not_leave_new_identity` | 真实 Turso 组件 |
| UA-S02 | 同归属登记不抖动版本；CAS 不覆盖新值 | `same_owner_registration_is_noop_and_other_owner_is_rejected`、`metadata_cas_preserves_unmentioned_fields_and_noop_revision`、`old_progress_cannot_overwrite_new_checkpoint` | 真实 Turso 组件 |
| UA-S03 | 请求去重包括参数及生命周期，旧请求不能修改重建对象 | `request_replay_returns_original_and_rejects_changed_parameters`、`recreation_and_control_cannot_reuse_the_same_request_identity`、`deletion_and_recreation_fence_late_unqualified_requests` | 真实 Turso 组件 |
| UA-S04 | 相同创建合并，不同意图不合并，完成后仍可去重 | `concurrent_ensure_joins_but_different_intent_does_not`、`joined_request_identity_remains_idempotent_after_completion` | 真实 Turso 组件；不证明 HTTP 首开 |
| UA-S05 | 恢复保存命令和不确定状态；不能接管另一执行者 | `turso_control_command_persistence_contract`、`restart_keeps_operation_and_uncertainty_blocks_new_creators`、`another_executor_cannot_advance_a_running_operation` | 真实 Turso 组件；不证明远端执行恢复 |
| UA-S06 | 数据库打开失败不降级，目录独占与链接别名安全 | `turso_instance_directory_is_exclusive_and_restart_preserves_data`、`（后端结构性保护移至 turso::tests；文件别名/边车防线由 exclusive_directory 共享实现持续覆盖）`、`（同上：边车/锁前置失败保护在共享 exclusive_directory 实现与离线观察器契约中覆盖）`、`（同上：初始化失败释放锁由 turso_store_rejects_checksum_mismatch 的失败路径覆盖）` | 真实 Turso 文件系统组件（exclusive_directory 共享实现） |
| UA-S07 | 删除只在完整证据提交后成功，独立清理不结束生命周期 | `turso_deletion_success_requires_evidence`、`storage_deletion_does_not_end_the_application_lifecycle` | 真实 Turso 组件；不证明物理删除 |
| UA-S08 | 策略仅随成功提交，控制快照读取一致 | `turso_configuration_policy_contract`、`turso_policy_commit_contract`、`turso_control_snapshot_contract`、`turso_control_snapshot_rejects_broken_operation_link` | 真实 Turso 组件 |
| UA-S09 | 故障不返回假成功，取消不泄漏写事务 | `closed_database_returns_error_not_absence_or_admission`、`turso_cancelled_caller_and_dangling_transaction_do_not_leak` | 真实 Turso 组件 |
| UA-P01 | PG 实际事务、导入迁移、重连及生命周期条件写入 | `postgres_real_transactions_and_restart_contract` | 独占 PostgreSQL 17，严格显式运行 ignored 用例 |

`storage_contract_cases.py` 列出以上及目录别名、恢复分页等全部固定组件场景，不能从测试发现结果反推必测集合。

## 尚需独立运行证据

- Docker Compose：三份配置的 SQLite 挂载、重建 rcoder 后原生命周期/操作 HTTP 查询一致。
- Docker/HTTP：多入口并发首开、流式错误收尾、工作空间清空实例身份、A/B 制品和定向清理。
- 故障恢复：受理后、远端成功而检查点未提交、终态提交而锁未释放等进程强杀窗口。
- K8s：实际多副本协调、PVC 409/403、UID/resourceVersion、旧资源替换和恢复。

不得用 UA-S/UA-P 全绿替代以上场景，也不得把数据库重新打开称为容器重建或进程强杀验收。

## 已接通的运行入口，仍待实际执行

- `turso_compose_recreation_contract`：固定 3 × 7 必经断言（配置、HTTP 首开收敛、唯一 builder 身份、HTTP 数据落盘、重建身份、错误启动拒绝、定向清理）；要求预先提供冻结构建二进制哈希。这里只是重建，**不是 SIGKILL 恢复**。
- `userapp_concurrency_component_contract`：`concurrency_contract.py` 固定的 18 项组件；单次执行数必须等于 1，不能依赖 suite exit 0。覆盖创建期限/取消/晚订阅/执行容量/代次与清空 nonce。

## 明确缺口（不能以既有组件替代）

1. 单副本真实 HTTP 首开扇入已接通 `first_open_contract.py`，跨 rcoder 副本仍未接通；SQLite 合并受理测试仅证明事务层。单副本场景先确认持久化受理，再同步发出四个消费入口，防止把尚未受理前的合法查询不存在误判为失败。
2. **登记后尚未请求 runtime** 时 SIGKILL：必须先确认 Pending 操作已持久化，在远端副作用之前杀进程，重启后证明安全继续。
3. **远端已成功但检查点尚未提交** 时 SIGKILL：必须可控制该准确窗口，重启读取物理身份后安全恢复或明确 RecoveryRequired，不能再次创建替代资源。
4. **终态已提交而锁尚未释放** 时 SIGKILL：必须观察数据库终态与残留锁同时存在，证明后续请求不会无条件清锁或永久假成功。

当前入口仅终止挂起的测试进程所使用的 SIGKILL 不属于以上三项；普通 Compose `--force-recreate` 也不属于它们。缺少可控进程边界时应补基础设施故障注入协议，不能用随机 sleep 取代窗口证据。

## 崩溃窗口入口（源码已接通，仍待执行）

- `docker_runtime_crash_recovery_contract`：`before_create` 在原样转发 Docker create 之前阻塞；`after_start` 在真实 Docker start 成功响应到达后阻塞。均 SIGKILL 指定 rcoder 容器、确认退出码 137，随后重启，要求原操作转为 RecoveryRequired、物理资源不替换、恢复扫描不重发 create。所有写仅允许本 app/owner/builder 族和已登记物理 ID＋lifecycle；代理在隔离 named-volume 内提供 Unix socket。
- `native_terminal_release_crash_contract`：真实 SQLx SQLite 和 OS 文件锁子进程，提交 DeleteCompute 终态后在真实 marker release 入口阻塞，父进程确认 SQL 与文件状态后 SIGKILL。它仅证明**无恢复凭据的旧 marker**不被自动接管以及终态不重复入队，不能代替新终态 release-receipt 协议的恢复成功证据。
- 以上全部均保留命令/二进制身份及现场数据；真实 Docker 前提是镜像含 python3，前置不成立必须报失败。组件/native/Docker 三层不能互相替代。
