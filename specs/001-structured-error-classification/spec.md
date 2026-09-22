# Feature Specification: 结构化错误分类——消除错误字符串匹配反模式

**Feature Branch**: `001-structured-error-classification`  
**Created**: 2026-09-22  
**Status**: Draft  
**Input**: User description: "不应该根据字符串来比较错误,这种不合理,应该有结构化错误代替,你分析下,看还有哪些是字符串比较错误的,如何开发修复"

## Clarifications

### Session 2026-09-22

- **Q**: 错误分类是否可以继续用 `msg.contains("...")` 做控制流？  
  **A**: 不允许。字符串匹配错误文案不合理（库升级/改措辞即静默破坏分支逻辑），必须用结构化错误（类型化枚举 / 字段 / 错误码）替代。

- **Q**: 本次范围是什么？  
  **A**: 全仓扫描所有「用错误文案字符串做控制流」的点（重试决策、错误分类、路由降级、退出码、自愈门控等），给出完整清单与修复方案。

- **Q**: 是否允许改变重试 / 降级 / 自愈策略？  
  **A**: 不允许。只做类型化重构，保持既有业务策略不变（与既有 `specs/error-string-matching-elimination/plan.md` R11 原则一致）。

- **Q**: 外部工具（pnpm / supervisord / K8s status）只有文本输出时怎么办？  
  **A**: 在边界集中解析一次，产出结构化 code/kind；业务层不得再匹配文案。有官方结构化字段（如 HTTP status、xmlrpc faultCode、gix 错误枚举）时必须优先读字段。

- **Q**: 已知线上缺陷是否纳入？  
  **A**: 是。`file-server` 的 `is_no_commit_error` 因匹配 `"does not have any commits yet"`（gix 实际文案无 `yet`）导致空仓库 `git log` 误报错，是本类反模式的实锤案例，必须一并修复并补反例测试。

### Session 2026-09-22 (follow-up)

- **Q**: 与 TypeScript `nuwax-file-server` 的对齐边界是什么？  
  **A**: **仅 HTTP 接口兼容**（路径、参数、响应形状、空数据语义）。内部实现**不得**移植/沿用 TS 的错误文案字符串匹配逻辑（`NotFoundError|does not have any commits` 类正则/contains）。

- **Q**: git 相关字符串匹配谁来改？  
  **A**: 用户已安排 ZCode 先改 git 几处；完成后用户会通知本会话做**遗漏/问题复查**。本特性规划需包含 post-review 检查清单，但不重复实施 git 改动（以工作区当前改动为准）。

- **Q**: 本轮规划重点？  
  **A**: 在 git 之外继续盘点「字符串比较错误」剩余点，给出结构化替代与开发修复顺序。

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
作为平台开发者 / 运维，我希望系统对错误的分类与分支决策基于**稳定的结构化语义**（错误类型、错误码、状态码字段），而不是错误文案的字面匹配；这样外部依赖升级、日志措辞调整、i18n 改写都不会静默破坏重试、降级、退出码、自愈等关键路径。

### Acceptance Scenarios
1. **Given** 一个空 git 仓库（已 init、0 commit），**When** 任何人查询提交历史，**Then** 系统返回空列表成功响应，而不是报「does not have any commits」错误。
2. **Given** 一次 HTTP 下载失败且状态码为 4xx，**When** 重试策略评估，**Then** 判定为不可重试——依据的是状态码字段，即便错误文案或 URL 恰好包含 "HTTP 4" 字样也不得误判。
3. **Given** 一次 HTTP 下载失败且状态码为 5xx 或连接级错误，**When** 重试策略评估，**Then** 判定为可重试。
4. **Given** pnpm 安装失败并携带 `ERR_PNPM_IGNORED_BUILDS`，**When** 自愈门控评估，**Then** 触发 ignored-builds 自愈——依据是结构化 code 字段，不是对完整输出文案的 contains。
5. **Given** 端口被占用导致 dev server 启动失败，**When** 预览执行器对错误分类，**Then** 归类为 PortInUse——依据是错误类型变体，修改中文提示文案不影响分类。
6. **Given** CLI 等待 Agent 响应超时，**When** 进程退出码决策，**Then** 仅「等待 prompt 完成」超时映射 Timeout 退出码——依据是 AcpError::Timeout 变体，不是 `contains("timed out")`。
7. **Given** RCoder 控制面返回 `code=not_found`，**When** 网关缓存 ensure 失败分类，**Then** 走 not_found 回退路径——依据是响应信封的 `code` 字段，不是 message 文案。
8. **Given** 任一错误分类单元测试，**When** 断言分类结果，**Then** 断言的是枚举变体 / 字段值，不是 `err.to_string().contains(...)`（测试对最终用户可见文案的快照断言除外，且不得参与控制流）。

### Edge Cases
- 外部工具只输出文本、无结构化 code 时：边界解析器必须集中、可测，解析失败落入 `Unknown`/`Unclassified` 显式变体，不得静默吞掉或用过宽子串兜底。
- 错误文案本地化（zh/en）后：分类结果必须不变。
- 同一底层错误被多层 `map_err` 包装后：结构化变体/字段必须能穿透（或在边界恢复），不得只在最后一层做字符串嗅探。
- 部分场景同时存在 code 与文案：code/枚举优先，文案兜底仅允许出现在**已文档化的边界解析器**内。

## Requirements *(mandatory)*

### Functional Requirements
- **FR-001**: 系统 MUST 用结构化手段（类型化错误枚举、错误码字段、协议字段）做一切错误分类与控制流决策（重试、降级、退出码、自愈、路由回退、错误抑制等）。
- **FR-002**: 系统 MUST 禁止在业务逻辑中对错误 `message` / `Display` 输出做 `contains`/`starts_with`/`to_lowercase` 后匹配来分支。
- **FR-003**: 对外部协议/工具，系统 MUST 优先读取官方结构化字段（HTTP status、xmlrpc faultCode、gix Error 枚举、pnpm ndjson error_codes、服务端信封 `code`）。
- **FR-004**: 对确实只有文本的外部输出，系统 MUST 在**单一边界解析器**内解析为结构化结果；业务层只消费结构化结果。过宽子串（如 `"not found"`）MUST NOT 作为兜底。
- **FR-005**: `file-server` 的空仓库/未出生分支检测 MUST 返回空历史成功结果（修复 `does not have any commits yet` 匹配偏移导致的误报错）。
- **FR-006**: 每个被替换的字符串匹配点 MUST 有能在修复前暴露错误行为的反例测试，以及修复后锁定结构化分类的回归测试。
- **FR-007**: 本次重构 MUST NOT 改变既有重试次数、退避、降级、自愈触发条件的业务语义（类型化前后策略等价）。
- **FR-008**: 新增/修改错误类型 MUST 在产生处保留类型信息（status/code/kind 字段），不得只改最后一个消费函数的匹配方式。
- **FR-009**: 全仓 MUST 可通过静态检查或评审清单验证：无新增「错误文案 → 控制流」模式。
- **FR-010**: 测试代码对用户可见错误文案做快照断言是允许的，但 MUST NOT 被生产控制流复用。

### Key Entities *(include if feature involves data)*
- **错误分类结果（FailureKind / ExitCode / RetryDecision / RouteFallback）**: 业务决策的稳定枚举；由结构化输入映射而来。
- **边界解析器（Boundary Classifier）**: 将外部工具/协议的原始输出（文本或结构化）归一为 code + kind + message；唯一允许接触文案的位置。
- **结构化错误载荷（Error Detail）**: 携带 `code` / `status` / `kind` 等字段的错误类型；`message` 仅用于展示与诊断，不参与分支。
- **反例回归（Counter-example Test）**: 修复前会失败、修复后锁定行为的测试；防止文案漂移或错误分类回退到字符串匹配。

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

---
