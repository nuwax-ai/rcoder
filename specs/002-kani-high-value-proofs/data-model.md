# Phase 1 Data Model: Kani 高价值有界证明试点

**Feature**: `002-kani-high-value-proofs`  
**Date**: 2026-09-22

## 实体总览

```text
DependencyPolicy ──约束──> ProofHarness ──陈述──> VerificationProperty
                                 │
                                 └──产出──> EvidenceRecord
```

---

## 1. DependencyPolicy（依赖策略）

| 字段 | 类型 | 约束 |
|---|---|---|
| `runtime_dep` | bool | 恒为 `false` |
| `dev_dep_unconditional` | bool | 恒为 `false`（本特性约定） |
| `ide_cfg_kani_ra_dep` | bool | 可选，默认 false |
| `harness_gate` | enum | 恒为 `CfgKani` |
| `toolchain_cli` | string | `kani-verifier`（机器级，非 Cargo 依赖） |

**校验规则**  
- 任何将 `kani` 写入 `[dependencies]` 或无门控 `[dev-dependencies]` 的变更 ⇒ 违反 FR-003。  
- 生产 `cargo build -p <crate>` 不得解析 `kani::` 路径。

**状态迁移**：无（静态策略）。

---

## 2. ProofHarness

| 字段 | 类型 | 约束 |
|---|---|---|
| `id` | string | 稳定名，如 `path_ok_implies_within` |
| `module_path` | path | 被证 crate 内 `#[cfg(kani)]` 模块 |
| `subject` | fn ref | **真实**被证 `pub fn`（禁止复制算法） |
| `property_id` | string | → `VerificationProperty.id` |
| `unwind` | u32 | 显式界限；UNWINDING 失败必须上调或重构循环 |
| `max_input` | map | 各 `kani::any` 输入长度上限（如 `[u8; 12]`） |
| `status` | enum | `Planned` \| `Proved` \| `Counterexample` \| `Unwinding` \| `Undetermined` |

**校验规则**  
- `subject` 必须是生产 `pub fn`（或 `pub(super)` 测试可见性仅限同 crate）。  
- `status=Proved` 仅当 SUMMARY 为 `VERIFICATION:- SUCCESSFUL` 且无 failed check。  
- `Unwinding` / `Undetermined` / `Counterexample` **均不得**记为完成。

**状态迁移**  
```text
Planned → (run cargo kani) → Proved | Counterexample | Unwinding | Undetermined
Counterexample → (fix + playback test) → Proved
Unwinding → (raise unwind / rewrite loop) → Proved | Counterexample
```

---

## 3. VerificationProperty

| 字段 | 类型 | 约束 |
|---|---|---|
| `id` | string | 如 `path.containment` |
| `theme` | enum | `PathContainment` \| `QuoteEscaping` \| `IdentityFailClosed` |
| `quantifier` | enum | 恒为 `ForAllInputs`（在 `max_input` 有界域内） |
| `assertion` | string | 人类可读性质（契约文档同文） |
| `contract_doc` | path | `contracts/*.md` |

**主题 → 被证函数（首批）**

| theme | functions |
|---|---|
| PathContainment | `file_server::path_safety::{ensure_within, ensure_within_path, safe_within_or_skip, safe_zip_entry}` |
| QuoteEscaping | `shared_types::pg_utils::{pg_shell_quote, pg_quote_ident, pg_escape_literal, validate_pg_identifier}` |
| IdentityFailClosed | `UserAppExecutionContext::validate_identity`；`UserAppDeletionCheckpoint::{validate, validate_operation}`；`HotDeploymentFailureEvidence::{validate, validate_success}` |

---

## 4. EvidenceRecord

| 字段 | 类型 | 约束 |
|---|---|---|
| `harness_id` | string | → ProofHarness.id |
| `kani_version` | string | 如 `0.68.0` |
| `cbmc_version` | string | 如 `6.11.0` |
| `summary_line` | string | 原始 `VERIFICATION:- …` / `SUMMARY` |
| `failed_checks` | u32 | 0 才允许 Proved |
| `unreachable_checks` | u32 | 记录；过高需检查 over-constraint |
| `undetermined_checks` | u32 | 记录；性质相关 UNDETERMINED 不得忽略 |
| `duration_ms` | u64 | 性能基线 |
| `recorded_at` | datetime | 归档时间 |

**校验规则**  
- 同一 `harness_id` + 源码基线（git sha）下证据可复现。  
- 历史报告不得改写为新一轮结果（对齐 AGENTS.md verification 纪律）。

---

## 5. 关系与不变量

1. `ProofHarness.property_id` 必须存在于 `VerificationProperty`。  
2. `EvidenceRecord.harness_id` 必须存在于 `ProofHarness`。  
3. **不变量**：`ProofHarness.status = Proved ⇒ last EvidenceRecord.failed_checks = 0 ∧ summary = SUCCESSFUL`。  
4. **不变量**：`DependencyPolicy.dev_dep_unconditional = false`（本特性范围）。  
5. **非目标实体**：并发交错、容器生命周期、网络超时——不在此数据模型。
