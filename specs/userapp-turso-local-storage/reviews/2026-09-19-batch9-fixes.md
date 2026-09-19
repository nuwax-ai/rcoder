# 2026-09-19 批 9 修复验证（Turso 阻断项 + app-cli 排队 + 关机顺序 + Compose 场景）

基线：`166c3579` → 本轮 `c46a2b94`（6 笔提交，未 push）。前置证据：
[2026-09-19-claude-verification.md](2026-09-19-claude-verification.md)（复核探针）、
[2026-09-19-batch8-followup.md](2026-09-19-batch8-followup.md)（排队槽源码路径）。

## 一、修复清单与提交

| 项 | 提交 | 行为变化 | 反例（修复前失败 → 修复后通过） |
|---|---|---|---|
| R01 worker/锁生命周期 | `7b24be95` | 目录锁移动进 worker 线程（连接 Drop 后才释放）；watch sender 消失/ready 接收方消失/runtime 构建失败全部可靠退出；expect 改错误传播；quarantine 失败路径显式关停并 join | `turso_quarantine_failure_releases_worker_and_lock`（lsof fd 证据）；修复前：忙循环 52 万次/300ms + 线程泄漏持 db/WAL fd |
| R03 并发 shutdown | `7b24be95` | WorkerHandle 共享 outcome+Notify；join 独立 spawn_blocking（不依赖任何调用方存活）；panic 传播为错误；重复调用共享结果 | `turso_concurrent_shutdown_waits_for_same_completion`（修复前 2.46µs 提前 Ok）；`turso_shutdown_repeats_share_result_and_panic_propagates`（修复前 panic 后 Ok） |
| R04 显式回滚/队列有界 | `7b24be95` | execute_task 错误路径立即发轻量语句触发挂起 ROLLBACK（不推迟到下一业务请求）；清理失败隔离连接（后续快速拒绝"connection isolated"）；run() 改 try_send+10s 有界容量等待（超时明确拒绝且从未入队） | `turso_queue_full_fails_within_budget`（修复前 500ms 无结果=无限等待） |
| R05 旧库目录保护 | `7b24be95` | 新库不存在而目录有旧 userapp.sqlite3 → fail-fast 拒绝（指引独立目录）；已有新库并存时正常打开；旧文件不动 | `turso_legacy_sqlite_directory_is_rejected`（修复前静默建二库）；`turso_existing_new_db_with_legacy_sibling_opens` |
| R02 关机顺序 | `7b24be95` | graceful_shutdown：①恢复扫描器停接单+30s 有界收束退出 → ②在途协调任务门闸（OperationFlightGate，builder 创建/控制工作器挂 guard，30s 预算）→ ③最后 control.shutdown()。预算耗尽记录未完成数量（重启隔离兜底） | `gate_reports_budget_overflow_and_idle`（门闸单测）；端到端 SIGTERM 反例待 Compose 轮（见未完成） |
| 批8§2 排队槽终局 | `7b24be95` | active Failed/Cancelled/RecoveryRequired → 排队者持久沉降 Cancelled/ERR_SUPERSEDED 清槽；无 active 受理先沉降滞留者；三处 supersede 落盘失败从 log-only 改传播（恢复槽位+拒绝受理） | `queued_operation_settles_when_active_fails_and_never_dispatches_after_c`（**修复前失败实证**：临时撤掉沉降调用重编译 → FAIL）；`queued_settles_on_cancelled_and_recovery_required_sets_protection`；`stale_queued_slot_settled_…` |
| 批8§3 重试反例 | `7b24be95` | tests/retry_after_failure.rs：真实 serve 进程 + admin API；恒败 run command 注入 | PASS（77s 真实进程链）：同 ID 重放返回原终态零新事件；新 ID 真实执行到终态或被 ERR_RECOVERY_REQUIRED 明确拒绝——**无瞬时 completed** |
| M3 结构化 blocker | `abe2e2e3` | try_acquire_process_release_lock 冲突：从权威持久状态取真实 Prod/Application 操作构造 ConflictBlocked；admission 窗口（持锁无 durable 记录）以 scope=Prod 哨兵表达（operation_id 空——不伪造） | `stop_conflicts_immediately_while_release_lock_held` 期望随 M3 契约更新（保护断言保留：零副作用/不排队） |
| Pingap 版本门禁 | `bded6fc4` | pingap_version_gate.py：四构建入口对齐 devtool.rs 单一事实源；挂 docker-build 前置 | 当前四入口全 0.14.3@cd74a461 exit 0（脚本实测） |
| N07 env 通道（线上事故治本） | `c46a2b94` | 内嵌形态收口 embedded_proxy_config 叠加 FILE_SERVER_PROXY_PUBLIC_BIND（OR 语义，与独立进程同词表"1"/"true"） | `embedded_config_env_channel_declares_public_bind`（修复前 false）+ 词表边界 + OR 两侧 4 个单测 |
| scope_isolation 契约步 | `fdf1f486` | 补 M5 登记 but 未实现的 required 步：窗口内两路 dev restart → 恰一胜者 + 败者 409 带 blocker.scope=Dev | 场景复跑 PASS（配合 abe2e2e3 + 容器内重编二进制） |

**文件拆分（用户要求）**：turso/mod.rs 1866 行 → mod.rs 837 + restart.rs 49 + tests.rs 983。

## 二、验证矩阵（实际命令与结果）

| 命令 | 结果 |
|---|---|
| `cargo nextest run -p rcoder-storage --features userapp-turso --no-fail-fast` | 101/101 |
| `cargo nextest run -p rcoder --no-fail-fast`（默认） | 324/324 |
| `cargo nextest run -p app_manager --all-features --no-fail-fast` | 194/195（唯一失败=既有环境项 storage_expansion_receipt，缺 RCODER_RUNTIME_IMAGE_DIGEST） |
| `cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast` | 222/222（含 retry e2e 76.7s 真实进程） |
| `cargo nextest run -p file-server-proxy -p rcoder --all-features --no-fail-fast` | 349/349 |
| `cargo fmt --all -- --check` | OK |
| `cargo clippy --workspace --all-targets --all-features` | 0 告警 |
| `cargo check -p rcoder-storage --features pg` | 0 错误（PG-only 独立编译） |

Compose 场景（容器内 dev-hot 重编全部修复后的二进制）：

| 场景 | 结果 | 说明 |
|---|---|---|
| scope_isolation_during_deploy | **PASS** | 两层修复：进程锁结构化 blocker（abe2e2e3）+ required 契约步补实现（fdf1f486） |
| two_users_share_app | **PASS** | 根因=环境残留：2026-09-16 的 bad-app-id builder 容器（lifecycle 标签冲突）。清理该确认属测试的残留容器后通过 |
| compose_regression | **FAIL（根因已精确诊断，修复待下一批）** | 见下 |
| deploy_full_chain | **FAIL（分层定位中）** | 分层证据：容器已创建 ✓；07:57:41 首次部署"orchestration started"后 pingap :9080 ConnectRefused（启动中正常）；07:58:12（31s 后）测试 upload 成功但 list/delete 打 file-server :60000 仍"error sending request"（连接层失败，非 N07 认证 401/403）；07:59:06 热部署"pod kept"完成。疑点：deploy_wait 在应用编排未就绪窗口返回，或 prod app-cli 启动慢于测试步进——待容器 app-cli 日志取证 |

## 三、compose_regression flock 根因（同进程重入，非泄漏）

**证据链**（按 batch8-followup §3 要求采集）：
- 锁路径：`/app/userapp-workspace/.app-operation-locks/builder-{app_id}.lock`（virtiofs，容器内 inode 11082218 ↔ 宿主 278341791）
- 新鲜 app 手工复现：ensure → 立即 destroy → 稳定失败 "lock acquisition failed because the operation would block"；**失败后直连 flock 探针 acquire OK**（/proc/locks 无该文件条目、fd 全扫描无持有者）——锁在请求时刻被同进程另一 fd 持有，请求结束后已释放
- **持有者**：`destroy_app_storage_controlled` 的外层 `AppOperationGuard`（`operation_lock.rs:204-227` 以无标记 flock 打开 `builder-{app_id}.lock`，`_file: Option<File>` 字段持有整个操作体期间——`ops/storage.rs:608` 将 `&guard` 传入 `execute_storage_destruction`）
- **受害者**：`UserappDevResourcesCleanup::capture` → `acquire_builder_operation` → `lock_builder_file_with_marker`（同一文件的**另一个 fd**，带 marker 的强身份锁）
- **机制**：flock 按打开文件描述符隔离——同进程两个 fd 对同一文件互斥。外层守卫（弱：无标记）先持，内层强锁重入必然 WouldBlock
- **排除**：非泄漏（请求后锁即释放）、非任务未结束、非外部进程

**修复方向（下一批，需充足上下文）**：Dev 域 destroy 的内层 `lock_builder_file_with_marker` 是权威变更租约（marker+身份）；外层守卫的文件锁与之重复且造成自死锁。方向：Dev destroy 路径的外层守卫不重复持有 builder 族文件锁（保留进程内 tokio 互斥 + durable admission 排他），或 `bind_lease` 复用守卫租约而非新开 fd——需评审租约/恢复语义后实施。**禁止**：删锁文件、强制解锁、吞 Conflict。

## 四、附带处理

- **userapp_storage 测试重开改有界轮询**（c46a2b94 附带）：R01 修正后锁随 worker 线程退出释放（Drop 只发信号不 join），立即重开需容忍异步收束窗口——不变量的正确后果
- **build-agent-docker**（工作树，未提交）：R06 四项同步（start-services.sh turso 分支/docker/TURSO.md/.env.turso.example/turso-volume）；用户指令两项（缓存 200Gi 默认 + 不兼容旧卷默认 B 态）——渲染矩阵 74/74、YAML 解析、bash -n、Compose 契约门禁全过；k8s-test overlay 实渲染确认 StatefulSet + storage:"200Gi"
- **two_users 残留清理**：仅删除证据确认属测试的 bad-app-id builder（2026-09-16 创建、application-id=bad-app-id 与测试固定复用名一致）；2 天旧的 e2eud* builder 未动（不属本测试）

## 五、仍未完成

1. **compose_regression flock 重入修复**（根因已诊断，方向已定，实现待下一批）
2. **deploy_full_chain**（复跑进行中；此前失败=prod file-server 转发连接失败，需分层定位）
3. **完整 make test-e2e**（本轮改动后的全量回归）
4. **端到端 SIGTERM 反例**（R02 关机顺序的真实进程验证——单元门闸已过，Compose 级待跑）
5. **remote-k8s 回归**（共享关机/内核改动按 AGENTS 需补 PG/K8s 验证——131 + .env.local 已就绪）
6. NT 三平台完整矩阵、N07 独立入口令牌认证层、R02 attach 语义（前轮遗留，不变）
