# Tasks: Kani 高价值有界证明试点

**Input**: `/Users/soddy/Documents/git-workspace/rcoder/specs/002-kani-high-value-proofs/`
**Prerequisites**: plan.md, research.md, data-model.md, contracts/
**Coordination**: 本清单只添加 `#[cfg(kani)]` harness / Make 入口 / 证据归档；**禁止**修改业务语义（FR-007）。依赖策略：**不**引入普通 `dev-dependencies` 的 `kani`（FR-003）。

## Format: `[ID] [P?] Description`
- **[P]**: 可并行（不同文件、无依赖）
- 路径均为仓库相对路径；仓库根 `/Users/soddy/Documents/git-workspace/rcoder`

## Execution Flow (main)
```
1. Load plan.md from feature directory
2. Load optional design documents (data-model.md, contracts/, research.md)
3. Generate tasks: Setup → Proof harnesses (TDD-style properties first) → Verify gate → Evidence
4. Different files = [P]; same file = sequential
5. Number T001+；先性质后调参；UNWINDING ≠ 通过
```

---

## Phase 3.1: Setup（依赖与门控约定）

- [ ] T001 记录基线：`git status --short`；`kani --version`（期望 0.68.x + CBMC 6.x）写入 `specs/002-kani-high-value-proofs/evidence/00-baseline.md`
- [ ] T002 确认依赖策略落地：扫描 workspace `Cargo.toml`，**不得**出现无门控 `kani` 于 `[dependencies]` / `[dev-dependencies]`；仅允许注释说明或可选 `cfg(kani_ra)`（见 research Decision 1）
- [ ] T003 [P] 在 `make/verify-kani.mk` 草拟 `verify-kani` 目标（按 crate 串行 `cargo kani -p <crate>`，失败非零；**不**挂入 `make test`）；Makefile 仅 `include` 挂接

## Phase 3.2: 性质契约 harness（先写断言与输入域，再调 unwind）⚠️ 证明对象必须是真实 `pub fn`

### 路径包含性 — `crates/file-server/src/path_safety.rs`

- [ ] T004 [P] `#[cfg(kani)]` 模块 + `path_ok_implies_within`：P1 Containment（调 `ensure_within`）；输入 `base:[u8;8]` `rel:[u8;12]`；对应 `contracts/path-containment.md` P1
- [ ] T005 [P] `path_escape_must_reject`：P2 Escape rejection（调 `ensure_within`）；UNWINDING 先按循环上界设 `--unwind`，失败禁止 `--no-unwinding-assertions`
- [ ] T006 [P] `zip_entry_no_slip`：P3 Zip-slip（调 `safe_zip_entry`）；含 `../`、绝对路径、混合段
- [ ] T007 `skip_equiv_ensure`：P4 `safe_within_or_skip` ⇔ `ensure_within`（与 T004 同文件，顺序追加）

### 注入转义闭合 — `crates/shared_types/src/pg_utils.rs`

- [ ] T008 [P] `shell_quote_roundtrip`：P1 转义双射（调 `pg_shell_quote`）；输入允许 `'` `\\` `;` `$` 换行
- [ ] T009 [P] `quote_ident_closure` + `escape_literal_pairs`：P2/P3（调 `pg_quote_ident` / `pg_escape_literal`）
- [ ] T010 `pg_identifier_iff_whitelist`：P4 `validate_pg_identifier` 全称刻画（同文件顺序）

### 身份 Fail-closed — `crates/shared_types`（`userapp/lifecycle.rs` / `app_resource_deletion.rs` / `app_cli_deploy.rs`）

- [ ] T011 [P] `identity_binding`：`UserAppExecutionContext::validate_identity` ⇒ P1/P2（短 ASCII 域 + fingerprint 分层：长度/hex）
- [ ] T012 [P] `deletion_op_membership`：`UserAppDeletionCheckpoint::validate_operation` ⇒ P3（字段级 `kani::any()`，**避免** serde_json 展开）
- [ ] T013 [P] `hot_evidence_exclusive`：`HotDeploymentFailureEvidence::validate` 与 `validate_success` 互斥 ⇒ P4（JSON 字段可 stub 为手工构造 `UserAppOperationRecord`；stuck 则降级为纯比较辅助并注明）
- [ ] T014 `incomplete_resources_reject`：`validate_deletion_resources` ⇒ P5（与 T012 同文件顺序）

## Phase 3.3: 证明调参与证据（每条 harness 出 SUMMARY）

- [ ] T015 对 T004–T014 逐条 `cargo kani -p <crate> --harness <id>`：记录 SUMMARY/耗时到 `specs/002-kani-high-value-proofs/evidence/<harness>.md`；**Proved 仅当** `VERIFICATION:- SUCCESSFUL` 且 failed=0（data-model 不变量）
- [ ] T016 UNWINDING 失败项：上调 `#[kani::unwind]` / 重写 harness 循环（定长字节、避免 `for` 迭代/`String`/`fmt`）；仍失败则记 `Unwinding` 未完成，**不得**勾完成
- [ ] T017 出现 assertion FAILURE：`cargo kani --concrete-playback inplace` 生成 nextest 反例；按 AGENTS.md「修复前能红」——若为业务缺陷，**另开修复任务**，本特性不静默改语义

## Phase 3.4: 验证入口与文档

- [ ] T018 收敛 `make verify-kani`：聚合 T015 已 Proved 的 crate/harness；`make test` 路径确认不包含 kani（`cargo nextest run --workspace` 冒烟）
- [ ] T019 [P] 核对 `quickstart.md` 命令可复制执行；依赖问答与 research Decision 1 一致（无普通 dev-dependency）
- [ ] T020 [P] 更新 `specs/002-kani-high-value-proofs/evidence/summary.md`：版本、命令、Proved/Unwinding/Counterexample 计数、未覆盖非目标（async/Q02–Q12）

## Dependencies

- T001–T003 先于一切 harness
- 性质 harness T004–T014 彼此可按 [P] 并行；同文件对（T007←T004；T010←T008/T009；T014←T012）顺序
- T015 依赖对应 harness；T016/T017 依赖 T015 结果
- T018–T020 依赖本批 harness 进入终态（Proved 或显式未完成）

## Parallel Example

```text
# 同时启动路径/转义/身份三主题（不同文件）：
T004 path_ok_implies_within          → crates/file-server/src/path_safety.rs
T008 shell_quote_roundtrip           → crates/shared_types/src/pg_utils.rs
T011 identity_binding                → crates/shared_types/src/userapp/lifecycle.rs
T012 deletion_op_membership          → crates/shared_types/src/app_resource_deletion.rs
T013 hot_evidence_exclusive          → crates/shared_types/src/app_cli_deploy.rs
```

## 完成标准（本批）

1. 至少 S1（路径）+ S3（转义）+ A1（身份绑定）三类各 1 条 harness **Proved** 且证据齐全  
2. `make verify-kani` 可重复跑；`make test` 不受影响  
3. 无普通 `dev-dependencies` 的 `kani`  
4. 未 Proved 项在 `evidence/summary.md` 显式列出，禁止空勾  
5. 生产业务代码 diff 为空（仅 `#[cfg(kani)]` / make / specs/evidence）

## 非目标（禁止写入本批任务）

- async / 并发交错 / Loom（Q02–Q12）  
- `archive_links` FS 真实 symlink（待 stubbing 批次）  
- Docker/K8s/PG 外部契约 E2E 替代  

## Validation Checklist
*GATE: Checked before returning*

- [x] All contracts have harness tasks (path P1–P5, quote P1–P5, identity P1–P5 → T004–T014)
- [x] All Key Entities covered (Harness / Property / DependencyPolicy / EvidenceRecord → T002/T004–T015/T020)
- [x] TDD-style: properties before unwind-tuning evidence
- [x] Parallel markers only on different files
- [x] Dependency graph documented
