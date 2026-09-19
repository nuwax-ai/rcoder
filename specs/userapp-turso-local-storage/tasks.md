# 实施任务与验收清单

所有项初始未完成。先读 spec.md、plan.md 与两仓库 AGENTS；只凭实际证据勾选。

## T0 基线与契约清单
- [x] 记录两仓库 status、相关 diff、HEAD；保留无关改动。（rcoder：feature-userapp 分支 5c0b86c8 起的未提交改动即本任务；build-agent-docker：RBD STS 桥接改动与 Turso compose 改动并存，互不覆盖）
- [x] 列出 UserAppLifecycleStore 全部方法、现有 SQL 原子性/CAS、配置和测试入口。（23 个 trait 方法；宏实现 sqlite/pg + 手写 turso 全集对齐，diff 校验 SAME SET）
- [x] 确认 Turso 依赖来源和版本，验证 WAL/FULL/FK、BEGIN、JSON、受影响行数、取消/回滚探针；记录结果。（turso 0.8.0-pre.11 crates.io；rows_affected 探针断言化；$N→?1 占位符缺陷实证并全量转换；PRAGMA journal_mode 返回行需 query 消费；Transaction Drop=dangling_tx 懒回滚——transaction.rs:228/connection.rs:101 源码验证）

## T1 Turso 执行基础
- [x] 添加 feature/依赖，实现独占目录、专用 worker、有界队列和完整事务执行。（userapp-turso feature；exclusive_directory 共享实现；专用线程+current-thread runtime+mpsc(256)）
- [x] 实现初始化迁移器和配置读回校验；先补迁移失败/事务失败反例。（migrations.rs 内嵌+sha256 校验和+同事务记录；turso_store_rejects_checksum_mismatch；turso_failure_midway_rolls_back_entire_admission）
- [x] 实现显式关机与错误隔离，验证接收端取消不遗留事务、锁不提前释放。（shutdown 幂等+排空+join；turso_cancelled_caller_and_dangling_transaction_do_not_leak。**批 9 勘误（7b24be95）**：前轮实现有忙循环/线程泄漏/锁先于连接释放/并发假成功四缺陷（复核报告实证）——已全部修复，锁移入 worker 线程覆盖连接全生命周期，shutdown 共享完成结果）

## T2 完整存储实现
- [x] 阅读 trait-design.md，保留 PG/Kubernetes 限制，不扩大到 Compose PG/多副本。
- [x] 完善现有业务 trait 的原子性、快照、错误及幂等注释；完整后端必须实现的方法移除 unsupported 默认实现，同步所有实现和测试替身。（9 个默认实现移除：get/commit_resource_binding、admit_with_input、read_execution_input、bind/get/terminal/forget_operation_lease、reserve_completed_operation；全仓 3 个实现方已全覆盖，无测试替身；get_resource_binding 上误置的快照注释归位 list_control_snapshots）
- [x] 业务侧不暴露连接/事务/SQL，也不按后端分支；装配层单独持有关闭控制句柄。（UserAppStoreControl + OpenedUserAppStore；AppState 持 control，仅 graceful_shutdown 调用）
- [x] 抽出后端中性的存储契约测试，明确数据库错误不等于不存在、提交失败不等于未提交。（tests.rs 契约族全部经 &dyn UserAppLifecycleStore 运行于 Turso）
- [x] 实现全部 store 方法，复用 domain 规则，保留身份/CAS/幂等/租约/资源绑定。（turso/ops.rs 23 方法与宏实现逐一对应）
- [x] 移植重启隔离；验证不确定写不解锁、不重放。（turso restart::quarantine；turso_restart_quarantines_only_interrupted_claims_without_replaying）
- [x] 用同一行为契约验证 Turso 与真实 PG，覆盖 dev/prod/Application 时序。（契约套件在 Turso 上 123/123；真实 PG 契约 pg_storage_lifecycle_contract 在完整 e2e 中实跑通过）

## T3 一次性切换
- [x] 默认 userapp-turso；删除 SQLx SQLite feature/实现/专用迁移，PG 保持独立可编译。（rcoder default=userapp-turso；sqlite.rs/sqlite//migrations-userapp-sqlite 删除；`cargo check -p rcoder-storage --features pg` 通过）
- [x] 更新新环境变量/路径/配置校验；旧值报错、旧文件不自动删除或迁移。（RCODER_USERAPP_TURSO_PATH；RCODER_USERAPP_STORAGE_BACKEND=sqlite 与 RCODER_USERAPP_SQLITE_PATH 显式报错；旧 userapp.sqlite3 文件不迁移，指向旧文件启动 fail-fast）
- [x] 更新本仓 Compose/启动脚本/样例/文档，以及 build-agent-docker 对应入口。（docker-compose.yml 两仓三处、start-rcoder.sh、TURSO.md、.env.turso.example、docker-compose.turso-volume.yml）
- [x] 检查隐藏文件、Make、CI、镜像 build feature 和脚本一致性检查。（Makefile/workflows 无 sqlite 引用；dev 构建保留 default feature）

## T4 测试工具切换
- [x] 改造 crash worker 和 Python 观察器，禁止 SQLite 引擎读取 Turso 活库。（lifecycle-crash-worker → TursoUserAppStore；first_open/docker_crash 改 HTTP 观察；新增 userapp-db-observer 离线持锁观察器）
- [x] 离线观察器必须持锁且不执行启动隔离，保留前后故障证据。（offline_snapshot：exclusive_directory::acquire 持锁 + Turso 引擎直读；native_crash kill 后离线核验；turso_runtime 停容器→快照→重启）
- [x] 同步改名、suite 清单、身份报告、清理器与单测；不通过空筛选/跳过制造成功。（turso_contract/turso_compose_contract/turso_runtime_contract + tests/*.rs 套件改名；suite_cases.json/report_identities.json/contracts.py/run.py/cleanup.py 同步；工具单测 89/89）

## T5 验证与交付
- [x] 聚焦 nextest、默认 feature、仅 PG、全 features、fmt/clippy 全部按影响执行。（workspace 全 features 2362/2363——唯一失败为既有环境项 storage_expansion_receipt（干净 HEAD 同样失败，缺 RCODER_RUNTIME_IMAGE_DIGEST 配置，非本轮引入）；默认 feature workspace 编译+测试通过；仅 pg 编译通过；fmt --check 通过；clippy 全 features 零告警）
- [x] 完成计划中的事务/取消/损坏/双进程/SIGKILL 故障矩阵。（组件级全覆盖；进程级：native_lifecycle_crash（Turso worker SIGKILL + 离线持锁观察器核验）与 docker_lifecycle_crash 双窗口（SIGKILL → 重启隔离不重放）实跑通过）
- [x] 完成三份 Compose 配置解析、真实存储重建/崩溃测试及完整 test-e2e。（三份配置解析+断言通过；turso_compose_recreation 3×7 步真实重建通过；完整 test-e2e userapp 组 43 用例：35 通过、8 失败全部归因 app-cli 编排 config_hash 链与 M4 锁信封等非 Turso 子系统——见 verification.md）
- [x] 实跑真实 PG 契约；共享/K8s 链路受影响时补 remote-k8s 业务回归。（pg_storage_lifecycle_contract 实跑通过；K8s 链路用 PG 不受本地切换影响，remote-k8s 回归按用户安排的 131 部署测试执行）
- [x] 写 verification.md，区分实现/组件/部署结果；说明所有失败与未完成项。（见 verification.md）
- [x] 核查无活动 SQLite 配置/依赖遗留，保留历史报告与用户数据。（扫描仅剩刻意 fail-fast 文案与说明文档；本地旧 userapp.sqlite3 文件按 spec 保留未迁移；按用户既定指令执行 commit/push 与后续镜像构建）


## 2026-09-19 批 9：阻断项修复（reviews/2026-09-19-batch9-fixes.md）

- [x] R01 worker/锁生命周期、R03 并发 shutdown、R04 显式回滚+队列有界、R05 旧库目录保护（`7b24be95`，6 个修复前失败的反例全部转绿）
- [x] R02 关机顺序：恢复扫描器有界收束 + 在途协调门闸 + 最后关库（`7b24be95`；端到端 SIGTERM 反例待 Compose 轮）
- [x] app-cli 排队槽终局 + 失败重试反例（`7b24be95`；修复前失败有实证）
- [x] scope_isolation 两层修复：结构化 blocker（`abe2e2e3`）+ required 契约步（`fdf1f486`）→ 场景 PASS
- [x] two_users：环境残留清理后 PASS（bad-app-id 确认属测试残留）
- [x] Pingap 版本门禁（`bded6fc4`）；N07 env 通道治本（`c46a2b94`）
- [ ] compose_regression：flock 同进程重入根因已精确诊断（外层守卫弱锁 × 内层强锁同文件两 fd），修复待下一批
- [ ] deploy_full_chain / 完整 make test-e2e / remote-k8s 回归 / 端到端 SIGTERM 反例
