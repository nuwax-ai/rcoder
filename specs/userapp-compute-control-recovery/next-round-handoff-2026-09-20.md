# 下一轮开发交接（2026-09-20 晚，审计版）

## 0. 本文是什么、不是什么

本文是**第三方审计视角的现状快照**，用于把开发交给新的接手 agent。它**不替代**同目录
`remaining-development-handoff-2026-09-20.md`（下称《前一版交接》）；那份文档定义了需求边界和
R1–R6 的任务定义，本文只做三件事：

1. 记录审计时刻的真实基线（提交、编译、fmt 实测结果及命令）。
2. 对 R1–R6 逐项给出**当前代码中的定点证据**，区分「已核实」与「仅推断、需复核」。
3. 补充审计中新发现、前一版交接没有写到的问题。

**可信度边界（必须先读）：**

- 本文结论来自**定点阅读与 grep，不是全量代码审计**。写明 `file:line` 的都是审计时刻亲自打开确认过的；
  没写证据的按「待复核」对待。
- 代码在持续变化，`file:line` 会漂移。接手后**先重新 grep 关键符号**再下判断，不要拿本文行号当事实。
- 本文**没有运行任何测试**，也没有跑 clippy、没有跑 docker_manager 默认（非 kubernetes）feature、
  没有任何部署/E2E。凡本文没有明确写「已实跑并给出退出码」的，都不能当作已验证。
- `tasks.md`、`verification.md` 是**追加式历史**。顶部 T2–T9 的勾选状态严重滞后于实现，
  不能只读开头判断现状；同样也不能因为 verification 里写了某条就认为已部署验收。
- 如果本文的某条判断与你实际读到的代码冲突，**以代码为准**，并在 `verification.md` 里记录勘误，
  不要默默按本文执行。

## 1. 基线

- 仓库：`/Users/soddy/Documents/git-workspace/rcoder`，分支 `feature-userapp`。
- 工作树在审计时刻**干净**（`git status --short` 为空）。
- 本地有 **2 个未推送提交**（`origin/feature-userapp` 落后 2）：
  - `a571a0ea` file-server normalProject 定位/agent-store 锚点分流（4 文件）。
  - `604fd2b6` 本轮 compute-control-recovery 全部成果，**139 文件 +16036 / −690**。
- ⚠️ `604fd2b6` 的 commit message 是 `chore(dependencies): update Cargo.lock and add rustix dependency`，
  与实际内容严重不符。尚未 push，**建议在 push 前 amend 为 feat 语义**；push 之后再改成本变高。
  是否 amend 由用户决定，接手 agent 不要擅自 push 或改写历史。

### 1.1 审计时实跑的检查（唯一可引用的编译证据）

| 命令 | 退出码 |
|---|---|
| `CARGO_TARGET_DIR=target-check CARGO_BUILD_JOBS=4 cargo check -p rcoder -p app_manager --all-features` | 0 |
| `CARGO_TARGET_DIR=target-appcli CARGO_BUILD_JOBS=4 cargo check --manifest-path crates/app-cli/Cargo.toml --all-features` | 0 |
| `cargo fmt --all -- --check` | 0 |
| `cargo fmt --manifest-path crates/app-cli/Cargo.toml -- --check` | **1（失败）** |

- 上一轮提到的「严格 lint 路径限定错误」在当前提交里已不复现（前两条退出 0 即证据）。
- 并行 Cargo 使用了独立 `CARGO_TARGET_DIR`，没有与默认 `target/` 争用；接手继续沿用这个约定。
- **未跑**：clippy（根与 app-cli）、nextest、`docker_manager` 默认 feature、任何 Compose/K8s E2E。

## 2. 待修的两处「push 前小事」

### F1. app-cli fmt 违规已进提交（确定，可立即修）

`cargo fmt --manifest-path crates/app-cli/Cargo.toml -- --check` 退出 1，唯一违规点：
`crates/app-cli/src/server.rs:2988` 附近的 `fail_preparation(state, format!("persist unchanged artifact: {error:#}")).await;`
需要拆成多行（rustfmt 期望形态见 `/tmp/fmt-appcli.log`，该日志可能已被清理，可重跑生成）。

根 workspace fmt 干净、app-cli 不干净，说明上一轮只跑了根 fmt。**app-cli 是被根 workspace
排除的独立项目，fmt/clippy/test 都必须单独跑**（见 AGENTS.md 第 5 节）。

### F2. commit message 名不符实

见 1 节。属于用户决策项，**不要自行 rebase/amend/push**。

## 3. R1–R6 当前状态

标记含义：🔴 缺口明确且已定位证据；🟡 部分完成；✅ 本轮审计范围内未发现缺口（不等于已验收）。

### R1 Restart 缺控制器/容器时的恢复 —— 🔴 三条路径只闭环了一条

**已核实：**

- 归档/重建这对能力**只有 K8s dev 有实现**：`crates/docker_manager/src/runtime/kubernetes_runtime.rs`
  中的 `archive_builder_restart` / `restore_builder_restart`。
- trait 默认实现即「无能力」：`crates/container-runtime-api/src/runtime_trait.rs` 的
  `archive_builder_restart` 默认返回 `Ok(None)`，`restore_builder_restart` 默认返回
  `ConfigurationError("Builder controller restoration is unsupported")`。
- **Docker dev 路径断在取归档处**：`crates/rcoder/src/userapp_builder/compute_control.rs`
  的 Restart 执行段，缩容后若 workload UID 变化（容器消失即属此列）会去读
  `builder_restart_template`，Docker 侧从未写入该字段，于是在
  `.context("Original restart template is unavailable")` 处失败，没有任何重建来源。
- **prod 路径完全没有归档概念**：同文件 prod 分支只做 `capture_app_mutation_target` +
  `old.resource.uid == fresh.resource.uid` 断言；恢复侧 `recover_confirmed` 里存在
  `anyhow!("Production compute is absent during recovery")` 的硬失败。
- **请求进入时控制器已消失**仍是纯拒绝：`crates/docker_manager/src/runtime/k8s_builder_control.rs`
  的 `"Cannot start an absent captured builder"`。

**开发要求（继承前一版交接，不得放宽）：** 从可信持久部署/运行配置和卷绑定恢复模板；来源不足时返回
可查询、可继续的恢复阶段，**不能猜默认镜像、不能按名称选卷**；先证明旧计算实例退出再用原卷启动；
覆盖 RBD；归档不得含明文密码。**不能只把 absent 拒绝分支删掉当作"修好了"。**

### R2 创建/取消的回执前崩溃窗口 —— 🔴 dev 已覆盖，prod 创建链没有回执

**已核实：**

- dev builder 侧有完整持久回执三件套：`builder_creation_receipt.rs`、`k8s_creation_receipt.rs`、
  `docker_compute_receipt.rs`（共约 630 行）。K8s 用 immutable ConfigMap，Docker 用
  `<userapp workspace root>/.app-operation-receipts/<app_id>/<family>-<action>-<operation_id>.json`。
- **prod 应用创建的复合写（PVC → Service → Deployment → start）没有对应回执模块**：
  `crates/docker_manager/src/runtime/k8s_app_create.rs` 里只有注释提到
  "Only successful create receipts authorize compensation" 和
  "The API committed, but the client never receives its receipt"，没有持久化实现。

**待复核：** Docker 侧 prod 创建是否有等价窗口、`k8s_agent_create.rs` 的 receipt 覆盖到哪一步，
本次未逐行确认，接手请自行核对后再决定实施边界。

### R3 无标记历史资源 / 孤儿 / 归并 —— 🔴 只有报错，prod 没有接管入口

**已核实：**

- `crates/docker_manager/src/runtime/lifecycle_discovery.rs` 的 `include()`：资源缺
  `rcoder.io/lifecycle-id` 标签时直接
  `Conflict("Existing resource has no lifecycle identity; physical adoption is required")`；
  同一 scope 出现多个 owner、生命周期冲突时也只是 `Conflict` 报错。
- 但**显式物理接管只有 dev builder 一条链**：`crates/rcoder/src/userapp_builder/adoption.rs`
  暴露 `POST /api/v1/userapp/{app_id}/builder/adopt`，runtime trait 只有
  `capture_builder_adoption`（Docker/K8s 各有实现），**没有 prod 对等接口**。
- 结论：上面那句报错把调用方指向一个 prod 并不存在的能力。

**开发要求：** 提交前实时核对控制器/Pod/PVC 的 UID、挂载关系与管理归属；唯一可核验时才原子登记；
停止状态保留；多生命周期或删除墓碑**不得盲目合并**；失败新根只有确认未产生有效新资源后才能恢复旧身份。

### R4 app-cli 崩溃后的凭据补交与 hot_execution —— 🟡 本轮推进最大，成功侧仍未闭环

**已核实（已完成部分）：**

- credentials-only hold 的消费**绑定 operation_id**，由 legacy `/v1/deploy` 受理与
  `/v1/runtime/operations` dispatch 共用；受理/执行失败按原 ID 恢复保护位。
- `GET /v1/runtime/recovery` 提供只读恢复证据（`crates/app-cli/src/api/runtime.rs`），
  **它是查询，不是恢复入口**，不要当成写通道。
- **hot 失败侧恢复已接通**：`crates/app_manager/src/lifecycle/recovery.rs` 的
  `reconcile_hot_failure` —— 校验原租约身份 → 按 checkpoint 里的物理身份 exec 读 owner 的
  `/v1/deploy/status` → 仅当 `phase == Failed` 时以完整记录 CAS 收束为 Failed 并释放原租约。
- hot_execution 提交前已持久化物理见证（Pod UID / container ID / deployment generation /
  release_id / receipt_protocol），见 `crates/app_manager/src/lifecycle/deploy_control.rs`。

**已核实（仍缺）：**

- `reconcile_hot_failure` 在 owner 返回的 `phase != Failed` 时直接 `return Ok(None)`，
  随后落到 `retry_control_operation` 的通用拒绝分支
  （`"Operation cannot be replayed automatically ... manual reconciliation required"`）。
  即**"owner 其实已成功、只是平台侧响应丢失"这一支仍然锁死等人工**，而这是可判定的。
- `crates/app_manager/src/service/control.rs` 里的 `hot_execution.phase = "converging"`
  配置收敛阶段没有对应的终结/恢复链。

**边界提醒（用户已确认，不得偏移）：** 改 PG 密码**立即作用于当前 prod 的 PG**，不为改密拉新 Pod、
不做 source-seal、不恢复默认密码；已完成迁移不重跑，未知迁移保留证据；**明文凭据落盘方案已被撤回，
不要重新引入**。

### R5 旧版本 wake 锁 —— 🔴 本轮未动

**已核实：** `crates/rcoder-storage/src/userapp_lifecycle/common/ops.rs` 中
`finalize_observed_wake` 的全记录 CAS 要求 checkpoint 含
`start_write_acknowledged == true`，否则 `VersionConflict`。现场旧版本记录没有该字段，
因此**旧 wake 锁仍然无法通过任何现有入口收束**。

R5 依赖 R2 的旧写核验能力，**实施顺序应排在 R2 之后**。禁止按租约超时/业务 Failed 自动释放，
禁止手工标成功。

### R6 接口与文档对齐 —— 🟡 RCoder 侧基本到位，文档反向落后于代码

**已核实：**

- `pod_compute_operation`、`pod_compute_recover` 已注册进 utoipa
  （`crates/rcoder/src/router_docs/api_doc.rs`）。
- 但 `java-compute-control.md` 的「当前支持以下恢复」清单**只列 K8s 分支**，并写着
  "没有匹配回执的旧版本操作、**其他后端**或其他未知阶段仍要求进一步核验"；
  而 verification.md 记录且代码中已存在 Docker dev/prod 的 stop 回执恢复、Restart/stopped 续行、
  启动确认回执。**文档落后于实现，Java 按此文档接会漏掉 Docker 分支。**
- Java 项目本轮未修改，202/轮询/结构化错误透传/真实聊天链路均未联调。

## 4. 审计新发现（前一版交接未记录）

### N1. 回执对象没有任何 GC（🔴 运维风险，建议纳入 R2 一并处理）

- Docker：`docker_compute_receipt.rs` 只有 save/read/matches，**没有删除逻辑**；
  文件按 `operation_id` 命名，逐次操作在 workspace 目录里永久累积。
- K8s：`k8s_creation_receipt.rs` 把创建/取消回执写成 **immutable ConfigMap**
  （label `rcoder.io/resource-type=builder-creation-receipt`），**没有清理路径**，
  namespace 内对象数随操作次数无限增长。
- 对比：只有 restart archive Secret 有 GC（`cleanup_builder_restart_archive` +
  `mark_restart_archive_cleaned` 的 CAS 扫描），这套模式可以直接借鉴。
- ⚠️ 设计约束：GC **不能**在终态未确认前删除回执，否则会把崩溃恢复能力一起删掉；
  必须复用「确认终态 + 释放原租约 + CAS 标记已清理」的既有顺序。

### N2. tasks.md 顶部清单严重滞后

`tasks.md` 的 T2–T9 全部未勾选，与实际实现量级完全脱节。接手后**按实际完成情况更新**，
但**不要改写历史轮次的记录**（AGENTS.md 要求历史报告保留其原基线）。

## 5. 建议实施顺序与完成标准

顺序（依赖驱动，不是重要性排序）：

1. **F1** 修 app-cli fmt（几分钟，先做掉，避免后续每次检查都被它干扰）。
2. **R1 prod + Docker 的缺失控制器重建** —— app 129 那类事故的直接堵点，且 K8s dev 已有可照搬结构。
3. **R2 prod 创建阶段回执**（顺带 N1 的 GC 设计）。
4. **R5 旧 wake 锁**（吃 R2 的旧写核验成果）。
5. **R3 prod 物理接管入口**。
6. **R4 成功侧收束 + converging 终结链**。
7. **R6 文档同步**（`java-compute-control.md` 补 Docker 分支）。

每项的完成标准以《前一版交接》第 4 节为准，本文不重新定义。统一红线：

- 不得以「移除拒绝分支」「放宽身份校验」「延长超时」代替恢复实现。
- 不得用 watch/cache、端口可连、同名资源、租约超时单独证明旧写入结束。
- 编译通过 ≠ 功能完成；暂时拒绝请求 ≠ 止血完成 ≠ 开发完成。

## 6. 当前阶段的操作约束（沿用用户指示，未变更）

- **先补生产逻辑，可集中编译，暂不新增/运行测试**；测试阶段等用户明确指示。
- **不提交、不推送、不发布、不部署、不操作故障现场**（app129/app141 等）。
- 保留全部既有成果，不 reset 到 HEAD，不删除 dirty tree 中已有的测试文件。
- 根 workspace 与 app-cli **分别**做 fmt/编译检查，且使用独立 `CARGO_TARGET_DIR`。
- 每批改动追加记录到 `verification.md`（本轮源码基线、实际命令、退出码、证据路径、未完成项），
  不改写历史条目；发现本文判断有误时在同处写明勘误。
