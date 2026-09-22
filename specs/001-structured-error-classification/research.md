# Phase 0 Research: 结构化错误分类——消除错误字符串匹配

**Feature**: `001-structured-error-classification`  
**Date**: 2026-09-22（follow-up 重扫）  
**Input**: `spec.md` Clarifications ×2 Session + 用户 follow-up：「git 的 http 接口兼容就行了,内部不应该用 typescript nuwax-file-server 的字符串匹配逻辑；再检查还有哪些字符串匹配错误」

## Decision 1: 与 TS 的对齐边界 = 仅 HTTP 契约

**Decision**  
Rust `file-server` 与 `nuwax-file-server`（TS）只保持 **HTTP 接口兼容**（路由、query/body、响应字段、空数据语义如 `commits: []`）。内部错误分类、重试、自愈**禁止**复制 TS 的文案匹配（`/NotFoundError|does not have any commits/i` 等）。内部一律 gix/类型化枚举/协议字段。

**Rationale**  
TS 的 catch-contains 是 isomorphic-git 无类型错误下的妥协；gix 提供 `Head::is_unborn()`、`Error::Unborn`、`reference::find::existing::Error::NotFound`。沿用文案匹配会把 TS 的脆弱性带进 Rust，且已造成 `"…yet"` 偏移线上缺陷。

**Alternatives considered**  
- 逐行翻译 TS gitService：会原样引入字符串嗅探。  
- 只对齐 HTTP 语义、内部完全自研：**采纳**。

## Decision 2: git 点位以 ZCode 工作区为准 + 复查清单

**Decision**  
git 字符串匹配修改由 ZCode 在本分支工作区进行（`crates/file-server/src/service/git/read.rs` 等，当前未提交）。本特性**不重复实施**，改为维护 **Post-Review Checklist**（用户通知后执行）。

**ZCode 当前工作区已见模式（需复查确认）**  
`resolve_rev()`：`HEAD` → `repo.head().is_unborn()` → `Ok(None)`；分支 → `find_reference` + `Error::NotFound` 结构化；OID 回退。`log_history` 对 `None` → `Ok(vec![])`。**已去掉** `is_no_commit_error` 文案 contains。

**Post-Review Checklist（等用户通知）**
- [ ] `git/read.rs` / `write.rs` / `diff` / `ops` / `refs` / `file-content` 无任何错误文案 contains/to_lowercase 分支
- [ ] 空仓库 `log_history` → `Ok(vec![])` 反例存在且绿
- [ ] 空分支 `branch=Some("main")` 同上
- [ ] ref 不存在 vs 真错误：后者仍 `Err`（不被空列表吞掉）
- [ ] `file-content`/`diff` 的 `Ok(None)` 空串契约不回归
- [ ] HTTP 响应形状与 TS 兼容（`commits,total,success,logId`）
- [ ] 无 TS 风格正则/`NotFoundError|…` 兜底被引入
- [ ] `ensure_repo` unborn HEAD→main 后仍走结构化 Unborn
- [ ] fmt/clippy/nextest -p file-server 通过

## Decision 3: 全仓剩余「字符串比较错误」清单（2026-09-22 follow-up 重扫）

**Decision**  
git 之外，生产控制流字符串匹配仅剩 3 处 + 边界解析器内部；其余为测试断言或非错误分类。

| 优先级 | 位置 | 现状 | 结构化替代 |
|--------|------|------|------------|
| **P1** | `crates/file-server/src/service/pnpm/classify.rs` `classify_text` | `contains_any` 扫 code+message+全文（etimedout/unauthorized/…） | `FailureKind` 由 `ERR_PNPM_*` **code 表驱动**；`extract_plain_error_code` 只认 `ERR_PNPM_*` token；英文短语仅在**无 code** 时作为收窄边界（或映射 `Unknown`） |
| **P1** | `crates/app_manager/src/utils.rs:149-161` `extract_reason` | `KNOWN.iter().find(\|k\| msg.contains(*k))` 扫 K8s **message** | 优先 kube status 结构化 **`reason`** 字段（`ContainerStatus.reason` / `PodCondition`）；message 扫描若保留必须单点文档化且白名单=官方 reason 常量 |
| **P2** | `crates/app-cli/src/xmlrpc.rs:56` `is_no_such_process` | `faultCode==10 \|\| message.contains("no process group"\|"BAD_NAME")` | **仅** `faultCode == Some(10)`（supervisord BAD_NAME）；faultString 兜底删除或降 debug 日志 |
| 已类型化 | download `Http{status}`、gateway `resp.code`、`ProcessPortInUse`、`AcpError::Timeout`、pnpm 自愈 `code==ERR_PNPM_IGNORED_BUILDS`、`From<io::Error> ErrorKind` | 已 R11 | 锁定，不回退 |
| 仅测试 | `assert!(…contains)` / `matches!(… msg.contains)` | 测试断言 | 允许文案快照；不得被生产复用 |
| 非错误 | 路径 `..`、i18n key、日志 filter、集合 contains | 业务 | 无关 |

**Rationale**  
用户要求「再检查还有哪些」；follow-up 重扫确认 git 外仅 3 处生产控制流。

**Alternatives considered**  
- 把测试断言也改掉：低收益，且测试检查用户可见文案合法。  
- 一次改 3 处：可以，但按 P1/P2 分 PR 更易归因。

## Decision 4: pnpm 无 code 时的归一策略

**Decision**  
`classify_failure`：`(FailureKind, Option<code>, message)`。  
1) code 表驱动 kind（`ERR_PNPM_FETCH_401`→RegistryAuth 等）；  
2) 无 code 时 `extract_plain_error_code` 仅解析 `Error: ERR_PNPM_*` 行；  
3) 仍无 code → `FailureKind::Unknown`（或保留极窄 OS errno 白名单 `ETIMEDOUT/ECONNREFUSED/…` 于**同一边界函数**）；  
4) 禁止 `"unauthorized"`/`"timed out"` 等自由英文短语独立分支。

**Rationale**  
自由短语跨 locale/版本漂移；code 与 errno 是半结构化边界。

## Decision 5: K8s reason 字段优先

**Decision**  
`extract_reason` 改为 `classify_k8s_reason(reason_field, message)`：`reason_field` 为 Some 且在官方常量集 → 直接用；否则收窄扫描或 `Unknown`。`derive_conditions` 消费枚举而非自由字符串。

## Decision 6: xmlrpc 只认 faultCode

**Decision**  
`RpcFault::is_no_such_process` → `faultCode == Some(10)`。文案 OR 条件删除（其注释已承认网络错误文案可误伤）。非 RpcFault 一律 `ShutdownUnconfirmed`，不得按文案判「不存在」。

## Decision 7: 策略不变 + 反例网

与既定 R11 一致：不改重试/自愈/回退语义；每点先红后绿（含：文案含 `BAD_NAME` 的网络错误、pnpm 无 code 假装 ignored-builds、K8s reason 字段命中而 message 无关键词）。

## Open Items
- ZCode git 改动完成后执行 Decision 2 Checklist（等待用户通知）。  
- kube-rs 实际暴露的 reason 路径以 `k8s_openapi` 为准（`ContainerStateTerminated.reason` / `PodCondition.reason`）。
