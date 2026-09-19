# verification — userapp Turso 本地存储切换

日期：2026-09-19。源码基线：rcoder `feature-userapp` 分支（上一个提交 `5c0b86c8` + 本轮未提交改动）；build-agent-docker 工作树（RBD STS 桥接改动与本轮 Turso compose 改动并存）。命令均在 `/Users/soddy/Documents/git-workspace/rcoder` 执行，退出码以 `echo $?` 记录。

## 实现完成（代码存在 ≠ 验收通过；各节分开看）

### T1/T2 Turso 后端（rcoder-storage）
- `crates/rcoder-storage/src/userapp_lifecycle/turso/`：mod（worker/门面/关机/控制接口实现/offline_snapshot）、exec（SQL 薄层 + Tx）、migrations（内嵌迁移+sha256）、ops（23 个 trait 方法）。
- `migrations-userapp-turso/0001_init.sql`：最终形态 schema（无 legacy 迁移——spec 明确 Compose SQLite 未上线、无存量数据）。
- 独占目录：`exclusive_directory.rs` 从 sqlite/ 提升为共享实现（别名/硬链/远程 fs 防护、措辞后端中性化）。

### T2 trait 收紧（shared_types）
- `crates/shared_types/src/userapp/lifecycle.rs`：移除 9 个 unsupported 默认实现（get_resource_binding、commit_resource_binding、admit_with_input、read_execution_input、bind_operation_lease、get_operation_lease、terminal_operation_leases、forget_operation_lease、reserve_completed_operation）——完整后端编译期必须实现全集。
- 契约注释按 trait-design §3 补全；“每页单语句快照”注释从 get_resource_binding 归位到 list_control_snapshots。
- 实现方核对：`grep impl UserAppLifecycleStore` 全仓仅 3 处（宏 sqlite→已删/pg + turso 手写）；turso 与宏方法集 diff 为 SAME SET（23 个）。

### T2 §6 控制接口
- `crates/rcoder-storage/src/userapp_lifecycle/control.rs`：`UserAppStoreControl` + `OpenedUserAppStore{store,control}`。
- Turso：与业务门面同 Arc 实现 control（shutdown 委派）；PG：close 自有池（生产只经 connect() 构造，外部共享池不构造 control）。
- `crates/rcoder/src/config/userapp_storage.rs::open` 返回 OpenedUserAppStore；AppState 增 `userapp_store_control` 字段；`main.rs` → `shutdown.rs::graceful_shutdown` 在清理容器/flush 前调用 `control.shutdown()`。

### T3 一次性切换
- rcoder `default = ["userapp-turso"]`；`userapp-sqlite` feature 删除；`rcoder-storage` `sqlite` feature 删除。
- 删除：`sqlite.rs`、`sqlite/restart.rs`、`migrations-userapp-sqlite/`。
- 配置：`UserAppStorageBackend::{Auto,Turso,Postgres}`；字段 `sqlite_path`→`turso_path`（默认 `data/rcoder/userapp.turso.db`）；env `RCODER_USERAPP_TURSO_PATH` 新增；`RCODER_USERAPP_STORAGE_BACKEND=sqlite` 与 `RCODER_USERAPP_SQLITE_PATH` 显式报错（含替换指引）；旧 `userapp.sqlite3` 文件不迁移，指向旧文件时迁移 fail-fast。
- Compose/脚本/文档（两仓）：`docker-compose.yml`×3（rcoder docker/、build-agent-docker docker/ 与 docker-userapp-computer/）env 改 turso；`start-rcoder.sh` 预检块改 Turso；`SQLITE.md`→`TURSO.md`（两仓）；`docker-compose.sqlite-volume.yml`→`docker-compose.turso-volume.yml`（两仓）；`.env.sqlite.example`→`.env.turso.example`（两仓）。

### T4 测试工具
- `tests-e2e/src/bin/lifecycle-crash-worker.rs`：`SqliteUserAppStore`→`TursoUserAppStore`（路径 `userapp.turso.db`）。
- 新增 `tests-e2e/src/bin/userapp-db-observer.rs` + `rcoder-storage::userapp_lifecycle::turso::offline_snapshot`：持实例目录锁、Turso 引擎直读、不迁移不隔离——Python sqlite3 观察全部退役。
- `first_open_contract.py`：受理窗口与终态对账均走 HTTP（原注释已实测宿主直读容器 SQLite 会致 SIGBUS；T4 明令禁止 SQLite 引擎读 Turso 活库）。
- `docker_crash_contract.py`：`db_operation` 改 HTTP `operations/current` 轮询。
- `native_crash_contract.py`：kill 前只断言屏障文件+operation.lock（普通文件）；kill 后离线持锁观察器核验终态行。
- `sqlite_contract.py`→`turso_contract.py`、`sqlite_compose_contract.py`→`turso_compose_contract.py`、`sqlite_runtime_contract.py`→`turso_runtime_contract.py`（快照=停容器→观察器→重启探活；非法启动尾部用 `RCODER_USERAPP_TURSO_PATH=/app/data`）。
- 套件接线：`tests/turso_storage_contract.rs`、`tests/turso_compose_runtime.rs`；`run.py` GROUPS、`suite_cases.json`、`report_identities.json`、`contracts.py` REQUIRED 键、`cleanup.py` 同步；`pg_contract.py` 构建 feature 改 `pg,userapp-turso`。
- 冻结清单 `storage_contract_cases.py`：TURSO_CASES(28) + TURSO_EXTRA_CASES(8，turso::tests 结构性保护，含新补的两个 quarantine 反例测试)。

## 组件测试（本轮实际运行）

| 命令 | 结果 | 退出码 |
|---|---|---|
| `cargo nextest run -p rcoder-storage --all-features --no-fail-fast` | 123 tests: 123 passed, 1 skipped（PG DSN 门控） | 0 |
| `cargo nextest run --workspace --all-features --no-fail-fast` | 2363 tests: 2362 passed, 1 failed, 13 skipped | 非 0（见失败归因） |
| `cargo check -p rcoder-storage --features pg` | 无错误无告警 | 0 |
| `cargo check --workspace --all-targets`（默认 feature） | 无错误无告警 | 0 |
| `cargo fmt --all -- --check` | 通过 | 0 |
| `cargo clippy --workspace --all-targets --all-features` | 0 告警 | 0 |
| `E2E_REPORT_DIR=... python3 tests-e2e/tools/turso_contract.py` | 36 冻结用例逐项通过（build+list+逐条 --exact 执行） | 0 |
| `python3 -m unittest discover -s tests-e2e/tools -p "test_*.py"` | 89 tests OK | 0 |
| `python3 tests-e2e/tools/turso_compose_contract.py <三份 compose>` | 三份配置解析+断言 OK（含 named-volume 变体） | 0 |

Turso 关键行为证据（T0 探针，源码级）：
- `$N` 占位符 UPDATE/DELETE rows_affected 恒 0（数据实际生效）而 `?N` 正确 → 全部 SQL 已用 `?N`（105+ 处）；探针 `turso_execute_rows_affected_semantics` 断言化。
- `PRAGMA journal_mode=wal` 赋值返回结果行——必须 query API 消费（execute_batch 报 unexpected row）。
- `Transaction` Drop 记 `dangling_tx=Rollback`，连接下一次任意语句前先回滚（turso 0.8.0-pre.11 `src/transaction.rs:228` Drop、`src/connection.rs:101` maybe_handle_dangling_tx、`src/transaction.rs:289` transaction_with_behavior 先处理 dangling）——worker 单连接串行下 `?` 错误路径=懒但必然回滚，exec.rs 文档已按源码事实更正。

## 失败归因
- `app_manager service::tests::storage_expansion_receipt_is_bound_to_the_update_operation`：`image not provided and RCODER_USERAPP_IMAGE_DIGEST env not set`。**干净 HEAD（git stash 后）同样失败**——既有环境依赖失败，与本轮改动无关，待归因项保留。

## 部署验收（本轮实跑，2026-09-19）

镜像链：`make dev-restart`（dev-master-rcoder 重建，Turso 已编入；其间同步了 agent-runner start-up.sh 与生产 build_config 1ffc1af 的漂移）→ 容器内 dev-hot 编译（一次性容器执行 dev-hot-build.sh，3m32s）→ compose 起服（/health healthy；userapp.turso.db + WAL 在挂载目录生成；turso 引擎逐语句 DEBUG 经 RUST_LOG 目标过滤收敛）。

完整 `make test-e2e`（userapp 组，43 用例）+ 修复后聚焦重跑：

| 用例 | 结果 | 说明 |
|---|---|---|
| turso_storage_lifecycle_contract | **pass** | 36 冻结组件契约逐条执行（首次失败为 report_identities.json 只改键未改值——已修，重跑过） |
| turso_compose_recreation_contract | **pass** | 3 份 Compose 配置 × 7 步：配置解析、HTTP 首开收敛、唯一 builder、HTTP 落库对账（停容器→持锁离线快照→重启）、重建身份保持、非法路径启动拒绝、定向清理（首次失败为 first_open 未适配 operations/current 的 Vec 形状——已修，重跑过） |
| native_terminal_release_crash_contract | **pass** | Turso crash worker SIGKILL；kill 前只断言屏障/锁文件，kill 后离线持锁观察器核验终态 |
| docker_runtime_crash_recovery_contract | **pass** | before_create/after_start 双窗口：屏障、SIGKILL、重启隔离不重放、定向清理（两次中间失败为 docker_fault_proxy/owned_builder_rows 仍要求已退役的 owner-id 标签、db_operation 未适配 Vec——均已修，重跑过） |
| pg_storage_lifecycle_contract | **pass** | 真实 PG 契约实跑通过 |
| userapp_concurrency_component_contract / docker_deletion_identity_contract | **pass** | |
| 其余 userapp compose 用例 | 26/34 通过 | 失败 8 例见下 |

### e2e 失败归因（8 例，均非 Turso 引入）
1. **app-cli 编排 config_hash 链（6 例：dev_server_lifecycle、dev_app_proxy_lazy_start、devbuild×2、compose_regression、two_users_share_app）**：builder 容器内 app-cli 报 "confirm initial Pingap config via loopback admin probe: config_hash mismatch (expected 90755485, observed 373F3032)"；dev 服务起不来 → 在途操作持 builder 文件锁 → destroy/ensure 连锁 Conflict。镜像内 pingap 0.14.3 与 pin 一致、app-cli 版本号与源一致（源码自镜像构建后无新 commit）、比对大小写不敏感——疑为 config.hash()（序列化前内存态）与 pingap 加载重解析后 hash 的往返差异，属 app-cli 子系统待归因；compose_regression/two_users 的 lease Conflict 为其级联。本轮 diff 不含 app-cli 与 pingap 配置生成路径。
2. **M4 进程锁信封（1 例：scope_isolation_during_deploy）**：并发 start 的 409 由进程锁层先发出（"application operation is in progress"，data=null），无 storage 层的 blocker.scope=Prod 结构化数据——M4（07a0b261 进程锁 scope 化）行为迁移后测试预期未同步，属该工作流。
3. **同 1 级联（1 例：deploy_full_chain 的 app-files 转发）**：部署服务不可达（config_hash 链）。

历史对照：workspace 组件测试既有环境失败 storage_expansion_receipt（干净 HEAD 同样失败）不在 e2e 组内，维持待归因。

### 环境事实
- dev compose 数据目录旧 userapp.sqlite3（5.7MB）按 spec 保留未迁移；Turso 新库独立生成。
- 旧容器曾以旧二进制 + 新 env 崩溃循环（"must be auto, sqlite or postgres"）——dev-hot 产物优先于镜像二进制所致，容器内重编后恢复；属 dev 流程已知形态，非缺陷。

## 远端 K8s 回归（131，2026-09-19 追加）

`make remote-k8s-verify SUITE=smoke`（源码快照 531330e9 / head dee80dc3 远端原生构建）→ 同快照追加 test：

| 套件 | 结果 | 说明 |
|---|---|---|
| smoke | **pass** | 部署身份、双副本就绪、PVC Bound、直连健康（131:31290 / Gateway :31536） |
| userapp | **pass**（51 场景零缺失） | ensure/builder 身份/生命周期/跨副本文件/构建/制品/cold+hot deploy/hot_env_rejected/hot_failure/hot_pod_preserved/锁冲突全景（busy×4、cross_replica、stale_version、version_keeps_resources、holder 系列）/tombstone/recreate/stop 幂等/SSE 终态/wake——**PG 后端 + 收紧后 trait + 新控制接口/关机接线在真实 K8s 全链路验证** |
| gateway | **pass** | Gateway/HTTPRoute 条件 + 健康接口实际请求 |
| chat | **pass** | 真实 AI 会话链路 |

### 归因记录
- 首轮 userapp 失败（部署应用 502，readiness :3010 拒连）：`.env.local` 基础镜像钉在 **0.1.265**，app-runtime 内旧 app-cli 与 HEAD 源码的 lock/编排契约漂移（已知"改 lock 必须重建 app-runtime 镜像"模式）。将基础镜像切到当日同源码构建的 **0.1.274**（build-agent-docker `make k8s-helm-rcoder-version-publish ENV=test AMD64_ONLY=1` 产物：rcoder-k8s/computer-agent-runner/app-runtime-base/app-runtime + chart 0.1.274 OCI）后全套通过——与 Turso 切换无关。
- `.env.local` 基础镜像已更新为 0.1.274（本地未提交配置，含陈旧 digest 回写行已清除）。

## 未完成项 / 后续
- app-cli config_hash 往返差异（6 例 e2e 失败根因）与 M4 锁信封（1 例）：移交对应子系统工作流；不阻塞 Turso 交付。
- build-agent-docker K8s 镜像构建（`make setup k8s-helm-rcoder-version-publish ENV=test`）与 131 K8s 业务测试：按用户收尾指令执行。
- cargo 单 feature 陈旧 fingerprint 问题（`--features userapp-turso` 偶发不重编）：本轮用 --all-features 与干净重试规避，待上游归因，不阻塞。
