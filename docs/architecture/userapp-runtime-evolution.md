# UserApp 运行时编排：长期演进方案

> 状态：草案（第 2 版），供评审。范围聚焦 UserApp/builder 的运行时编排层：`container-runtime-api` trait 面、`docker_manager` 的 Docker/K8s builder control、`rcoder-engine/userapp_builder` 编排、`rcoder-storage/userapp_lifecycle` 持久化，以及 `runtime-supervisor` 的 owner/进程存活判定与 `shared_types/userapp` 契约。
>
> 本文不是重构指令，而是一份「长期往哪走、分几步走、每步怎么验收」的方向性方案。任何一阶段落地前都应先按第 5 节拆出独立的 spec/plan/tasks。
>
> **现状核对基线**：初版写于 `db1ef875`；本版按 `793004ea`（2026-10-15）重新核对代码。两轮之间 `db1ef875..HEAD` 改动 202 个文件（+21527/−1752），大部分落在本文范围内，因此第 1–3 节的现状描述与第 5 节的阶段状态均已重写。文中所有计数均为该基线下 `grep` 实测值（除特别说明外均排除 `*_tests.rs` 与 `tests/` 目录），随代码演进需重新采集。

## 1. 背景与问题

RCoder 的 UserApp 是一个**多租户、有状态、长生命周期**的计算单元：builder 以 StatefulSet + `volumeClaimTemplates`（K8s）或容器 + bind-mount/PVC（Docker）形式运行，数据卷必须保留，且业务上要求「**同一应用最终只有一套业务在服务**」。

为满足 AGENTS.md 里那组 UserApp 核心原则（重复 `run` 自动复用/恢复、单 owner、停止确认后再起新实例、已发副作用未知则对账、真实错误可修正重试），运行时管理层手写了大量身份对账与存证逻辑。典型例子是 `capture_builder_volume_witness`：在危险操作前逐层校验 workload 存在、kind 为 StatefulSet、`uid` 未变、`resource_version` 未变、`deletion_timestamp` 为空、再逐个 PVC 校验归属与删除态。**它在 Docker 与 K8s 各有一份同名独立实现**（`docker_manager/src/runtime/docker_builder_restart.rs:697` 的 `pub(super)` 版本 / `k8s_builder_control/ops.rs:56` 的 `pub(crate)` 版本），共 14 个非测试调用点分散在 6 个文件里。

这套设计的「积木」全部是社区标准惯用法（StatefulSet 稳定卷、`ownerReferences` 归属、`resourceVersion` 乐观并发、`uid` 判代次）。问题不在概念，而在**工程形态**：

- **对账逻辑在两个方向上都没有收敛**。横向看，同一语义的 witness 采集在 Docker/K8s 各写一份实现；纵向看，trait 抽象之上的编排层仍在手写比较——`rcoder-engine/src/userapp_builder/` 非测试文件中含 `==`/`!=` 的 `uid`/`resource_version` 判断共 24 处（`compute_control.rs` 10、`adoption.rs` 5、`dev_locator.rs` 3、`creation/observe.rs` 2、`auto_repair.rs`/`creation/registration.rs`/`compute_control/stopped_template.rs`/`dev_cleanup.rs` 各 1），该目录 `ok_or_else` 共 53 处，靠字符串守卫维持。
- **部分收敛已经做到，可作为模板**。K8s 内部的 `verify_workload_stable` 已收敛为 `k8s_builder_control/helpers.rs:174` 唯一的 `pub(super)` helper，被 `ops.rs` 复用 7 处而非各写一遍；trait 层的 `capture_builder_compute_volumes`（`container-runtime-api/src/runtime_trait.rs:323`）也已由双后端各自实现（Docker 侧 `docker_runtime/agent_runtime.rs:387` 委托给自己的 witness 实现，K8s 侧 `kubernetes_runtime/agent_runtime.rs:296`）。要复制的是这个形态，不是重造它。
- **快照类型碎片化**。已验证的物理快照以多个互不组合的 evidence 类型返回：`BuilderCreationEvidence`、`BuilderOrphanStopTarget`、`AppAdoptionTarget`、`BuilderRestartTemplate`、`UserAppComputeStartTarget`，以及裸 `Vec<AppResourceIdentity>`。调用方需要自己判断"这次拿到的快照够不够授权下一步写操作"，而这条判断没有被类型固化。
- **内部错误原因仍是自由字符串**。HTTP 面契约已类型化（见 §2），但产生端仍用 `Error::Conflict(String)` / `Error::ConfigurationError(String)`：非测试代码分别约 685 / 476 处（含测试为 706 / 490；计数含各错误枚举的同名变体）。`verify_workload_stable` 的返回类型就是 `Result<(), String>`，`error_diagnostic.rs` 只能把它们塌缩成泛化的 `ERR_CONFLICT`，`K8sError`/`DockerError` 落到 "Unclassified"。
- **状态推进契约已集中，但编排层分支未全部收在其后**。`shared_types/src/userapp/compute_control.rs` 已有显式状态机（`ComputeControlState` 6 态 + `ComputeControlStage` 6 阶段 + `ComputeControlAction`）与纯校验函数 `validate_compute_progress_transition()`(:267)，并在持久化边界 `rcoder-storage/src/userapp_lifecycle/common/compute_execution.rs:128` 强制执行——非法转换确实在集中点被拒。剩余问题是编排侧：2260 行的 `rcoder-engine/src/userapp_builder/compute_control.rs` 仍以命令式分支驱动这些阶段（该文件引用契约类型 27 处），阶段推进与上面那 24 处身份判断交织，新增一条恢复路径时仍要在编排层手写分支。
- 持久化里存了 `BuilderControlTarget`、receipt、历史模板，长期背着「历史数据永不假解析」的兼容包袱。字段级兼容处理已经很自觉（`restart_runtime_workspace` 注释明确"Missing in old checkpoints; recovery must never invent a new intent"，配 `#[serde(default)]`；`BuilderControlTarget` 还手写了 `Deserialize`(:97) 以在接受 compute ledger 附加字段的同时不放宽对无关输入的拒绝），但**载荷没有版本字段，也没有 N-2/N-1 旧数据回归矩阵**；SQL schema 层则已版本化（见 R4）。

**长期目标不是减少保证，而是让「新增一条能力/一条恢复路径的边际复杂度递减」**，而不是递增。否则平台越成功，运行时管理层越没人敢动。

## 2. 现状盘点（已有的良好底子）

演进方案必须建立在这些既有资产上，避免推倒重来：

| 资产 | 位置 | 价值 |
|---|---|---|
| 运行时 trait 面 | `container-runtime-api/src/runtime_trait.rs`：`WorkspaceRuntime`(:623) + `UserAppDeploymentRuntime`(:772) → `UserAppRuntime`(:1618) / `ContainerRuntime`(:1632)；`AgentContainerRuntime`(:37) 为 agent 线 | 已隔离 Docker/K8s 双后端，是收敛对账逻辑的天然挂载点。builder 系列方法在 `WorkspaceRuntime`/`UserAppDeploymentRuntime` 上，**不在** `AgentContainerRuntime` 上 |
| 类型化错误诊断 | `container-runtime-api/src/error_diagnostic.rs` | `runtime_error_code()` 变体→稳定码；`runtime_mutation_outcome_unknown()`(:64) 区分只读超时与「已发写入结果未知」并升级为 `ERR_OPERATION_OUTCOME_UNKNOWN`；`runtime_app_error()`(:78) 携带 stage/cause/retryable/operation_id；`runtime_pg_command_error()` 做凭据脱敏（测试断言 `ALTER ROLE … PASSWORD` 不回显） |
| 稳定错误码表 | `shared_types_i18n/src/error_codes.rs`（80 个 `ERR_*` 常量）+ `shared_types::ErrorDetail`(`reason_code`/`stage`/`detail`/`hint`/`retryable`/`task_id`/`service_id`) | 错误码已集中且已 i18n，Phase 3 的地基已在 |
| 运行时错误契约文档 | [docs/runtime-error-contract.md](../runtime-error-contract.md) | HTTP 面错误分类、`ERR_OPERATION_IN_PROGRESS` holder 语义、retry_after 建议、前端分支处理已成文 |
| 结构化错误变体 | `container-runtime-api/src/types.rs:25`：`OperationInProgress(Box<UserAppOperationInProgress>)`、`RequestRejected(RuntimeRequestRejection)`、`PreparationFailed(AppPreparationFailure)`、`CreationAborted { progress, source }`、`ManagementNotRunning`、`CreationCancelled` | 「受理/执行/终态」与取消语义已进类型，不再靠文案区分 |
| **纯状态机转换（无 I/O）** | `rcoder-storage/src/userapp_lifecycle/domain.rs`（"Pure transitions shared by both SQL backends. No database or runtime I/O."） | 转换逻辑可穷举/性质测试，是形式化验证的最佳起点 |
| 显式生命周期模型 | `shared_types/src/userapp/lifecycle.rs`：`UserAppLifecycleState` + `UserAppOperationState` + `UserAppOperationScope`（Application/Dev/Prod 三槽位）+ `UserAppAdmission`/`UserAppAdmissionOutcome` + `UserAppActiveOperationRecords` + `UserAppStoreError` | app/operation 层的状态与准入**已集中**，不是隐式散落 |
| **Kani 有界证明（已在跑）** | `tools/verify_kani.py` PROOFS 白名单 + `make verify-kani`：`userapp::lifecycle::kani_progress_claim_proofs`、`userapp::lifecycle::kani_scope_conflict_proofs`、`userapp::compute_control::kani_progress_policy_proofs`，另含 identifier/validation 4 个语法与长度证明 | 生命周期不变量已有机器证明锚点；harness 在 `shared_types/src/userapp/lifecycle.rs:1791,1822`、`shared_types/src/userapp/compute_control.rs:359`。状态枚举已带 `#[cfg_attr(kani, derive(kani::Arbitrary))]`，扩展新证明无需另建模 |
| **compute 生命周期显式状态机** | `shared_types/src/userapp/compute_control.rs`：`ComputeControlState`(:41，Pending/Running/RecoveryRequired/Succeeded/Failed/Superseded，含 `is_terminal()`) + `ComputeControlStage`(:156，DrainingPrevious/Stopping/Stopped/Starting/Verifying/Completed) + `ComputeControlAction`(:34) + `ComputeProgressPolicyError`(:232) + 纯校验 `validate_compute_progress_transition()`(:267) | Phase 2 想要的「`enum State` + 转换前置条件 + 非法转换集中拒绝」**已存在于契约层**，并被 `rcoder-storage/src/userapp_lifecycle/common/compute_execution.rs:128` 在持久化边界强制执行 |
| 分离式 compute 编排 | `rcoder-engine/src/userapp_builder/compute_control.rs`（2260 行，引用上述契约类型 27 处）+ `compute_control/{builder_start,stopped_template,missing_tests}` | admission 不等槽、receipt 保留至结果确认，「受理/执行/终态」解耦已实现；命令式分支仍是本层的主要形态 |
| 租约与唤醒栅栏 | `shared_types/src/userapp/operation_lease.rs`、`rcoder-storage/src/userapp_lifecycle/common/ops/{operation.rs,wake_fences.rs}` | I4「已发副作用未知则保持写保护」的落地形态 |
| 停止观察与恢复扫描 | `shared_types/src/userapp/compute_control/stop_observation.rs`、`recovery_scan.rs`、`compute_recovery.rs`；`common/{compute_execution.rs,compute_recovery.rs,compute_restart_absence.rs}` 及 `recovery_scan_tests.rs`、`root_claim_tests.rs`、`wake_fence_tests.rs`、`transaction_fault_tests.rs`、`pg_commit_reply_tests.rs`、`compute_stop_observation_tests.rs` | receipt/ledger 台账、并发控制、恢复扫描、故障注入测试的存储基础已齐 |
| owner/进程存活判定 | `runtime-supervisor`：`monitor/historical_stop.rs`、`legacy_drain.rs`、`resident.rs`、`resident/legacy_application_tests.rs` | I1/I3/I5 的实际承载者；**初版范围声明遗漏了这一层** |
| UID/version 栅栏与卷身份 | `docker_manager/src/runtime/k8s_builder_control/{helpers.rs,ops.rs,absent.rs}`、`k8s_pvc/`、`lifecycle_discovery.rs:72 discover_volume_identities`；`shared_types/src/app_resource_deletion.rs` 的 witness 比较（含"忽略 image 字段"测试） | 对账「积木」齐全，`verify_workload_stable` 已在 K8s 内部收敛；但 `capture_builder_volume_witness` 仍是 Docker/K8s 各一份实现（R1） |
| 写保护栅栏 | `fence_builder_compute_write`（`runtime_trait.rs:340`，K8s 实现于 `kubernetes_runtime/agent_runtime.rs:303`，调用于 `compute_control.rs:1748`） | I4 的运行时侧闸门已存在 |
| **版本化 SQL 迁移台账** | `rcoder-storage/src/db/schema.rs`：`rcoder_schema_migrations`(component,version,name,checksum,schema_fingerprint,applied_at_us) + `migration_ddl()`(:53) 强制加法式迁移 + 迁移历史缺口检测 | R4 的 SQL 层已治理；未治理的是 JSON 载荷层 |
| 载荷级兼容纪律 | `shared_types/src/userapp/` 下 7 个文件共 14 处 `#[serde(deny_unknown_fields)]`；`BuilderControlTarget` 手写 `Deserialize`(`builder_control.rs:97`) 用内层 `Wire` 结构精确控制放宽边界；旧字段以 `#[serde(default)]` + 注释标注"Missing in old checkpoints" | 兼容处理是自觉设计而非偶然；Phase 5 要做的是给这套纪律加版本号与回归矩阵，不是从头建立纪律 |
| 并发红线 lint | 根 `Cargo.toml:46` `await_holding_lock = "deny"`、`:34` `unsafe_code = "deny"`、`:37` `unused_qualifications = "deny"` | 部分并发纪律已编译器化，不再纯靠 review |
| 闲置回收 / 自愈 | `docker_manager/src/runtime/idle_builder_retirement/`、`rcoder-engine/src/userapp_builder/{recovery,compute_drain_recovery,auto_repair.rs,shutdown_gate.rs}` | 恢复路径已成体系，是 Phase 2 不变量测试必须覆盖的对象 |
| 迁移策略 | [docs/userapp-migration-policy.md](../userapp-migration-policy.md) | 兼容性纪律已文档化 |

**结论**：底子明显好于「纯手搓」——错误契约、生命周期模型（含 builder compute 的显式状态机与集中转换校验）、Kani 证明、SQL 迁移台账、载荷级兼容纪律都已存在，且 `verify_workload_stable` / `capture_builder_compute_volumes` 已示范了对账逻辑应有的收敛形态。真正缺的是四件事：(a) 把这种收敛**同时贯穿到横向（Docker/K8s 双份 witness 实现）与纵向（编排层手写 uid/version 比较）**；(b) 合并碎片化的 evidence 快照类型；(c) 把编排层的命令式分支收到已有的 compute 状态机契约之后，并把 Kani 门禁接进 CI；(d) 内部错误原因从 `String` 升级为带码 enum，并把「期望身份 vs 实际身份」带进 `ErrorDetail`。

## 3. 现状风险清单

| # | 风险 | 触发后果 | 严重度 | 现状 |
|---|---|---|---|---|
| R1 | 对账逻辑双向未收敛：横向 `capture_builder_volume_witness` 在 Docker/K8s 各一份实现（14 个非测试调用点跨 6 文件）；纵向 `rcoder-engine/userapp_builder/` 手写 uid/resource_version 判断 24 处 + `ok_or_else` 53 处 | 新路径漏校验 → 错接管 / 双跑 | 高 | 未治理；K8s 内部 `verify_workload_stable` 与 trait 层已示范收敛形态 |
| R2 | 编排层命令式分支未全部收在已有状态机契约之后：`rcoder-engine/src/userapp_builder/compute_control.rs`（2260 行）以分支驱动 `ComputeControlStage` 推进，并与 R1 的 24 处身份判断交织；`validate_compute_progress_transition` 只守住持久化边界，不覆盖编排侧「下一步该做什么」的决策 | 补丁侵蚀规则，边界时序出错 | 中 | 大部分治理：契约层状态机 + 集中校验 + Kani 完整矩阵证明已存在，缺口在编排层复用度 |
| R3 | 内部错误原因是自由字符串（非测试代码 `Conflict(String)` ~685、`ConfigurationError(String)` ~476；`verify_workload_stable` 返回 `Result<(), String>`）。连 I2 那样的关键 fail-fast 守卫也只是一句 `Error::Conflict("Builder volume workload missing")`(`k8s_builder_control/ops.rs:101`)，调用方无法机器判别 | 运维无法归因，误判为业务失败 | 中 | 部分治理：HTTP 面已类型化并成文（`error_diagnostic.rs` + `runtime-error-contract.md`），产生端仍是字符串 |
| R4 | **JSON 载荷层**（`BuilderControlTarget`/receipt/`BuilderRestartTemplate`）无版本字段与 N-2/N-1 旧数据矩阵 | 升级时旧数据假解析/丢字段 | 高 | 部分治理：SQL schema 层已有 `rcoder_schema_migrations` 台账 + 指纹 + 缺口检测 |
| R5 | 自研面积持续膨胀 | 无法把「集群本可保证」的部分外包，维护成本线性上升 | 中 | 未治理 |
| R6 | DashMap guard 跨 await 是否被 `await_holding_lock = "deny"` 覆盖**未实测确认**；锁序仍靠 review | 偶发死锁/竞态 | 中 | 部分治理：std/parking_lot 锁 guard 已被 deny 拦截 |

## 4. 长期目标与设计原则

**北极星指标：新增一条 UserApp 能力时，开发者需要手写的新校验/新状态分支数量趋近于 0（复用既有快照抽象 + 状态机），而非线性增长。**

原则：

1. **单一对账入口**：任何「操作前确认物理身份」的动作，收敛到一个受测抽象，禁止在编排层调用点手搓比较。
2. **不变量集中声明**：把「不会双跑、不错接管、停止未确认不起新实例、卷归属正确」显式建模，用可执行手段验证。
3. **能外包就外包**：凡是集群标准原语（ownerReferences/finalizers/SSA/ReclaimPolicy）本来就能保证的，交还给平台；自研只留「多租户 + 跨副本 + 强恢复语义」才需要的部分。
4. **错误即诊断契约**：每个拒绝/失败都带稳定错误码 + operation_id + 期望身份 vs 实际身份。
5. **兼容永不放宽**：保留现有迁移纪律，把 SQL 层已验证的版本化治理延伸到 JSON 载荷层。
6. **护栏优于约定**：能用 lint/CI/类型系统拦住的，不靠人肉 review。

## 5. 分阶段演进方案

每个阶段独立可交付、可回退，不要求一次性完成。落地某阶段前，先按其拆 `specs/` 下的 spec/plan/tasks。阶段状态按文首「现状核对基线」核定。

### Phase 0：建立度量与护栏基线（1–2 周，低风险）— **未开始**

先量化问题，否则后续无法判断「是否变好」。

- **对账点普查**：以本文 §1/§3 的实测计数为起点（编排层身份判断 24 处、`userapp_builder/` 的 `ok_or_else` 53 处、`capture_builder_volume_witness` 14 个非测试调用点跨 6 文件、`verify_workload_stable` 7 个调用点全在 `k8s_builder_control/ops.rs`），扩成全量「对账逻辑地图」清单并落进脚本，使 §7 的 grep 门禁有可对比基线。
- **错误原因盘点**：枚举 builder 相关的 `Error::Conflict` / `Error::ConfigurationError` 字符串，归类为「业务不安全 / 环境抖动 / 配置错误」三类，并标出哪些已被 `error_diagnostic.rs` 塌缩成泛化码。
- **CI 护栏**：
  - 实测 `await_holding_lock = "deny"` 是否覆盖 `dashmap::mapref` guard；未覆盖则补自定义检查（R6）。
  - 把 `make verify-kani` 纳入 CI gate（`tools/verify_kani.py` 已有白名单与逐证明超时；`.github/workflows/` 目前 grep 不到 kani，R2 起点）。
- 验收：产出对账地图 + 错误原因分类表；两条护栏在 CI 生效且不误报。

### Phase 1：快照抽象贯穿横向与纵向（核心，2–4 周）— **未开始，目标已重述**

初版目标是「从零建 `WorkloadWitness` 抽象」；核对后应改为**把已有的收敛形态贯穿到横向与纵向，并合并碎片化的 evidence 类型**。目标：消灭 R1。

- 在 `container-runtime-api` 定义一个**组合式已验证快照**类型，把现有分散 evidence 收敛为其字段：

  ```
  经对账的物理快照 = { workload: AppResourceIdentity(已校验 uid/version/归属),
                     pod: BuilderPodIdentity(已校验 ownerRef),
                     volumes: Vec<AppResourceIdentity>(已校验存在/未删除/归属) }
  ```

  现有 `BuilderCreationEvidence` / `BuilderOrphanStopTarget` / `AppAdoptionTarget` / `BuilderRestartTemplate` / `UserAppComputeStartTarget` 改为该快照的视图或从中派生，而不是各自独立返回。对账失败返回**结构化原因**（见 Phase 3），而非自由字符串。

  这三个字段的类型**都已存在**，本步是组合而非新造（`volumes` 复用同一 `AppResourceIdentity`）：`AppResourceIdentity`(`shared_types/src/app_resource_deletion.rs:34`，含 `kind`/`name`/`uid`/`resource_version`)、`BuilderPodIdentity`(`shared_types/src/userapp/builder_control.rs:40`，含 `name`/`uid`/`resource_version`)；`BuilderControlTarget`(:76) 本身已是 `workload: Option<AppResourceIdentity>` + `pod: Option<BuilderPodIdentity>` 的组合，缺的是「已校验」这一层类型区分与 `volumes` 的并入。注意 `AppResourceIdentity` 目前不携带删除态/归属信息，「未删除、归属正确」是采集时的断言而非快照字段——这正是要补的部分。
- **横向合并**：`capture_builder_volume_witness` 的 Docker（`docker_builder_restart.rs:697`）与 K8s（`k8s_builder_control/ops.rs:56`）两份实现，收敛为该快照抽象的两个后端实现，共用同一套校验步骤与结构化失败原因；`verify_workload_stable` 这种「单份 `pub(super)` helper 被复用 7 处」的形态是参照标准。
- 让「快照是否授权下一步写操作」由**类型**表达（例如区分只读观察快照与已授权变更快照），而不是靠调用点注释与 review。`capture_builder_orphan_stop` / `inspect_builder_orphan_stop` / `capture_bound_builder_orphan_stop` 这一族已经体现了这个区分，应推广。
- **纵向下沉**：编排层改调统一入口——`rcoder-engine/src/userapp_builder/{compute_control.rs,adoption.rs,dev_locator.rs,creation/observe.rs,dev_cleanup.rs,control.rs,app_adoption.rs,creation/registration.rs,compute_control/stopped_template.rs,auto_repair.rs}` 中手写的 uid/resource_version 判断全部下沉到快照抽象内部；`docker_manager/src/runtime/{k8s_builder_restart.rs,k8s_app_adoption.rs,docker_builder_control.rs}` 同步。
- 新快照类型同样要求 Docker 与 K8s 双后端实现并保持对称（现状 `capture_builder_compute_volumes` 已如此：`docker_runtime/agent_runtime.rs:387`、`kubernetes_runtime/agent_runtime.rs:296`）。
- 验收：
  - Phase 0 对账地图里的调用点已切到统一入口；`grep` 不到 `userapp_builder/` 下散落的手写 uid/version 判断，也不再有第二份 `capture_builder_volume_witness` 实现。
  - 补**能在切换前暴露差异的反例测试**（AGENTS.md 测试要求）：至少覆盖「同名重建（uid 变）」「采集中途删除」「多副本」「PVC 正在删除」。
  - `cargo nextest run -p docker_manager --no-fail-fast --all-features` + K8s `make remote-k8s-verify SUITE=userapp` 全绿。

### Phase 2：把编排层收到已有状态机契约之后（3–6 周）— **契约层已完成，编排层未开始**

目标：消灭 R2。**注意本阶段不是「建模状态机」——那已经完成**：`shared_types/src/userapp/compute_control.rs` 已有 `ComputeControlState`/`ComputeControlStage`/`ComputeControlAction`/`ComputeProgressPolicyError` 与纯校验 `validate_compute_progress_transition()`，`rcoder-storage/.../common/compute_execution.rs:128` 在持久化边界强制执行，且 Kani 已证明完整有限矩阵。app/operation 层同理（`domain.rs` 纯转换 + `kani_progress_claim_proofs` / `kani_scope_conflict_proofs`）。

本阶段的实际工作是**扩大契约覆盖面并让编排层复用它**：

- 盘点 `rcoder-engine/src/userapp_builder/compute_control.rs`（2260 行，引用契约类型 27 处）中「决定下一步做什么」的分支，判断哪些应当由 `validate_compute_progress_transition` 或新的纯决策函数给出，而不是在编排层就地判断；把 R1 的 24 处身份判断从阶段推进逻辑里解耦（依赖 Phase 1 的快照抽象）。
- 以 `rcoder-storage/src/userapp_lifecycle/domain.rs` 与 `compute_control.rs` 的纯函数为基座，把恢复/排空/自愈路径（`userapp_builder/{recovery,compute_drain_recovery,auto_repair.rs,shutdown_gate.rs}`、`idle_builder_retirement/`）的准入条件也纳入同一套纯转换，使它们可被穷举。
- 关键不变量清单见 §6（I1–I6），其中 I1/I3/I5 的验证必须覆盖 `runtime-supervisor`（owner 存活、历史停止判定、legacy drain）。
- 验证手段（复用现有基建，非新建）：
  - **性质测试**：对已有的纯校验/转换函数（`validate_compute_progress_transition`，以及 `domain.rs` 的 `admission`/`advance`/`validate_active`/`slot_matches_operation`/`operation_owns_any_slot`）做穷举/随机事件序列，断言 I1–I6 恒成立。状态枚举已带 `#[cfg_attr(kani, derive(kani::Arbitrary))]`，新增证明无需另建模。
  - **Kani**：已有 3 个 UserApp 生命周期证明（progress claim、scope conflict、compute progress policy）。本阶段把它们扩到恢复/排空准入，并**接进 CI gate**（Phase 0 的前置项）。
  - **状态机时序集成测试**：覆盖取消、崩溃、owner 强杀后恢复（AGENTS.md 验收项），复用 `tests-e2e` 的 `docker_lifecycle_crash.rs` / `native_lifecycle_crash.rs` / `host_k8s_lifecycle.rs`。
- 验收：不变量清单进 CI；编排层的隐式分支逐步迁到契约之后（以 Phase 0 对账地图与契约引用计数为度量）；性质测试能在故意引入的反例（如允许 None workload 继续、允许跳过 `Stopped` 直接 `Starting`）上失败。

### Phase 3：错误原因带码化 + 身份对账入诊断（2–3 周，可与 Phase 1/2 并行）— **大部分已完成，剩余缺口明确**

已交付：稳定错误码表（80 个 `ERR_*`）、`ErrorDetail`、`error_diagnostic.rs` 的类型→码映射、`ERR_OPERATION_OUTCOME_UNKNOWN` 与只读/变更上下文区分、`PgCommandEvidence`（NotDispatched / DefinitivelyRejected / OutcomeUnknown）、凭据脱敏、`operation_id` 贯穿、`docs/runtime-error-contract.md` 契约文档。

剩余缺口（本阶段的实际工作）：

- 引入**带码内部原因 enum**，替换产生端的字符串区分，至少覆盖：
  `WorkloadMissing` / `IdentityDrift` / `ResourceVersionChanged` / `VolumeDeleting` / `OwnershipMismatch` / `EnvTimeout` / `ReconcileRequired`。首要目标是 `verify_workload_stable` 的 `Result<(), String>` 与 `Error::Conflict(String)` / `Error::ConfigurationError(String)` 的 builder 相关子集，使 `error_diagnostic.rs` 不再把它们塌缩成泛化 `ERR_CONFLICT`，`K8sError`/`DockerError` 的 "Unclassified" 占比可度量地下降。
- `ErrorDetail` 补**期望身份 vs 实际身份**字段（当前只有 `reason_code`/`stage`/`detail`/`hint`/`retryable`/`task_id`/`service_id`），使「同名重建导致 uid 变化」这类拒绝可自解释。
- 打通 **OpenAPI（utoipa）+ telemetry（`rcoder-telemetry`）**：文档注册错误码，指标暴露各类拒绝计数，使「环境故障」与「业务失败」可分开统计。
- 把 receipt/journal 统一成**可查询审计时间线**：一次操作从受理→排队→执行→终态→对账的完整轨迹（存储基础已在 `userapp_lifecycle/common/`，缺的是统一查询口径）。
- 验收：新错误码进 `shared_types_i18n`；运维能对任一 operation_id 拉出完整时间线；指标上环境故障与业务失败可分开；`Conflict(String)` 在 builder 路径的计数相对 Phase 0 基线可度量地下降。

### Phase 4：自研面积审计 + 标准原语外包（持续，机会主义推进）— **未开始**

目标：遏制 R5，让自研只保留平台给不了的部分。

- 对每处自研对账判断，问一句：**「这是集群标准原语本来就能保证的吗？」** 若是，评估交还：
  - 级联/孤儿清理 → `ownerReferences` + finalizers；
  - 写冲突/并发覆盖 → Server-Side Apply + `resourceVersion` 重试；
  - 卷回收意图 → StorageClass / ReclaimPolicy / CSI，减少应用层逐 PVC 判回收。
- 明确**必须自研**的边界并文档化：多租户身份绑定、跨副本恢复语义、operation 身份贯穿、receipt 台账、owner 强杀后的恢复判定。
- 判据：自研代码行数在能力增长时**不随之线性上升**（用 Phase 0 的对账地图定期复查）。
- 验收：每次外包都有对应的平台机制验证（K8s 场景），且不变量测试（Phase 2）仍全绿；不能外包的部分有明确书面理由。

### Phase 5：JSON 载荷层的版本化治理（2 周 + 持续，针对 R4）— **SQL 层已完成**

已交付：`rcoder-storage/src/db/schema.rs` 的 `rcoder_schema_migrations` 台账（component/version/name/checksum/schema_fingerprint/applied_at_us，主键 (component,version)）、`migration_ddl()` 强制加法式迁移与「改基线表需显式迁移」纪律、迁移历史缺口检测。载荷侧也已有自觉的字段级兼容处理：`shared_types/src/userapp/` 下 7 个文件共 14 处 `#[serde(deny_unknown_fields)]`，`BuilderControlTarget` 手写 `Deserialize`(`builder_control.rs:97`) 用内层 `Wire` 结构精确控制放宽边界，旧字段以 `#[serde(default)]` + "Missing in old checkpoints" 注释标注。

剩余缺口：

- 给 `BuilderControlTarget` / receipt / `BuilderRestartTemplate` 等 **serde 载荷**加版本字段并建版本化 schema，CI 里跑**迁移测试矩阵**（旧 N-2 / N-1 → 当前），而非人肉判断。仓库目前没有对应的旧数据 fixture，需要先落地一批（可参考 `runtime-supervisor/src/resident/legacy_application_tests.rs`、`app-cli/src/control/legacy_management.rs` 的 legacy 兼容测试形态）。
- **`deny_unknown_fields` 是双刃**：它保证不静默吞掉无关输入，但也意味着新版本写入的字段会让旧二进制解析失败。14 处使用点需要逐个确认「谁是读者、是否可能读到更新版本写的数据」，并在矩阵里覆盖双向（旧读新 / 新读旧），而不是只测升级方向。
- 保持现有纪律：保留无关字段与历史 receipt，不用清库或放宽解析代替兼容处理。
- 验收：JSON 载荷迁移矩阵进 CI；用真实旧版本数据验证升级路径（含 K8s 默认化差异、随机 token、环境变量顺序等正常差异）；双向兼容均有断言。

## 6. 关键不变量清单（形式化验证锚点）

| ID | 不变量 | 现有实现锚点 | 验证方式 |
|---|---|---|---|
| I1 | 同一 app 任一时刻至多一个 owner/业务实例 | `UserAppActiveOperationRecords` 三槽位（Application/Dev/Prod）、`operation_lease.rs`、`runtime-supervisor` owner 存活判定 | 状态机性质测试 + 已有 Kani `kani_scope_conflict_proofs` + K8s 多副本场景 |
| I2 | workload 缺失时拒绝一切卷存证/危险写 | `k8s_builder_control/ops.rs:101` 的 `Error::Conflict("Builder volume workload missing")` fail-fast 守卫、`absent.rs` 中 8 处 `workload.is_some()/is_none()` 前置判断、`verify_workload_stable`(`helpers.rs:174`)、`capture_builder_volume_witness`(`ops.rs:56` / `docker_builder_restart.rs:697`) | 单测（反例）+ Kani；注意该守卫目前以自由字符串表达，Phase 3 应给它带码原因 |
| I3 | 停止未确认前不启动新实例 | `ComputeControlStage` 的 `DrainingPrevious→Stopping→Stopped→Starting` 顺序由 `validate_compute_progress_transition()` 集中校验并在 `compute_execution.rs:128` 强制；`runtime-supervisor/src/monitor/historical_stop.rs`、`legacy_drain.rs`；`shared_types/src/userapp/compute_control/stop_observation.rs` | 状态机时序集成测试 + `compute_stop_observation_tests.rs` + 已有 Kani `kani_progress_policy_proofs` |
| I4 | 已发副作用结果未知时保持写保护至对账完成 | `fence_builder_compute_write`(`runtime_trait.rs:340`)、`runtime_mutation_outcome_unknown`(`error_diagnostic.rs:64`)、`ERR_OPERATION_OUTCOME_UNKNOWN`、`PgCommandEvidence`、`common/ops/wake_fences.rs` | 故障注入测试（`transaction_fault_tests.rs`、`pg_commit_reply_tests.rs`、`wake_fence_tests.rs`） |
| I5 | 恢复验证真实进程与服务，非仅改状态字段 | `userapp_builder/{recovery,compute_drain_recovery,auto_repair.rs}`、`common/recovery_scan_tests.rs`、`shared_types/src/userapp/recovery_scan.rs` | 强杀后恢复 E2E（`make remote-k8s-verify SUITE=userapp`、`tests-e2e` 的 `*_lifecycle_crash.rs`） |
| I6 | 卷身份归属正确且未在删除中才可操作 | `AppResourceIdentity`、`app_resource_deletion.rs` witness 比较、`lifecycle_discovery.rs:72 discover_volume_identities`、`k8s_pvc/` | 快照抽象单测 + 真实 PVC 场景 |

## 7. 度量与护栏（CI Gates）

- **护栏（防回归）**：
  - DashMap guard 跨 await 检查（Phase 0；先实测 `await_holding_lock = "deny"` 的覆盖面）；
  - 散落手写 uid/version 校验的 `grep` 断言（Phase 0 记基线：编排层 24 处；Phase 1 后应为 0），以及 `capture_builder_volume_witness` 的实现份数（基线 2 份 → 1 份抽象 + 双后端实现）；
  - `make verify-kani` 进 CI（Phase 0/2）；
  - JSON 载荷迁移测试矩阵（Phase 5）。
- **度量（看趋势）**：
  - 对账逻辑调用点数量（应随 Phase 1 下降并稳定）；
  - 编排层对 compute 状态机契约的复用度：`rcoder-engine` 中契约类型/校验函数的引用数（基线 27 处集中于 `compute_control.rs`）应上升，就地阶段判断分支应下降（Phase 2）；
  - `Conflict(String)` / `ConfigurationError(String)` 在 builder 路径的计数，以及 `error_diagnostic.rs` 落到 `ERR_CONTAINER_ERROR`（Unclassified）的比例（应随 Phase 3 下降）；
  - 每类错误码的发生频率（区分环境 vs 业务）；
  - 新增能力所需新校验/新分支数（北极星，应趋近 0）；
  - UserApp 恢复类 E2E 通过率与耗时。

## 8. 风险与回退

| 风险 | 缓解 |
|---|---|
| 抽象过度，反而更难读 | Phase 1 先在一条路径试点（如 `dev_locator.rs` 或 adoption），验证收益再推广；快照类型优先「组合现有 evidence」而非新造概念 |
| 状态机迁移引入回归 | 迁移期新旧并存，用 Phase 2 性质测试兜底；每个不变量先补反例再改 |
| 外包给平台原语导致行为变化 | 每次外包配 K8s 场景验证；不变量测试全绿才合入 |
| 阶段过长阻塞业务 | 每阶段独立可交付；Phase 0/4 风险低可先行，Phase 1/2 分路径推进；Phase 3/5 只剩明确缺口，可插空完成 |
| 形式化验证维护成本 | 只对**纯转换函数**上 Kani（无 I/O、易建模），不对整链路；沿用 `tools/verify_kani.py` 的白名单 + 逐证明超时，避免 CI 时长失控 |
| 现状描述再次过期 | 本文计数标注核对基线 commit；每轮大改后重新采集，不把旧计数当现状 |

## 9. 若只做三件事

资源有限时，按此优先级：

1. **Phase 1（快照抽象贯穿横向与纵向）**——直接消除 R1 的漏校验与错接管风险，收益最快、最实在；地基（trait 抽象 + `AppResourceIdentity` + `verify_workload_stable` 的收敛范例）已在，主要工作是合并 Docker/K8s 两份 witness 实现、下沉编排层 24 处手写比较、合并 evidence 类型族。
2. **Phase 2（编排层收到已有状态机契约之后 + CI gate）**——契约层状态机、集中转换校验与 3 个 Kani 证明**都已存在**，缺的是让 `compute_control.rs` 的编排分支复用它、把恢复/排空准入纳入同一套纯函数，以及把 `make verify-kani` 接进 CI。这是把「不会双跑/不错接管」从"契约层已证明"推进到"全链路都走契约"的关键一步。
3. **Phase 0（度量与护栏基线）**——成本最低，且是前两项的验收前提；没有基线数字，Phase 1/3/5 的「下降」无法被证明。

Phase 3 与 Phase 5 只剩明确缺口（内部原因带码化 + 身份入 `ErrorDetail`；JSON 载荷版本化 + 迁移矩阵），可作为低风险插空项推进。Phase 4 机会主义持续。

## 10. 相关文档

- [架构总览](overview.md) / [gRPC 内部通信](grpc.md)
- [UserApp 应用管理](../concepts/userapp.md) / [UserApp 子路径路由](../concepts/userapp-subpath-routing.md)
- [运行时错误与安全重试](../runtime-error-contract.md) —— Phase 3 的已交付契约
- [UserApp 迁移策略](../userapp-migration-policy.md)
- [UserApp dev owner 恢复](../userapp-dev-owner-recovery.md) / [UserApp 构建队列](../userapp-build-queue.md) / [UserApp 启动探针](../userapp-startup-probes.md) / [UserApp 就绪容器](../userapp-readiness-container.md) / [UserApp 停止后日志](../userapp-stopped-logs.md) / [UserApp 项目诊断](../userapp-project-diagnostics.md)
- `runtime-state-layout` crate（运行态状态根解析契约）
- AGENTS.md 第 3 节「UserApp 核心业务原则」「生命周期、取消与恢复」
