# Phase 0 Research: Kani 高价值有界证明试点

**Feature**: `002-kani-high-value-proofs`  
**Date**: 2026-09-22  
**Input**: spec.md Clarifications Session 2026-09-22 + 用户要求「先找高价值使用 kani 看效果；是否引入 dev-dependencies 的 kani」

---

## Decision 1: 依赖策略——**不**引入普通 dev-dependencies 的 `kani`

**Decision**  
- **不要**在 `[dependencies]` 或普通 `[dev-dependencies]` 中加入 `kani` crate。  
- Proof harness 一律放在 `#[cfg(kani)]` 模块（或 `tests/` 内再套 `#[cfg(kani)]`）。  
- 可选：仅为 rust-analyzer 增加  
  ```toml
  [target.'cfg(kani_ra)'.dependencies]
  kani = { git = "https://github.com/model-checking/kani" }
  ```
- 工具链侧已安装 `kani-verifier`（`cargo-kani` / `kani` 0.68.0），这是**安装在机器上的验证器**，与 Cargo 依赖无关。

**Rationale**  
Kani 官方 `docs/src/usage.md` 明确：
1. `cargo kani` 编译目标 crate 时 **injects the `kani` crate** 并设置 `cfg(kani)`；  
2. 若把 `#[kani::proof]` 写在无门控代码里，`cargo build` 会因不认识 `kani` crate 而失败；  
3. 推荐与测试同构：`#[cfg(kani)] mod verification { ... }`，使普通构建**完全不受影响**；  
4. IDE 支持用 `cfg(kani_ra)` **条件依赖**，而不是无门控 dev-dependency。

**Alternatives considered**  
- **无门控 `dev-dependencies` 的 `kani`**：会让 `cargo test`/`cargo build --tests` 拉入 nightly 特性的 kani 库，污染日常构建与 IDE 以外的工具链；被拒。  
- **`[dependencies]` 正式依赖**：生产库引用验证 API，错误分层；被拒。  
- **独立 `verification` crate**：可避免门控，但 harness 离被证函数太远，逻辑漂移风险高；被拒（当前规模不需要）。  
- **`cfg(kani)` 就地模块**：**采纳**。

---

## Decision 2: 首批高价值目标 = 路径逃逸 / 注入转义 / 身份 Fail-closed

**Decision**  
按安全边界与 ∀ 输入量词收益排序，首批 3 主题（与 2026-09-22 深度分析 S/A 级一致）：

| 主题 | 模块 | 代表性质 |
|---|---|---|
| S1 路径防逃逸 | `file-server/src/path_safety.rs`（`ensure_within` / `safe_zip_entry`） | Ok ⇒ 落在 base 内；Zip-Slip 形态必拒 |
| S2 转义闭合 | `shared_types/src/pg_utils.rs`（`pg_shell_quote` / `pg_quote_ident` / `pg_escape_literal`） | 引号不可逃逸为第二条命令/语句 |
| A1/A2 身份 Fail-closed | `app_resource_deletion.rs`、`app_cli_deploy.rs`、`userapp/lifecycle.rs::validate_identity` | 五元组任一不匹配 ⇒ Err；validate 与 validate_success 互斥 |

**Rationale**  
这些点是「错一次 = 穿越/注入/删错资源」的硬边界；现有单测/fuzz 未覆盖 ∀ 输入；且全部是纯同步函数，Kani 技术上可入模。

**Alternatives considered**  
- 先上 `version_util`/`quantity`（已有 fuzz）：收益是证明升级，但安全面小于路径/注入/身份；降为第二批。  
- 直接上 `archive_links` FS 图：需要 stubbing + 图模型，pilot 成本高；推迟。  
- 上 async/并发：Kani 不支持 `.await`/数据竞争；非目标。

---

## Decision 3: Pilot 效果（本机实测，2026-09-22）

**Decision**  
记录真实 `cargo kani` 行为作为试点「效果」证据与 harness 设计约束。

**环境**  
- `kani` / `cargo-kani` 0.68.0（standalone + cargo plugin）  
- CBMC 6.11.0  
- macOS aarch64；pilot crate `/tmp/kani-pilot`（独立最小 crate，不污染 rcoder）

**实测结果（模型 = ensure_within 的固定缓冲词法包含性）**

| Harness | 结果 | 耗时 | 含义 |
|---|---|---|---|
| `proof_dot_segment_is_noop` | **VERIFICATION: SUCCESSFUL** | ~86.8s | `./seg` 与 `seg` 等价——性质成立 |
| `proof_escape_must_reject` | **FAILED**（unwinding assertion loop 0） | ~31s | 展开界限不足，**未证明**；非性质反例 |
| `proof_ok_implies_within_base` | **FAILED**（unwinding assertion loop 0 @ harness 的 `for` 迭代） | ~32–36s | 同上：`for b in base.iter().chain(rel.iter())` 循环需更大 unwind 或改为无迭代 assume |
| 早期 `String` 模型 | 大量 **UNDETERMINED** + unwind 失败 | ~0.6s/harness | 堆/`String`/`fmt` 展开爆炸；应用定长 `[u8; N]` |

**关键经验（写入 harness 规范）**  
1. **UNWINDING FAILURE ≠ 性质为假**，必须调 `--unwind` / 重写循环；**禁止** `--no-unwinding-assertions` 掩盖。  
2. 输入模型优先 **定长字节数组 + `kani::assume`**，避免 `String`/`format!`/`Vec` 深展开。  
3. harness 自身的 `for` 迭代也算循环，会计入 unwind。  
4. 单 harness 数十秒可接受；批处理并行或按主题拆包。

**Rationale**  
用户要求「看下效果」——用真实 SUMMARY 而非纸面推演。结论：链路可用，harness 工程质量决定成败。

**Alternatives considered**  
- 只写文档不跑：无法回答「效果」；被拒。  
- 在 rcoder workspace 内跑：会拖慢且污染 target；用 `/tmp/kani-pilot` 隔离；**采纳**（实现阶段再进 workspace）。

---

## Decision 4: 与现有质量门禁的关系

**Decision**  
- `make test` / `cargo nextest`：日常主路径，**不含** Kani。  
- 新增 `make verify-kani`（实现阶段）：只跑 `#[cfg(kani)]` harness。  
- Compose/K8s E2E 与 Loom：继续覆盖 Q02–Q12 并发/取消/恢复。  
- 证明失败 → `--concrete-playback inplace` 生成 nextest 反例（符合 AGENTS.md「修复前能红」）。

**Rationale**  
模型检查是证明层，不是测试层；合并进 nextest 会引入分钟级状态爆炸与 nightly 工具链耦合。

---

## Decision 5: 工具链与 CI

**Decision**  
- 开发机：已安装 kani 0.68.0（用户已装）；文档写明 `cargo install --locked kani-verifier && cargo kani setup`。  
- CI（后续）：独立 job，cache `~/.kani`；失败阻断合并（仅针对已拥有 harness 的模块）。  
- 版本钉扎：记录 `kani --version` 与 CBMC 版本进证据，避免「我这儿过了」。

**Alternatives considered**  
- 不进 CI：证明易腐；被拒（实现阶段可后置）。  
- 全 workspace 每次 PR 全跑：成本高；按文件路径过滤只跑受影响 harness。

---

## 风险与缓解

| 风险 | 缓解 |
|---|---|
| 状态爆炸 / 证明超时 | 小输入、定长缓冲、分 harness、主题拆分 |
| UNWINDING 被误当通过 | 规范：UNWINDING=未证明；CI 视为失败 |
| UNDETERMINED 被误当通过 | SUMMARY 须为 `VERIFICATION:- SUCCESSFUL` 且 failed=0 |
| harness 与实现漂移 | harness 调用真实 `pub fn`，不复制算法（pilot 的字节模型仅用于效果展示；实现阶段必须调 `ensure_within` 本尊） |
| 误改业务语义 | FR-007；实现 diff 只允许 `#[cfg(kani)]` 模块与 make/文档 |
