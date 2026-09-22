# Tasks: 结构化错误分类——消除错误字符串匹配

**Input**: `/Users/soddy/Documents/git-workspace/rcoder/specs/001-structured-error-classification/`
**Prerequisites**: plan.md, research.md, data-model.md, contracts/
**Coordination**: git 字符串匹配由 ZCode 在工作区修改中；**本清单不重复实施 git**，只保留复查门禁。

## Format: `[ID] [P?] Description`

## Phase 3.1: Setup
- [ ] T001 记录基线：branch `001-structured-error-classification`；ZCode 未提交改动文件列表（`git status --short`）

## Phase 3.1b: ZCode git 复查门禁
- [x] T-REVIEW 已执行（e985badb0；39 git 测试绿）— 报告见 `review-zcode-git.md`
- [x] T-REVIEW2 问题已写入 `review-zcode-git.md`，交 ZCode 继续
- [x] T-FIX-A1 修 G-A1：`resolve_rev` 支持 `HEAD~1`/`HEAD^` 修订语法 + T-d1/T-d2 反例（先红后绿）
- [x] T-FIX-A2 修 G-A2：缩写 OID 前缀解析（歧义前缀→Validation 显式错误）+ T-d3/T-d5（T-d5 锁定：越界=`HEAD~99`→空数据，歧义→显式错误）
- [x] T-FIX-B1 修 G-B1：`diff.rs` from/to 迁 `resolve_rev` + T-d6（缺 from 保持 system 类，零 HTTP 变化）
- [x] T-FIX-B2 修 G-B2/G-B3：`ops.rs`/`refs.rs` 迁结构化解析 + T-d7；**另修报告遗漏的 create_tag 裸 `head_id()`（G-B4 同款）**
- [x] T-FIX-C1 修 G-C1：`has_any_commit` 改 `AppResult<bool>`（与 `is_unborn` 同源，真错误传播）+ T-d4；附带修 `commit_indexed` parents 吞错（损坏 HEAD 曾静默写孤立 root commit，反例先红）

## Phase 3.2: Tests First (TDD) ⚠️ 与 git 无关，可并行
- [x] T002 [P] 反例 pnpm：`code=None` + message 含 ignored-builds/unauthorized 文案 → 不得映射对应 kind/自愈；`code=ERR_PNPM_FETCH_401` → RegistryAuth in `crates/file-server/src/service/pnpm/classify.rs`（先红后绿）
- [x] T003 [P] 反例 K8s：`reason=Some("CrashLoopBackOff")` 而 message 无该词 → 必须命中；message-only 过宽子串不得吞错 in `crates/app_manager/src/utils.rs`（枚举版已在 app_manager 218 跑中全绿）
- [x] T004 [P] 反例 xmlrpc：网络错误文案含 `BAD_NAME` → 非 NoSuchProcess；`faultCode=10` → true；`faultCode=50`+faultString BAD_NAME → false in `crates/app-cli/src/xmlrpc.rs`（先红后绿；r11 存量断言按 C5 更新=faultString 兜底删除）
- [ ] T005 [P] 锁定：已类型化点（Http status / resp.code / ProcessPortInUse / AcpError::Timeout / pnpm 自愈 code）不得回退

## Phase 3.3: Core Implementation（git 除外）
- [x] T006 P1：`crates/file-server/src/service/pnpm/classify.rs` — `FailureKind` code 表驱动；`extract_plain_error_code` 仅 `ERR_PNPM_*`；删除自由英文短语独立分支（收窄到单点 errno token 白名单，全 token 等值非子串）；无 code → `Unknown`
- [x] T007 P1：`crates/app_manager/src/utils.rs` — `extract_reason` → `classify_k8s_reason(reason, message)`；优先结构化 `reason`；message 白名单单点文档化（整 token 兜底保留）
- [x] T008 P2：`crates/app-cli/src/xmlrpc.rs` — `is_no_such_process` 仅 `faultCode == Some(10)`；faultString 兜底删除
- [x] T009 [P] `derive_conditions` / 调用方改消费结构化 reason（`DeploymentStatus.reason: Option<ContainerFailureReason>` 枚举穿透 container-runtime-api + K8s/Docker 双实现 + mock）

## Phase 3.4: Integration
- [ ] T010 pnpm 自愈链路仍只认 `code == ERR_PNPM_IGNORED_BUILDS`（`crates/file-server/src/service/pnpm/cli.rs`）
- [x] T011 HTTP 兼容抽查：`/api/git/log` 空数据形状与 TS 一致（unborn/缺 ref/越界=空列表锁测试）；另 N1 worktree file-content 缺文件对齐 TS 空串契约

## 本批补充（复查报告未覆盖的问题，ZCode 修）
- [x] N1 worktree file-content 缺文件→空串（原 500，TS 契约 `existsSync?read:""`）+ 非 UTF-8 统一 lossy（`worktree_content` 抽函数）
- [x] N2 `resolve_rev` OID 回退 `find_object` 按 `existing::Error` 类型化（NotFound=缺席，真错误传播，原 `is_ok()` 全吞）
- [x] N3 `ops.rs` reset `previous_head` 的 `head_id().ok()` 吞错 → `resolve_rev("HEAD")` 真错误传播
- [x] N4 refs.rs 缺分支/缺标签/重名显式错误契约锁测试（HTTP 类别按拍板保持 system 不变）
- 新增显式错误面（拍板允许的例外）：歧义短前缀→Validation；坏修订表达式（`HEAD~x`）→Validation
- 未做项：G-B1「缺 from 改 400」按用户拍板「严格零 HTTP 变化」保持 system 500；`@{}`/`:/` 等 revspec 不支持（Java/TS 契约未承诺，文档收窄）

## Phase 3.5: Polish
- [ ] T012 [P] 注释清理：删除暗示「对齐 TS 文案匹配」的表述，改为「仅 HTTP 契约对齐」
- [ ] T013 静态门禁：`rg 'msg\.contains|message\.contains|err_str\.contains|to_string\(\)\.contains' crates --type rust | rg -v 'assert!|#\[cfg\(test\)\]'` 仅剩文档化边界
- [ ] T014 聚焦 nextest：file-server / app_manager / app-cli（独立 workspace）
- [ ] T015 根 workspace fmt/clippy/nextest
- [ ] T016 执行 quickstart.md 行为等价表

## Dependencies
```
T001
 ├─ T-REVIEW → T-REVIEW2 → T011（等用户通知 ZCode 完成）
 └─ T002–T005 [P] 反例先红
      └─ T006–T009（T006/T007/T008 不同文件可 [P]）
           └─ T010–T016
```

## Parallel Execution Examples
```bash
# 组 1（ZCode 改 git 期间即可进行）
# T002 ∥ T003 ∥ T004 ∥ T005 测试先红

# 组 2
# T006 (pnpm/classify.rs) ∥ T007 (app_manager/utils.rs) ∥ T008 (app-cli/xmlrpc.rs)
```

## Validation Checklist
- [x] 剩余 3 处生产点有反例 + 实现任务
- [x] git 有独立复查门禁、不与 ZCode 冲突
- [x] 已类型化点有锁定测试
- [x] HTTP 兼容 / 内部无 TS 字符串逻辑写入 spec Clarifications
