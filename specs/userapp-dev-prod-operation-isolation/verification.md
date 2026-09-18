# userapp dev/prod 操作隔离——验证记录

日期：2026-09-18。分支 `feature-userapp`。

## 源码基线

- 实施前 HEAD：`54ab0c12`（M1 提交于 `d0c89ebb`）
- 本任务提交：`d0c89ebb`（M1 类型基础）→ `079df176`（M2 三槽位模型+迁移）→ `01660640`（M2 rustfmt）→ `664a53de`（M3 查询/错误契约）→ `07a0b261`（M4 后台围栏与锁）→ M5（本批：e2e 场景+契约登记+文档）
- 无关改动保留：`tests-e2e/tests/compose_userapp_build_rules.rs`（上一任务场景的 rustfmt 重排）、`specs/rcoder-local-cache-per-replica-rbd/`（他特性）
- 过程事件：19:21 并行会话执行 `git stash` 收走未提交的 M2 全部改动；经 `git stash apply stash@{0}` 完整恢复并立即提交固化（22 文件 +881/-135 无缺失，恢复后 94/94 通过验证）。`stash@{0}` 保留未删（内容已含于 079df176）。

## 修复前反例证据（先失败后修复）

1. 组件级（真实 SQLite，修复前运行输出）：
   - `sqlite_cross_scope_admission_is_independent` FAILED：`dev restart must be admitted while prod outcome is unknown: OperationInProgress("scope-prod-wake")`
   - `sqlite_dev_uncertainty_does_not_block_prod` FAILED：`prod start must be admitted while dev outcome is unknown: OperationInProgress("reverse-dev-ensure")`
   - 反向护栏抓到实现中期 bug：blocker 先 find 后 filter 跨域漏拦（`prod slot stays occupied` 失败）→ 谓词并入搜索后修复。
2. compose 级（旧容器 23h 前构建，报告 `d8336c17133f477e83f515b4afec9128`）：
   - `dev restart 与 prod 部署并发 → 独立受理` FAILED：**HTTP 409 ERR_CONFLICT "A conflicting application operation is in progress"**——app104 事故现场在 compose 复现
   - `同域并发 prod start → … blocker.scope=Prod` FAILED：旧信封只有 code/message/tid/success，无 blocker 字段
   - `current 数组双 scope 同现` FAILED：旧返回单对象无 scope
   - （前两轮 `d58238f11dad48c6a090a3ed6ec33ff9`/`ba85009815644672a00fe7090d2fc712` 为场景自身缺陷迭代：pinned future 不 poll 不执行导致无真实并发窗口——已改 owned spawn 后修正）

## 实际命令与退出码（修复后）

| 命令 | 结果 |
|---|---|
| `cargo nextest run -p rcoder-storage --features sqlite --no-fail-fast` | 94/94 PASS（含 5 个新用例）exit 0 |
| `cargo nextest run -p shared_types --no-fail-fast` | 263/263 PASS exit 0 |
| `cargo nextest run -p app_manager --all-features --no-fail-fast` | 194/195；唯一失败 `storage_expansion_receipt_is_bound_to_the_update_operation` 为**存量问题**（HEAD d0c89ebb worktree 复现失败；缺 `RCODER_RUNTIME_IMAGE_DIGEST` env） |
| `cargo nextest run -p rcoder --no-fail-fast` | 323/323 PASS（含 cleaner 围栏新用例）exit 0 |
| `cargo nextest run -p docker_manager --all-features --no-fail-fast` | 207/207 PASS（5 skipped 环境门控） |
| `cargo check -p rcoder-storage --features pg` | exit 0 |
| `cargo check --workspace --all-targets` | 0 error |
| `cargo fmt --all -- --check` / `cargo clippy`（shared_types/rcoder-storage/container-runtime-api/app_manager/docker_manager/rcoder --all-targets） | 全部通过 0 error |
| app-cli 独立：`cargo fmt --manifest-path crates/app-cli/Cargo.toml -- --check`；`cargo clippy … --all-targets`；`cargo nextest run --manifest-path crates/app-cli/Cargo.toml --all-features` | 218/218 PASS、clippy 0 error；fmt --check 报 `runtime_kernel.rs:1408` 既有漂移（7f82f943 提交遗留，非本任务文件，未改动） |
| `python3 tests-e2e/tools/run.py --suite pg_storage_faults` | **PASS**（真 PG17 容器；含新增跨域契约 cross_scope/dev_uncertainty + 0007-PG 迁移；报告 3b163c670af04448b328c3c209ce2731） |
| `python3 tests-e2e/tools/run.py --suite sqlite_storage_contract` | **PASS**（冻结目录含 5 个新用例；报告 79c7eef2a9cb4b58a6222c702f3844a1） |
| `grep current_operation_id crates/ --include=*.rs` | 生产代码清零；仅存于测试 legacy fixture SQL/注释与 0007 迁移 |

### 覆盖对照（plan §7 十一项）

| # | 项目 | 落点 | 结果 |
|---|---|---|---|
| 1 | prod RecoveryRequired + dev restart/ensure | storage 反例 1（SQLite+PG 契约）+ compose 场景（409 反例取证） | 组件过；compose 修复后复跑受阻（见下） |
| 2 | builder 缺失 ensure 恢复 / strict restart | strict restart 语义保留（未改）；隔离场景覆盖 ensure 不被 prod 阻塞路径 | 过（组件） |
| 3 | dev RecoveryRequired 不阻 prod；同域冲突 | storage 反例 2 + scoped_terminals | 过 |
| 4 | 双 PG 执行者并发 | PG 契约（真库 4 连接池 tokio::join! 既有模式+新契约） | 过 |
| 5 | 双域终态各清己槽 | scoped_terminals_clear_own_slots_and_blocker_is_structured | 过 |
| 6 | DeleteApplication 竞态/整体围栏 | scoped_terminals + 反例 1 | 过 |
| 7 | 请求 ID 重放/跨环境误用 | 反例 1（blocker 拒绝）+ 既有 replay 契约含 scope 比较 | 过 |
| 8 | 旧 JSON 迁移矩阵/幂等/不半提交 | sqlite_legacy_pointer… + sqlite_scope_migration_aborts…（dev/prod/终态/悬空/未知 kind）+ PG 契约 | 过 |
| 9 | 执行者死亡扫描/租约 | 既有 restart quarantine（槽位化改写）+ terminal lease 契约 | 过 |
| 10 | cleaner 与 dev 并发 | cleaner 围栏单测（dev/application 围栏、prod-only 放行、读失败 fail-closed） | 过 |
| 11 | Java 透传 409 结构化详情 | **未完成**（跨仓，见待办）；正常流程不退化由全量组件回归覆盖 | Rust 侧过 |

## 受阻与未运行项

1. **compose 修复后复跑**：新场景 `userapp_scope_isolation_during_deploy` 已实现并登记（contracts.py/suite_cases.json/report_identities.json 三件表）；`make dev-hot` 两次被环境操作策略拦截，compose 容器仍运行旧代码——修复前反例已取证，修复后 PASS 需待环境刷新（`make dev-hot` 或 `make dev-restart`）后执行：
   `python3 tests-e2e/tools/run.py --suite compose_userapp_deploy --filter userapp_scope_isolation_during_deploy`
   其余 compose 套件同理需环境刷新后按 `make test-e2e` 全量回归。
2. **remote-k8s `SUITE=userapp`**：未运行（环境与镜像推送授权未确认；K8s 模式 PG 后端将执行 0007 迁移于测试 namespace）。前置：`.env.local` 就绪、`make remote-k8s-doctor`、`make remote-k8s-verify SUITE=smoke` → `SUITE=userapp`。
3. **Java（agent-platform）待办**：`ComputerPodClient.java` `parseClientErr`（约 :274-294）丢弃 operation_id；需其领域层与响应模型透传新信封 `blocker{scope,operation_id,kind,state,step}` 与 `GET /operations/current` 数组化返回；`GET current` 形状由单对象改数组属破坏性变更，Java 消费方需同步适配。本轮仅交付 Rust 侧与文档契约，未改 agent-platform。
4. 存量失败（非本任务）：`app_manager storage_expansion_receipt…`（env 依赖，HEAD 复现）；`app-cli runtime_kernel.rs` fmt 漂移（7f82f943 遗留）。

## 状态区分

- **源码修复完成**：是（M1–M4 已提交，M5 代码+登记本批提交）。
- **测试通过**：组件/真库存储契约全绿；compose 修复后复跑与 remote-k8s 待环境。
- **部署迁移生效**：否——0007 迁移仅随新二进制在 store.open() 执行，未在任何现场库执行。
- **存量恢复**：否——现场 app104 的 prod RecoveryRequired 操作按 runbook 归 Prod 槽保留，dev 操作即可解阻塞，但现场处置未执行。

## 部署迁移 runbook（维护窗口一次性，禁止新旧协调器混跑）

1. 停止接收新的 userapp 变更请求，排空可完成操作（`GET /operations/current` 全 app 空数组）。
2. 停止全部旧 RCoder 执行者/后台扫描器（SQLite 单进程独占目录锁保证无残留写者；PG 确认无存活写连接）。停止进程不证明在途 K8s 写已结束——未终态记录保持其保护，迁移会把它们归入对应槽位。
3. 备份 `userapp_lifecycles`/`userapp_operations`（含 `_sqlx_userapp_migrations`）；备份须可恢复且不含私密部署输入。
4. 启动新版本：`store.open()` 自动执行 0007（PG=DO 块守卫，SQLite=临时 trigger 守卫）。悬空指针/未知 kind → 迁移整体回滚、启动失败：保留库做诊断，人工修复后再启动（不可删记录绕过）。
5. 校验：抽样 `active_operations` 三槽与操作 `scope` 一致；`GET /operations/current` 返回形状为数组。
6. 回滚规则：**不允许旧版本直接读迁移后的库**。仅当全部槽位收束、无并行活动记录且执行显式降级转换（槽位→单指针、剥离 scope，需专门脚本）后方可回旧版本；否则保持新 schema 向前修复。备份恢复不得在外部资源已变化后盲目执行。
7. 现场 app104 处置（本任务不执行）：其 prod 唤醒 RecoveryRequired 操作迁移后归 Prod 槽、operation_id/revision/checkpoint/receipt 原样保留；dev restart/ensure 立即可用；prod 恢复按既有 retry/证据链流程另行处理。
