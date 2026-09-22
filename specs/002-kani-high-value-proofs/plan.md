# Implementation Plan: Kani 高价值有界证明试点

**Branch**: `002-kani-high-value-proofs` | **Date**: 2026-09-22 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/Users/soddy/Documents/git-workspace/rcoder/specs/002-kani-high-value-proofs/spec.md`

> **路径说明**：`.specify/scripts/bash/setup-plan.sh --json` 在当前 git 分支 `001-structured-error-classification` 上返回了错误特性目录（错误字符串匹配）。本计划按用户实际请求（Kani 试点）落在 `specs/002-kani-high-value-proofs/`，不覆盖 001 的 research/tasks/review 产物。

## Execution Flow (/plan command scope)
```
1. Load feature spec from Input path
   → If not found: ERROR "No feature spec at {path}"
2. Fill Technical Context (scan for NEEDS CLARIFICATION)
   → Detect Project Type from file system structure or context
   → Set Structure Decision based on project type
3. Fill the Constitution Check section based on the content of the constitution document.
4. Evaluate Constitution Check section below
   → If violations exist: Document in Complexity Tracking
   → If no justification possible: ERROR "Simplify approach first"
   → Update Progress Tracking: Initial Constitution Check
5. Execute Phase 0 → research.md
   → If NEEDS CLARIFICATION remain: ERROR "Resolve unknowns"
6. Execute Phase 1 → contracts, data-model.md, quickstart.md, agent-specific template file
7. Re-evaluate Constitution Check section
   → If new violations: Refactor design, return to Phase 1
   → Update Progress Tracking: Post-Design Constitution Check
8. Plan Phase 2 → Describe task generation approach (DO NOT create tasks.md)
9. STOP - Ready for /tasks command
```

**IMPORTANT**: The /plan command STOPS at step 8. Phases 2-4 are executed by other commands.

## Summary
在 rcoder 的**纯同步安全/身份内核**上引入 Kani 有界证明试点：路径防逃逸、SQL/Shell 注入转义、删除/部署证据身份 Fail-closed。回答依赖问题：**不需要**引入普通 `dev-dependencies` 的 `kani` crate；`cargo kani` 验证构建会自动注入 `kani` crate，harness 一律 `#[cfg(kani)]` 门控，可选 `cfg(kani_ra)` 条件依赖服务 rust-analyzer。本机 pilot 已跑通 Kani 0.68.0 + CBMC 6.11.0，出现 SUCCESS 与 UNWINDING 两类真实结果，证明链路可用且需要认真设计展开界限。

## Technical Context
**Language/Version**: Rust 2024 Edition（MSRV 1.85+）；Kani 0.68.0 / CBMC 6.11.0（已安装于本机）  
**Primary Dependencies**: 既有纯模块（`shared_types`、`file-server::path_safety`、`app_manager::lifecycle::start`）；**不**把 `kani` 写入 `[dependencies]` / 普通 `[dev-dependencies]`  
**Storage**: N/A（纯函数证明，无持久化）  
**Testing**: 既有 `cargo nextest`；新增独立 `cargo kani` 门禁（`make verify-kani`）；证明失败经 concrete playback 回灌 nextest 反例  
**Target Platform**: macOS/Linux 开发机与 CI（Kani 官方支持 Linux/Mac）  
**Project Type**: Cargo workspace（多 crate）；harness 落在被证 crate 内 `#[cfg(kani)]` 模块  
**Performance Goals**: 单 harness 证明时间目标 < 60s；整包试点 < 10min；禁止拖慢 `make test`  
**Constraints**: 生产代码零 `unsafe`、零 `unwrap/expect`；Fail Fast；SOLID；不改业务语义；并发/外部系统非目标  
**Scale/Scope**: 首批约 8–12 个 harness、3 个主题（路径/注入/身份）；不覆盖 async 与 Q02–Q12 时序

**用户提供的技术上下文（本轮）**:
- 先找高价值点使用 Kani，看效果
- 明确回答：是否需要引入 dev-dependencies 的 kani 依赖 → **否（见 Research Decision 1）**

## Constitution Check
*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

`.specify/memory/constitution.md` 仍为占位模板；以项目 `AGENTS.md` 红线作为本特性约束源：

| Gate | 结果 | 说明 |
|---|---|---|
| SOLID / 职责单一 | PASS | 仅添加验证 harness 与 Make/文档，不混入业务模块逻辑 |
| Fail Fast | PASS | 校验函数保持尽早 `Err`；证明针对「拒绝必须发生」性质 |
| 禁止生产 `unsafe` | PASS | harness 无 unsafe；不放宽 workspace lint |
| 禁止生产 `unwrap/expect` | PASS | harness 非生产路径；若进 `tests/` 需 `cfg(kani)` 仍禁止 unwrap |
| 契约集中 `shared_types` | PASS | 身份/校验谓词已在 `shared_types`，harness 就地挂接 |
| 不改业务语义 | PASS | FR-007 明确非目标 |
| 测试有效性（反例先行） | PASS | FR-008 要求 concrete playback 反例 |

**Initial Constitution Check: PASS**  
**Post-Design Constitution Check: PASS**

## Project Structure

### Documentation (this feature)
```
/Users/soddy/Documents/git-workspace/rcoder/specs/002-kani-high-value-proofs/
├── spec.md              # 需求与澄清
├── plan.md              # 本文件
├── research.md          # Phase 0：依赖策略、pilot 效果、harness 设计
├── data-model.md        # Phase 1：Harness/Property/Evidence 模型
├── quickstart.md        # Phase 1：如何跑证明
├── contracts/           # Phase 1：性质契约
│   ├── path-containment.md
│   ├── quote-escaping.md
│   └── identity-fail-closed.md
└── tasks.md             # Phase 2（/tasks 生成，本阶段不写）
```

### Source Code (repository root)
```
/Users/soddy/Documents/git-workspace/rcoder/
├── crates/shared_types/src/**          # 身份/校验/pg_utils/version_util（加 #[cfg(kani)] 模块）
├── crates/file-server/src/path_safety.rs
├── crates/app_manager/src/lifecycle/start.rs
├── make/verify-kani.mk                 # 新增 verify-kani 目标（实现阶段）
└── Makefile                            # 挂接 verify-kani（实现阶段）
```

**Structure Decision**: 验证代码不独立成 crate，而是 **紧贴被证函数的 `#[cfg(kani)]` 模块**（官方推荐），避免复制逻辑导致证明对象漂移；跨 crate 性质用 `contracts/` 文档对齐。不引入第二个 workspace、不引入 kani 为运行时依赖。

## Phase 0: Outline & Research

详见 [research.md](./research.md)。要点：

1. **依赖策略（回答用户问题）**：不需要普通 `dev-dependencies` 的 `kani`；`cargo kani` 注入 crate；`#[cfg(kani)]` 门控；可选 `cfg(kani_ra)` 给 IDE。
2. **Pilot 效果（本机实测）**：Kani 0.68.0 + CBMC 6.11.0 可用；固定缓冲模型下 `proof_dot_segment_is_noop` **SUCCESSFUL**（~87s）；其余 harness 出现 **unwinding assertion** 失败——需按循环上界设计 `--unwind`，禁止关闭展开断言冒充通过；堆/`String`/格式化路径大量 `UNDETERMINED` → 证明模型应收缩到定长缓冲/纯字节逻辑。
3. **首批目标**：S 路径与转义、A 身份 Fail-closed（与 2026-09-22 深度分析一致）。

**NEEDS CLARIFICATION 已全部在 spec Clarifications 解决。**

## Phase 1: Design & Contracts

- 实体与状态：见 [data-model.md](./data-model.md)
- 性质契约：
  - [contracts/path-containment.md](./contracts/path-containment.md)
  - [contracts/quote-escaping.md](./contracts/quote-escaping.md)
  - [contracts/identity-fail-closed.md](./contracts/identity-fail-closed.md)
- 操作指南：[quickstart.md](./quickstart.md)
- Agent 上下文：执行 `.specify/scripts/bash/update-agent-context.sh claude`

**设计决策**：
- Harness 形态：`#[cfg(kani)] mod kani_proofs` + `#[kani::proof]` + 小输入（`[u8; N]`）+ `kani::assume` 收紧文法。
- 大状态结构（`UserAppDeletionCheckpoint`）用 `kani::any()` 字段级非确定 + 必要 `assume`；避免展开 serde/JSON。
- FS 碰触函数（`archive_links::install`）本轮不做；先做纯词法/引用转义/结构体校验。

## Phase 2: Task Planning Approach
*2026-09-22 已生成 `tasks.md`（20 个有序任务）*

**Task Generation Strategy**:
- 加载 `.specify/templates/tasks-template.md`
- 每个 contract → harness 任务 [P]
- 每个 harness → 「先写性质、调 unwind、记录 SUMMARY 证据」
- 验证脚本/Make 目标、quickstart 验证任务
- **禁止**业务语义修改任务

**Ordering Strategy**:
1. 依赖/门控约定落地（Cargo 无污染）
2. 路径包含性 harness（pilot 已有雏形，收敛 unwind）
3. 转义闭合 harness
4. 身份 Fail-closed harness
5. `make verify-kani` + 证据归档

**Estimated Output**: 12–18 个有序任务（少于常规 25–30，因非功能特性、无 API/UI）

## Phase 3+: Future Implementation
**Phase 3**: /tasks 生成 tasks.md  
**Phase 4**: 实现 harness 与 verify-kani  
**Phase 5**: 本机跑通并归档 SUMMARY；失败则按反例规范回灌

## Complexity Tracking
*无宪法违规需豁免。*

| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|-------------------------------------|
| （无） | — | — |

## Progress Tracking
*This checklist is updated during execution flow*

**Phase Status**:
- [x] Phase 0: Research complete (/plan command)
- [x] Phase 1: Design complete (/plan command)
- [x] Phase 2: Task planning complete (/plan command)
- [x] Phase 3: Tasks generated (tasks.md, 2026-09-22)
- [ ] Phase 4: Implementation complete
- [ ] Phase 5: Validation passed

**Gate Status**:
- [x] Initial Constitution Check: PASS
- [x] Post-Design Constitution Check: PASS
- [x] All NEEDS CLARIFICATION resolved
- [x] Complexity deviations documented

---
*Based on AGENTS.md project redlines (constitution template unfilled) — see `/Users/soddy/Documents/git-workspace/rcoder/AGENTS.md`*
