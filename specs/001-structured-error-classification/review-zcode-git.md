# ZCode git 改造复查报告（e985badb0）— 待处理问题清单

**日期**: 2026-09-22  
**复查人**: MiMo（本会话）  
**基线**: `e985badb0 fix(file-server): git 读路径结构化解析 unborn/缺席 ref + 写路径去吞错`  
**验证**: `cargo nextest run -p file-server -E 'test(git)'` → **39 passed**  
**用途**: 交给 ZCode 继续开发；按优先级处理 G-A → G-B → G-C/G-D；G-E 为同特性剩余（可下一批）

---

## 0. 结论摘要

| 维度 | 结论 |
|------|------|
| 结构化替换文案匹配 | **达标** — `is_no_commit_error` 已删，`resolve_rev` 用 `is_unborn()` + 类型化 `NotFound` |
| 空仓库线上缺陷（app-169） | **已修** — unborn/缺 ref → 空列表/`Ok(None)`，反例测试在 |
| 与 TS 边界 | **达标** — 仅 HTTP 空数据契约对齐，未引入 TS 正则/contains |
| 写路径吞错 | **达标** — `has_any_commit` 幂等门 + 失败传播 |
| **能力回归** | **有问题** — 修订语法（`HEAD~1`）与缩写 OID 能力丢失（见 G-A） |
| **解析路径未收口** | **有问题** — `diff`/`ops`/`refs` 仍走 `rev_parse_single`（见 G-B） |
| 同特性剩余字符串匹配 | 3 处非 git（见 G-E） |

---

## 1. 已确认通过（无需再改）

1. `resolve_rev`（`read.rs:19-45`）：
   - `HEAD` → `repo.head().is_unborn()` 结构化判定
   - ref 名 → `find_reference` + `Error::NotFound` 类型匹配（gix `PartialNameRef` 支持短名 `main`，doctest 可证）
   - 未走 `rev_parse_single`（类型擦除）— 决策正确
2. `log_history` / `file_content_at_ref`：`Ok(None)`/空列表空数据契约，HTTP 形状不变
3. `is_no_commit_error` 及 `"does not have any commits yet"` 偏移已删除
4. `init_repo`：幂等门 `has_any_commit`（修粘性失败）+ commit 失败传播
5. 反例测试：unborn HEAD/分支、缺分支、完整 OID、OID 缺席、unborn file-content
6. git 模块生产代码无错误文案 `contains`/`to_lowercase` 分支

---

## 2. G-A 能力回归（优先）

### G-A1 【P0】修订语法丢失：`HEAD~1` / `HEAD^` / `main~2`

| 项 | 说明 |
|----|------|
| **现象** | `resolve_rev` 只识别：精确 `"HEAD"`、ref 短/全名、**完整 40 位** OID |
| **影响** | `file-content` 的 `ref`、`log` 的 `branch`、Java 契约明确写了 **`HEAD~1` 等**（agent-platform `GitController.fileContent`：「ref 取值：worktree/staged/HEAD/commit hash/**HEAD~1** 等」）。当前 `find_reference("HEAD~1")` → `NotFound` → `from_hex` 失败 → **静默 `Ok(None)`/空列表**，比报错更糟（Monaco 对比会拿空内容） |
| **旧能力** | `rev_parse_single` 支持 `HEAD~n` / `ref^` / `ref~n` |
| **修复建议** | `find_reference` NotFound 后增加修订解析通道（保留类型化）：gix `repo.rev_parse(spec)`（platform，支持 revspec）区分 `NotFound/Unborn` 与真错误；或手写 `~`/`^` 后缀剥离 + 递归 parent。**禁止**文案 contains |
| **验收** | 反例：2+ commit 仓库 `file_content_at_ref(repo, "HEAD~1", …)` 取到父提交内容；`log_history(..., Some("HEAD~1"))` 从父提交 walk；`HEAD~99` → 空/`Ok(None)` 而非 500 |

### G-A2 【P1】缩写 OID 丢失

| 项 | 说明 |
|----|------|
| **现象** | `ObjectId::from_hex(spec.as_bytes())` 只接受完整 hash；`rev_parse_single` 原先支持前缀展开 |
| **影响** | UI/用户传 7–12 位短 hash（版本对比常见）→ 静默空 |
| **修复建议** | 与 G-A1 同一解析通道；或 gix `repo.rev_parse` 的短 OID 前缀解析；歧义前缀应显式报错/空，不猜测 |
| **验收** | 短 hash 命中 → 正常返回；不存在短前缀 → 空数据；歧义 → 明确错误（勿静默取第一个） |

---

## 3. G-B 解析路径未收口（同一语义应共用 `resolve_rev`）

### G-B1 【P1】`diff.rs` `from`/`to` 仍用 `rev_parse_single`（约 67/81 行）

- 类型擦除错误，无法结构化区分「缺 ref」与「真失败」
- 语义与 `log_history` 不一致：log 缺 ref → 空列表；diff 缺 from → 500
- **建议**：迁到 `resolve_rev`（或 G-A1 增强版）；`from` 缺席 → 空 diff 或 400（与契约一致二选一，写进测试）；`commit` source 无 `from` 保持现校验
- **注意**：迁移时一并支持 `HEAD~1`（G-A1）

### G-B2 【P1】`ops.rs` `reset`/`checkout` 等 target 仍用 `rev_parse_single`（约 90/146/225 行）

- checkout/reset 的 target 同样可能是分支名/OID/`HEAD~1`
- **建议**：统一 `resolve_rev`；「缺席 target」应 validation/resource 错误（写操作不适用空列表契约），不要静默成功

### G-B3 【P1】`refs.rs` `create_branch` 的 `start_point` 仍用 `rev_parse_single`（约 28 行）

- 默认 `head_id()` 在 unborn 下报错（创建分支无起点可理解，可保留）
- 显式 `start_point=HEAD~1`/短 hash 需同 G-A1
- **建议**：共用增强版 `resolve_rev`；unborn 且未传 start_point → 明确错误文案/错误码（勿文案匹配）

### G-B4 【P2】`create_branch` 默认路径 `repo.head_id()`

- 与 `has_any_commit`/`resolve_rev` 风格不统一
- **建议**：`head.is_unborn()` → 带上下文的 validation（「需要 start_point 或已有提交」）

---

## 4. G-C 实现细节

### G-C1 【P2】`has_any_commit` 用 `repo.head_id().is_ok()`

```rust
fn has_any_commit(repo: &Repository) -> bool {
    repo.head_id().is_ok()
}
```

- 功能正确（unborn → Err → false）
- **建议**：改为 `matches!(repo.head(), Ok(h) if !h.is_unborn())`，与 `resolve_rev` 同一观测源，避免 `head_id` 其它失败被误判为「无提交」而触发 initial commit

### G-C2 【P2】`resolve_rev` 对非法 hex / 非 ref 名

- 一律 `Ok(None)` 可接受（缺席契约）
- 若 G-A1 引入 revspec 后，`HEAD~` 等半截语法应 400/空，保持可测

---

## 5. G-D 测试缺口（实现 G-A/G-B 时一并补）

| # | 用例 | 期望 |
|---|------|------|
| T-d1 | `file_content_at_ref(..., "HEAD~1", ...)` | 父提交内容（修复前应红） |
| T-d2 | `log_history(..., Some("HEAD~1"))` | 自父提交 first-parent walk |
| T-d3 | `log_history(..., Some(&short_oid))` | 短 hash 命中非空 |
| T-d4 | 有 commit 后 `branch=Some("main")` 短名 walk | 非空（锁 PartialNameRef） |
| T-d5 | `HEAD~99` / 歧义短前缀 | 空数据或明确错误（锁定一种） |
| T-d6 | diff `from` 缺席 | 与契约一致（空 diff 或 400） |
| T-d7 | reset target 缺席 | 明确错误，不静默 |

---

## 6. G-E 同特性剩余（非 git，建议下一批或同批）

字符串比较错误仍在（生产控制流）：

| 优先级 | 位置 | 问题 | 结构化替代 |
|--------|------|------|------------|
| P1 | `crates/file-server/src/service/pnpm/classify.rs` `classify_text` | 自由英文短语 `contains_any` | `ERR_PNPM_*` code 表驱动；无 code → `Unknown` |
| P1 | `crates/app_manager/src/utils.rs` `extract_reason` | 对 K8s **message** 扫 KNOWN | 优先 status **`reason`** 字段 |
| P2 | `crates/app-cli/src/xmlrpc.rs` `is_no_such_process` | `faultString` 含 BAD_NAME 兜底 | 仅 `faultCode==10` |

详细契约见 `contracts/error-classification.md`；任务见 `tasks.md` T006–T008。

---

## 7. 建议施工顺序（给 ZCode）

1. **G-A1** 修订语法 + **T-d1/T-d2**（先红后绿）— 止住静默空内容  
2. **G-A2** 短 OID + **T-d3/T-d5**  
3. **G-B1** diff 迁移 + **T-d6**（依赖 A1）  
4. **G-B2/G-B3** ops/refs 迁移 + **T-d7**  
5. **G-C1** + **T-d4** 补锁  
6. 可选同批：**G-E** 三处  
7. 收尾：`cargo nextest run -p file-server --no-fail-fast --all-features`，涉及 app-cli 时独立 workspace 三连（fmt/clippy/nextest）

---

## 8. 复查清单对照（research.md Decision 2）

| Checklist | 状态 |
|-----------|------|
| git 全模块无错误文案 contains | ✅ 生产代码通过（测试断言除外） |
| 空仓库 log 反例 | ✅ `log_history_on_unborn_repo_returns_empty` |
| 空分支 branch=main | ✅ 同上 |
| 真错误不被空列表吞掉 | ✅ 其它 gix 错误仍 `map_git_err` 传播 |
| file-content 空契约 | ✅ `Ok(None)` |
| HTTP 与 TS 兼容 | ✅ `commits,total,success,logId` |
| 无 TS 正则兜底 | ✅ |
| ensure_repo unborn→main 仍结构化 | ✅（write/init 路径） |
| fmt/clippy/nextest -p file-server | ✅ git 过滤 39 绿；全量见 G-E 前再跑 |
| **修订/短 OID 能力不回归** | ❌ **G-A1/G-A2** |
| **diff/ops/refs 共用结构化解析** | ❌ **G-B1–G-B3** |

---

**路径**  
- 本报告：`/Users/soddy/Documents/git-workspace/rcoder/specs/001-structured-error-classification/review-zcode-git.md`  
- 规格/计划：同目录 `spec.md` / `plan.md` / `research.md` / `tasks.md`
