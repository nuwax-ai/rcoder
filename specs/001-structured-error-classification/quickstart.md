# Quickstart: 结构化错误分类——验证与回归

**Feature**: `001-structured-error-classification`  
**Date**: 2026-09-22

## 0. 范围
本特性为类型化重构 + 空仓库 `git log` 行为修正。**不改变**重试/降级/自愈业务策略。

## 1. 先红后绿（TDD）

```bash
cd /Users/soddy/Documents/git-workspace/rcoder

# 反例必须先失败（示例筛选，按 tasks 展开）
cargo nextest run -p file-server --no-fail-fast git::read
cargo nextest run -p download_utils --no-fail-fast
cargo nextest run -p app_manager --no-fail-fast extract_reason
cargo nextest run -p app-cli --no-fail-fast xmlrpc
```

**关键反例（修复前应 FAIL）**
1. 空仓库 `log_history` → 期望 `Ok(vec![])`（当前因 `"…yet"` 匹配偏移返回 Err）
2. `DownloadError::Http{status: Some(503), message: 含 "HTTP 4"}` → 可重试
3. pnpm `code=None` + 文案含 ignored-builds → 不得自愈
4. xmlrpc 非 fault 错误文案含 `BAD_NAME` → 不得判「进程不存在」

## 2. 实现后全量

```bash
# 聚焦
cargo nextest run -p file-server --no-fail-fast --all-features
cargo nextest run -p download_utils --no-fail-fast --all-features
cargo nextest run -p agent_provisioning --no-fail-fast --all-features
cargo nextest run -p app_manager --no-fail-fast --all-features
cargo nextest run -p rcoder-gateway --no-fail-fast --all-features
cargo nextest run -p rcoder-cli --no-fail-fast --all-features

# app-cli 独立 workspace
cargo fmt --manifest-path crates/app-cli/Cargo.toml -- --check
cargo clippy --manifest-path crates/app-cli/Cargo.toml --all-targets
cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast --all-features

# 根 workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo nextest run --workspace --no-fail-fast --all-features
```

## 3. 行为等价核对（人工清单）

| 路径 | 核对 |
|------|------|
| 下载 4xx/5xx | 重试次数与之前一致 |
| Gateway not_found | 仍回退控制面 |
| pnpm ignored-builds | 仍触发自愈 |
| CLI prompt 超时 | 仍退出 Timeout |
| 端口占用 | 仍 PortInUse |
| 空仓库 git log | **变为** 空列表成功（预期差异） |

## 4. 静态门禁（防回潮）

```bash
# 业务代码不得新增错误文案控制流（评审 + 抽样）
rg -n 'msg\.contains|message\.contains|err_str\.contains|to_string\(\)\.contains' crates --type rust \
  | rg -v 'assert!|#\[cfg\(test\)\]|mod tests'
```
允许：边界解析器内部（pnpm classify / xmlrpc faultString 文档化兜底 / k8s reason 白名单）与测试断言。

## 5. 完成标准
- [ ] 全部反例修复前红、修复后绿
- [ ] 无策略行为变化（上表除空仓库外全一致）
- [ ] workspace fmt/clippy/nextest 通过（含 app-cli 独立检查）
- [ ] `rg` 抽样无新增业务层文案分支
