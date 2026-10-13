# UserApp 运行时编排：长期演进方案

> 状态：草案，供评审。范围聚焦 UserApp/builder 的容器运行时编排层（`docker_manager` 的 builder control、`container-runtime-api` trait、`rcoder-engine/userapp_builder`、`rcoder-storage/userapp_lifecycle`）。
>
> 本文不是重构指令，而是一份「长期往哪走、分几步走、每步怎么验收」的方向性方案。任何一阶段落地前都应先按第 5 节拆出独立的 spec/plan/tasks。

## 1. 背景与问题

RCoder 的 UserApp 是一个**多租户、有状态、长生命周期**的计算单元：builder 以 StatefulSet + `volumeClaimTemplates`（K8s）或容器 + bind-mount/PVC（Docker）形式运行，数据卷必须保留，且业务上要求「**同一应用最终只有一套业务在服务**」。

为满足 AGENTS.md 里那组 UserApp 核心原则（重复 `run` 自动复用/恢复、单 owner、停止确认后再起新实例、已发副作用未知则对账、真实错误可修正重试），当前实现在运行时管理层**手写了大量身份对账与存证逻辑**。典型例子是 `capture_builder_volume_witness`：在危险操作前，逐层校验 workload 存在、kind 为 StatefulSet、`uid` 未变、`resource_version` 未变、`deletion_timestamp` 为空、再逐个 PVC 校验归属与删除态。

这套设计的「积木」全部是社区标准惯用法（StatefulSet 稳定卷、`ownerReferences` 归属、`resourceVersion` 乐观并发、`uid` 判代次）。问题不在概念，而在**工程形态**：

- 对账逻辑分散在 `adoption.rs` / `registration.rs` / `stopped_template.rs` / `k8s_builder_restart.rs` 等多处**各写一遍**，靠 `.ok_or_else(...)` 字符串守卫维持，易漂移、易漏校验、报错口径不一；
- 生命周期规则本质是状态机，但很大程度靠散落的分支 + 文档约定 + 人工 review 维持，缺少「非法转换在集中点被拒」的护栏；
- 错误以 `Error::Conflict` / `Error::ConfigurationError` + 自由字符串返回，运维难以区分「业务真不安全」与「环境抖动」；
- 持久化里存了 `BuilderControlTarget`、receipt、历史模板，长期背着「历史数据永不假解析」的兼容包袱。

**长期目标不是减少保证，而是让「新增一条能力/一条恢复路径的边际复杂度递减」**，而不是递增。否则平台越成功，运行时管理层越没人敢动。

## 2. 现状盘点（已有的良好底子）

演进方案必须建立在这些既有资产上，避免推倒重来：

| 资产 | 位置 | 价值 |
|---|---|---|
| 运行时 trait 抽象 | `container-runtime-api`（`AgentContainerRuntime` + builder 系列方法） | 已隔离 Docker/K8s 双后端，是收敛对账逻辑的天然挂载点 |
| **纯状态机转换（无 I/O）** | `rcoder-storage/src/userapp_lifecycle/domain.rs`（"Pure transitions... No database or runtime I/O"） | 转换逻辑已可被穷举/性质测试，是形式化验证的最佳起点 |
| 分离式 compute 编排 | `rcoder-engine/src/userapp_builder/compute_control.rs`（admission 不等槽、receipt 保留至结果确认） | 已实现「受理/执行/终态」解耦，符合 AGENTS.md 生命周期要求 |
| receipt/ledger 台账 | `rcoder-storage/src/userapp_lifecycle/common/*` | 已有物理回执、并发控制、恢复扫描的存储基础 |
| UID/version 双栅栏 | `docker_manager/src/runtime/k8s_builder_control/*` | 对账「积木」齐全，缺的是把它们收敛成可复用抽象 |
| 形式化基建 | `tools/verify_kani.py`、`make/verify-kani.mk`、`fuzz/` | 性质验证/fuzz 的脚手架已存在，可直接复用 |
| 迁移策略 | `docs/userapp-migration-policy.md` | 兼容性纪律已文档化 |

**结论**：底子比「纯手搓」要好。真正缺的是三件事——(a) 对账逻辑的统一抽象；(b) 生命周期不变量的集中化 + 可执行验证；(c) 错误与可观测性的可诊断契约。

## 3. 现状风险清单

| # | 风险 | 触发后果 | 严重度 |
|---|---|---|---|
| R1 | 对账校验多处重复、口径不一 | 新路径漏校验 → 错接管 / 双跑 | 高 |
| R2 | 生命周期状态机隐式、散落 | 补丁侵蚀规则，边界时序出错 | 高 |
| R3 | 错误靠字符串区分 | 运维无法归因，误判为业务失败 | 中 |
| R4 | 持久化格式无版本化迁移矩阵 | 升级时旧数据假解析/丢字段 | 高 |
| R5 | 自研面积持续膨胀 | 无法把「集群本可保证」的部分外包，维护成本线性上升 | 中 |
| R6 | 并发纪律靠 review（DashMap guard 跨 await、锁序） | 偶发死锁/竞态 | 中 |

## 4. 长期目标与设计原则

**北极星指标：新增一条 UserApp 能力时，开发者需要手写的新校验/新状态分支数量趋近于 0（复用既有 Witness + 状态机），而非线性增长。**

原则：

1. **单一对账入口**：任何「操作前确认物理身份」的动作，收敛到一个受测抽象，禁止在各调用点手搓。
2. **不变量集中声明**：把「不会双跑、不错接管、停止未确认不起新实例、卷归属正确」显式建模，用可执行手段验证。
3. **能外包就外包**：凡是集群标准原语（ownerReferences/finalizers/SSA/ReclaimPolicy）本来就能保证的，交还给平台；自研只留「多租户 + 跨副本 + 强恢复语义」才需要的部分。
4. **错误即诊断契约**：每个拒绝/失败都带稳定错误码 + operation_id + 期望身份 vs 实际身份。
5. **兼容永不放宽**：保留现有迁移纪律，加版本化 schema + 迁移测试矩阵。
6. **护栏优于约定**：能用 lint/CI/类型系统拦住的，不靠人肉 review。

## 5. 分阶段演进方案

每个阶段独立可交付、可回退，不要求一次性完成。落地某阶段前，先按其拆 `specs/` 下的 spec/plan/tasks。

### Phase 0：建立度量与护栏基线（1–2 周，低风险）

先量化问题，否则后续无法判断「是否变好」。

- **对账点普查**：用脚本统计所有 `capture_builder_compute_volumes` / `verify_workload_stable` / `ok_or_else(|| Error::Conflict(...))` 的调用点与字符串，形成「对账逻辑地图」清单。
- **错误码盘点**：枚举当前所有 builder 相关的 `Error::Conflict/ConfigurationError` 字符串，归类为「业务不安全 / 环境抖动 / 配置错误」三类。
- **CI 护栏**：
  - 加 clippy lint / 自定义检查：禁止在持 DashMap guard 的作用域内出现 `.await`（R6）。
  - 把 `make/verify-kani.mk` 纳入受影响 crate 的 CI gate（R2 起点）。
- 验收：产出对账地图 + 错误码分类表；护栏在 CI 生效且不误报。

### Phase 1：统一对账抽象 `WorkloadWitness`（核心，2–4 周）

目标：消灭 R1，让「操作前对账」只有一个受测入口。

- 在 `container-runtime-api` 层定义返回**已验证快照**的抽象，例如：

  ```
  经对账的物理快照 = { workload: AppResourceIdentity(已校验 uid/version/归属),
                     pod: BuilderPodIdentity(已校验 ownerRef),
                     volumes: Vec<AppResourceIdentity>(已校验存在/未删除/归属) }
  ```

  对账失败返回**结构化原因**（见 Phase 3），而非自由字符串。
- 把 `capture_builder_volume_witness` / `builder_volumes_from_current_view` / `verify_workload_stable` / `discover_volume_identities` 收敛为这个抽象的内部步骤；`adoption` / `registration` / `stopped_template` / `restart` 全部改调统一入口。
- Docker 与 K8s 各自实现同一抽象，保持双后端对称。
- 验收：
  - 对账地图里所有调用点已切到统一入口；`grep` 不到散落的手写 uid/version 校验。
  - 补**能在切换前暴露差异的反例测试**（AGENTS.md 测试要求）：至少覆盖「同名重建（uid 变）」「采集中途删除」「多副本」「PVC 正在删除」。
  - `cargo nextest run -p docker_manager --no-fail-fast --all-features` + K8s `remote-k8s-verify SUITE=userapp` 全绿。

### Phase 2：生命周期不变量的集中化 + 可执行验证（3–6 周）

目标：消灭 R2，让非法转换在集中点被拒。

- 以 `rcoder-storage/userapp_lifecycle/domain.rs` 的纯转换为基座，把 builder/app 生命周期**显式建模**为 `enum State` + `event` + `transition()`，明确每个转换的前置条件与「不变量」。
- 定义**关键不变量清单**（供形式化验证）：
  - I1：任一时刻同一 app 最多一个 owner（单业务实例）。
  - I2：`workload = None` 时，任何 volume witness / 危险写操作必须被拒（即本次讨论的 `Builder volume workload missing` 属于 fail-fast 守卫）。
  - I3：停止未被确认（进程/服务/目录切换无证据）前，不得启动新实例。
  - I4：已发副作用结果未知时，涉及该副作用的写保护保持开启，直到对账完成。
  - I5：恢复必须验证真实进程与服务，不能只改状态字段。
- 验证手段（复用现有基建）：
  - **性质测试**：对纯 `transition()` 做穷举/随机事件序列，断言 I1–I5 恒成立。
  - **Kani**：对关键转换函数做有界模型检查（`tools/verify_kani.py`）。
  - **状态机时序集成测试**：覆盖取消、崩溃、owner 强杀后恢复（AGENTS.md 验收项）。
- 验收：不变量清单进 CI；Phase 0 里发现的隐式分支逐步迁移到集中 `transition()`；性质测试能在故意引入的反例（如允许 None workload 继续）上失败。

### Phase 3：错误即诊断契约（2–3 周，可与 Phase 1/2 并行）

目标：消灭 R3，让每次拒绝可归因、可审计。

- 引入**带码错误类型**（enum），替换字符串区分，至少覆盖：
  `WorkloadMissing` / `IdentityDrift` / `ResourceVersionChanged` / `VolumeDeleting` / `OwnershipMismatch` / `EnvTimeout` / `ReconcileRequired`。
- 每个错误携带：期望身份 vs 实际身份、阶段、`operation_id`、是否可重试。
- 打通到 **OpenAPI（utoipa）+ telemetry（`rcoder-telemetry`）**：文档注册错误码，指标暴露各类拒绝计数。
- 把 receipt/journal 统一成**可查询审计时间线**：一次操作从受理→排队→执行→终态→对账的完整轨迹。
- 验收：错误码进 `shared_types_i18n`；运维能对任一 operation_id 拉出完整时间线；「环境故障」与「业务失败」在指标上可分开统计。

### Phase 4：自研面积审计 + 标准原语外包（持续，机会主义推进）

目标：遏制 R5，让自研只保留平台给不了的部分。

- 对每处自研对账判断，问一句：**「这是集群标准原语本来就能保证的吗？」** 若是，评估交还：
  - 级联/孤儿清理 → `ownerReferences` + finalizers；
  - 写冲突/并发覆盖 → Server-Side Apply + `resourceVersion` 重试；
  - 卷回收意图 → StorageClass / ReclaimPolicy / CSI，减少应用层逐 PVC 判回收。
- 明确**必须自研**的边界并文档化：多租户身份绑定、跨副本恢复语义、operation 身份贯穿、receipt 台账。
- 判据：自研代码行数在能力增长时**不随之线性上升**（用 Phase 0 的对账地图定期复查）。
- 验收：每次外包都有对应的平台机制验证（K8s 场景），且不变量测试（Phase 2）仍全绿；不能外包的部分有明确书面理由。

### Phase 5：持久化兼容的版本化治理（2 周 + 持续，针对 R4）

- 给 `BuilderControlTarget` / receipt / ledger 建**版本化 schema**，CI 里跑**迁移测试矩阵**（旧 N-2 / N-1 → 当前），而非人肉判断。
- 保持现有纪律：保留无关字段与历史 receipt，不用清库或放宽解析代替兼容处理；`#[serde(deny_unknown_fields)]` 的边界要显式覆盖。
- 验收：迁移矩阵进 CI；用真实旧版本数据验证升级路径（含 K8s 默认化差异、随机 token、环境变量顺序等正常差异）。

## 6. 关键不变量清单（形式化验证锚点）

| ID | 不变量 | 验证方式 |
|---|---|---|
| I1 | 同一 app 任一时刻至多一个 owner/业务实例 | 状态机性质测试 + K8s 多副本场景 |
| I2 | workload 缺失时拒绝一切卷存证/危险写 | 单测（反例）+ Kani |
| I3 | 停止未确认前不启动新实例 | 状态机时序集成测试 |
| I4 | 已发副作用结果未知时保持写保护至对账完成 | 故障注入测试 |
| I5 | 恢复验证真实进程与服务，非仅改状态字段 | 强杀后恢复 E2E（`SUITE=userapp`）|
| I6 | 卷身份归属正确且未在删除中才可操作 | Witness 抽象单测 + 真实 PVC 场景 |

## 7. 度量与护栏（CI Gates）

- **护栏（防回归）**：
  - DashMap guard 跨 await 检查（Phase 0）；
  - 散落手写 uid/version 校验的 `grep` 断言（Phase 1 后应为 0）；
  - 迁移测试矩阵（Phase 5）；
  - Kani 对关键转换（Phase 2）。
- **度量（看趋势）**：
  - 对账逻辑调用点数量（应随 Phase 1 下降并稳定）；
  - 每类错误码的发生频率（区分环境 vs 业务）；
  - 新增能力所需新校验/新分支数（北极星，应趋近 0）；
  - UserApp 恢复类 E2E 通过率与耗时。

## 8. 风险与回退

| 风险 | 缓解 |
|---|---|
| 抽象过度，反而更难读 | Phase 1 先在一条路径试点（如 adoption），验证收益再推广 |
| 状态机迁移引入回归 | 迁移期新旧并存，用 Phase 2 性质测试兜底；每个不变量先补反例再改 |
| 外包给平台原语导致行为变化 | 每次外包配 K8s 场景验证；不变量测试全绿才合入 |
| 阶段过长阻塞业务 | 每阶段独立可交付；Phase 0/3/5 风险低可先行，Phase 1/2 分路径推进 |
| 形式化验证维护成本 | 只对**纯转换函数**上 Kani（无 I/O、易建模），不对整链路 |

## 9. 若只做三件事

资源有限时，按此优先级：

1. **Phase 1（统一 Witness 抽象）**——直接消除 R1 的漏校验与错接管风险，收益最快、最实在。
2. **Phase 2（不变量集中化 + 性质验证）**——把「不会双跑/不错接管」从约定变成可机器证明，是长期不腐化的关键。
3. **Phase 3（错误诊断契约）**——让系统长期可运维、可归因，避免每次故障都靠读代码猜。

Phase 0/4/5 作为低风险的持续护栏与治理，机会主义推进。

## 10. 相关文档

- [架构总览](overview.md) / [gRPC 内部通信](grpc.md)
- [UserApp 应用管理](../concepts/userapp.md)
- [UserApp 迁移策略](../userapp-migration-policy.md)
- `runtime-state-layout` crate（运行态状态根解析契约）
- AGENTS.md 第 3 节「UserApp 核心业务原则」「生命周期、取消与恢复」
