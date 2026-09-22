# Feature Specification: Kani 高价值有界证明试点

**Feature Branch**: `002-kani-high-value-proofs`
**Created**: 2026-09-22
**Status**: Draft
**Input**: User description: "我们先找高价值的,来使用kani,看下效果,另外需要引入 devdependencies 的kani依赖吗?"

## Clarifications

### Session 2026-09-22

- **Q**: 首批范围是全面铺开还是先打高价值点？  
  **A**: 只做**高价值试点**。首批限路径防逃逸、注入转义、身份 Fail-closed 三类纯同步内核；不覆盖 async/并发/外部系统。

- **Q**: 是否必须引入 `dev-dependencies` 的 `kani` crate？  
  **A**: **不是必须**。`cargo kani` 在验证构建中会自动注入 `kani` crate；官方推荐 harness 一律放在 `#[cfg(kani)]` 模块内，使 `cargo build` / `cargo test` / 发布产物完全不受影响。仅当需要 rust-analyzer 解析 `kani::` API 时，增加可选的 `[target.'cfg(kani_ra)'.dependencies] kani = ...`。**禁止**把 `kani` 写入普通 `[dependencies]` 或无门控的 `dev-dependencies` 导致生产/日常构建受影响。

- **Q**: 本次交付边界是什么？  
  **A**: Spec/Plan/任务规划 + 本机 `cargo kani` 效果证据（pilot）；**本阶段不改生产业务代码**。实现阶段再按 tasks 落地 `#[cfg(kani)]` harness 与 `make verify-kani`。

- **Q**: 与现有测试体系（nextest / fuzz / E2E）的关系？  
  **A**: **补充而非替代**。Kani 独立门禁目标（如 `make verify-kani`），不进 `make test` 主路径；证明失败用 concrete playback 反哺 nextest 反例。并发生命周期不变量继续由 E2E/Loom 承担。

- **Q**: 验收时是否要求全部目标证明通过？  
  **A**: 试点以「能跑通 `cargo kani`、给出 SUCCESS/FAILURE/UNWIND 证据、依赖策略落地」为准；个别 harness 需调 `--unwind` 属预期调试，不作为失败。禁止用关闭 unwinding assertions 的方式掩盖未展开循环。

---

## Execution Flow (main)
```
1. Parse user description from Input
   → If empty: ERROR "No feature description provided"
2. Extract key concepts from description
   → Identify: actors, actions, data, constraints
3. For each unclear aspect:
   → Mark with [NEEDS CLARIFICATION: specific question]
4. Fill User Scenarios & Testing section
   → If no clear user flow: ERROR "Cannot determine user scenarios"
5. Generate Functional Requirements
   → Each requirement must be testable
   → Mark ambiguous requirements
6. Identify Key Entities (if data involved)
7. Run Review Checklist
   → If any [NEEDS CLARIFICATION]: WARN "Spec has uncertainties"
   → If implementation details found: ERROR "Remove tech details"
8. Return: SUCCESS (spec ready for planning)
```

---

## ⚡ Quick Guidelines
- ✅ Focus on WHAT users need and WHY
- ❌ Avoid HOW to implement (no tech stack, APIs, code structure)
- 👥 Written for business stakeholders, not developers

---

## User Scenarios & Testing *(mandatory)*

### Primary User Story
作为平台维护者，我希望对**安全边界与 Fail-closed 身份核验**等纯逻辑内核做**有界穷尽证明**，而不仅是抽样测试；这样路径穿越、SQL/Shell 注入转义逃逸、删除/部署证据身份错配等事故面，在合并前就能被机器证明「不存在反例」。

### Acceptance Scenarios
1. **Given** 任意相对路径字符串（含 `..`、绝对路径、空段、`.`），**When** 工作区路径约束校验执行，**Then** 凡返回成功的路径必然落在选定根目录之下；任何逃逸形态必须被拒绝。
2. **Given** 任意自由文本口令/值，**When** 进入 Shell/SQL 字面量引用，**Then** 引号与转义后不得形成可逃逸的第二条命令/语句。
3. **Given** 删除/热部署证据与存储中的操作记录，**When** 身份核验执行，**Then** 五元组（app_id、lifecycle_id、operation_id、executor_id、request_fingerprint）任一不匹配必须拒绝，不存在伪造字段组合能通过。
4. **Given** 本机已安装 Kani，**When** 对试点 harness 运行 `cargo kani`，**Then** 产出可解读的 SUCCESS / FAILURE / UNWINDING 结果与耗时，且生产构建不引用 kani API。
5. **Given** 一次证明失败，**When** 使用 concrete playback，**Then** 可生成回归单测进入 nextest 体系（修复前能红、修复后锁绿）。

### Edge Cases
- 输入含 NUL、控制字符、非 UTF-8 字节：校验层必须拒绝或安全返回，不得 panic。
- Windows verbatim（`\\?\`）、UNC、盘符与 POSIX 混合：判定与规范化不得互相矛盾。
- 循环展开不足导致 UNWINDING 失败：必须记录并调整界限；禁止静默关闭展开断言冒充通过。
- 证明目标含堆分配/格式化时 UNDETERMINED 增多：需收缩为固定缓冲模型或 stub，不得把 UNDETERMINED 报成 SUCCESS。

## Requirements *(mandatory)*

### Functional Requirements
- **FR-001**: 系统 MUST 为高价值纯同步内核提供 Kani proof harness（路径防逃逸、注入转义、身份 Fail-closed）。
- **FR-002**: harness MUST 以 `#[cfg(kani)]` 门控；`cargo build`、`cargo test`、发布构建 MUST NOT 依赖 `kani` crate。
- **FR-003**: 验证依赖策略 MUST 符合 Clarifications：默认**不**引入普通 `dev-dependencies` 的 `kani`；仅允许可选 `cfg(kani_ra)` 目标依赖服务 IDE。
- **FR-004**: 提供独立验证入口（如 `make verify-kani` / `cargo kani` 聚合），MUST NOT 拖慢 `make test` 主路径。
- **FR-005**: 每个 harness MUST 有明确的性质陈述（∀ 输入量词）与展开界限；UNWINDING 失败 MUST 当作未证明处理。
- **FR-006**: 试点 MUST 产出可复现的效果记录（命令、退出状态、SUMMARY、耗时），证明本机 Kani 0.68 + CBMC 链路可用。
- **FR-007**: 本特性 MUST NOT 修改既有重试/降级/删除/部署业务语义；实现阶段仅添加验证 harness 与验证脚本。
- **FR-008**: 证明失败的反例 MUST 可通过 concrete playback 转为回归测试，并遵循「修复前能暴露错误」的反例规范。
- **FR-009**: 明确非目标：async/并发交错、Docker/K8s/PG 外部契约、Q02–Q12 时序不变量——继续由 E2E/Loom 承担。
- **FR-010**: 文档 MUST 回答「是否引入 dev-dependencies 的 kani」并给出可执行的 Cargo.toml/门控约定。

### Key Entities *(include if feature involves data)*
- **Proof Harness**: `#[cfg(kani)]` 门控的证明入口，绑定一条全称性质与 unwind 界限。
- **Verification Property**: 对纯函数的 ∀ 输入断言（包含性、转义闭合、身份一致）。
- **Dependency Policy**: kani crate 的引入策略（默认不引入生产/dev 硬依赖；可选 IDE 条件依赖）。
- **Evidence Record**: 一次 `cargo kani` 的 SUMMARY/状态/耗时，用于试点验收与回归基线。

---

## Review & Acceptance Checklist
*GATE: Automated checks run during main() execution*

### Content Quality
- [ ] No implementation details (languages, frameworks, APIs)
- [ ] Focused on user value and business needs
- [ ] Written for non-technical stakeholders
- [ ] All mandatory sections completed

### Requirement Completeness
- [ ] No [NEEDS CLARIFICATION] markers remain
- [ ] Requirements are testable and unambiguous
- [ ] Success criteria are measurable
- [ ] Scope is clearly bounded
- [ ] Dependencies and assumptions identified

---

## Execution Status
*Updated by main() during processing*

- [x] User description parsed
- [x] Key concepts extracted
- [x] Ambiguities marked (resolved in Clarifications Session 2026-09-22)
- [x] User scenarios defined
- [x] Requirements generated
- [x] Entities identified
- [x] Review checklist passed
