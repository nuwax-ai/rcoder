# userApp 持久化回归映射

本表固定行为不变量和验收层级；用例源码存在不等于执行通过。每轮实际结果以严格启动器生成的 run ID 报告为准。

| ID | 行为不变量 | 固定组件用例（名称及模块见下方目录） | 当前层级 |
|---|---|---|---|
| UA-S01 | 元数据与受理原子提交，失败不留下半条身份 | `metadata_changes_commit_with_admission_and_never_before_rejection`、`failed_admission_rolls_back_every_row_and_same_request_can_retry`、`rejected_admission_does_not_leave_new_identity` | 真实 Turso 组件 |
| UA-S02 | 同归属登记不抖动版本；CAS 不覆盖新值 | `repeated_registration_is_an_idempotent_noop`、`metadata_cas_preserves_unmentioned_fields_and_noop_revision`、`old_progress_cannot_overwrite_new_checkpoint` | 真实 Turso 组件 |
| UA-S03 | 请求去重包括参数及生命周期，旧请求不能修改重建对象 | `request_replay_returns_original_and_rejects_changed_parameters`、`recreation_and_control_cannot_reuse_the_same_request_identity`、`deletion_and_recreation_fence_late_unqualified_requests` | 真实 Turso 组件 |
| UA-S04 | 相同创建合并，不同意图不合并，完成后仍可去重 | `concurrent_ensure_joins_but_different_intent_does_not`、`joined_request_identity_remains_idempotent_after_completion` | 真实 Turso 组件；不证明 HTTP 首开 |
| UA-S05 | 恢复保存命令和不确定状态；不能接管另一执行者 | `turso_control_command_persistence_contract`、`restart_keeps_operation_and_uncertainty_blocks_new_creators`、`another_executor_cannot_advance_a_running_operation` | 真实 Turso 组件；不证明远端执行恢复 |
| UA-S06 | 数据库打开失败不降级，目录独占与链接别名安全 | `turso_instance_directory_is_exclusive_and_restart_preserves_data`、`turso_store_exclusive_directory_rejects_second_instance`、`failed_initialization_joins_before_returning_error`；目录别名由共享 exclusive_directory 契约覆盖 | 真实 Turso 文件系统组件 |
| UA-S07 | 删除只在完整证据提交后成功，独立清理不结束生命周期 | `turso_deletion_success_requires_evidence`、`storage_deletion_does_not_end_the_application_lifecycle` | 真实 Turso 组件；不证明物理删除 |
| UA-S08 | 策略仅随成功提交，控制快照读取一致 | `turso_configuration_policy_contract`、`turso_policy_commit_contract`、`turso_control_snapshot_contract`、`broken_slot_is_rejected_instead_of_reported_idle` | 真实 Turso 组件 |
| UA-S09 | 故障不返回假成功，取消不泄漏写事务 | `closed_database_returns_error_not_absence_or_admission`、`cancelled_caller_does_not_cancel_admitted_transaction` | 真实 Turso 组件 |
| UA-P01 | PG 实际事务、导入迁移、重连及生命周期条件写入 | `postgres_real_transactions_and_restart_contract` | 独占 PostgreSQL 17，严格显式运行 ignored 用例 |

[`storage_contract_cases.py`](storage_contract_cases.py) 固定组件用例、完整模块路径和额外 PostgreSQL/Turso 保护场景。必测集合不从测试发现结果反推；表内名称随实现迁移维护，不能通过空筛选制造通过。

## 当前验收入口与层级

以下是源码中的测试入口，不是本轮执行结果。按 [严格 E2E 启动说明](README.md) 选择套件与场景，并检查整轮报告、固定断言及源码/二进制身份。

| 入口 | 观察范围与限制 |
|---|---|
| `turso_storage_lifecycle_contract`、`pg_storage_lifecycle_contract` | 真实数据库组件、事务和生命周期条件写；不能证明容器和卷的行为 |
| `turso_compose_recreation_contract` | 三种 Compose 配置的挂载、HTTP 首开、生命周期和操作落盘、重建身份与定向清理；重建不是 SIGKILL |
| `userapp_concurrency_component_contract` | 创建期限、取消、晚订阅、执行容量、代次与清空 nonce；不替代多副本 HTTP |
| `first_open_contract.py` | 单副本真实 HTTP 首开扇入，先确认持久受理，再同步请求消费入口；不证明跨副本协调 |
| `docker_runtime_crash_recovery_contract` | 在 Docker create 转发前或 start 成功响应后精确阻塞，强杀指定 RCoder，再核对原操作和物理身份；不能以普通重建替代 |
| `native_terminal_release_crash_contract` | 真实 Turso/文件锁子进程在终态提交后的 release barrier 被 SIGKILL，退出后离线核验数据库和残留 marker；不证明完整 Docker/K8s 恢复 |

崩溃窗口工具见 [`docker_crash_contract.py`](docker_crash_contract.py) 和 [`native_crash_contract.py`](native_crash_contract.py)。真实 Docker 场景需要配套镜像和工具；前置缺失应报失败，不能降级为组件测试。无原始 release 回执的旧 marker 保护不等于新协议的恢复成功证据。

## 验收边界

- 跨 RCoder 副本首开、K8s PVC 的 409/403、UID/resourceVersion 与资源替换，应使用对应真实部署场景核验。
- 强杀验收必须观察准确的受理、远端副作用或终态释放窗口，并确认退出；随机 sleep、取消挂起测试进程和 `--force-recreate` 不能替代这些证据。
- 组件、原生进程、Docker、K8s 与业务 HTTP 是不同证据层级。每轮是否通过，以该轮完整报告为准；历史通过数量和源码中的入口不能代替实际运行。
