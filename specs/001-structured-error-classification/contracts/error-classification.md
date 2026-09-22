# Contract: 错误分类边界接口（结构化）

**Feature**: `001-structured-error-classification`  
**Date**: 2026-09-22  
**Scope**: 内部 Rust API 契约（跨模块消费）；HTTP 对外语义见 `http-semantics.md`

## C1. `download_utils::DownloadError`

```rust
pub enum DownloadError {
    Http { message: String, status: Option<u16> },
    Io(std::io::Error),
    Json(serde_json::Error),
    BinaryTooLarge { size: u64, max: u64 },
    ChecksumMismatch { expected: String, actual: String },
    StreamTruncated,
    Cancelled,
    InvalidUrl(String),
    TooManyRedirects(usize),
    RedirectMissingLocation,
}

impl DownloadError {
    /// 判据仅使用 status/variant；message 不参与
    pub fn is_retryable(&self) -> bool;
}
```

**Contract tests**（`crates/download_utils/src/error.rs` 或 `tests/`）:
- `Http{status: Some(404)}` → `is_retryable() == false`
- `Http{status: Some(503)}` → `is_retryable() == true`
- `Http{status: None}` → `is_retryable() == true`
- **反例**：`Http{ message: "GET http://ex/HTTP 4xx", status: Some(503) }` → `true`（文案含 "HTTP 4" 不得误判）

## C2. `git::log_history` 空仓库语义

```rust
pub fn log_history(
    repo: &Repository,
    max_count: usize,
    skip: usize,
    branch: Option<&str>,
    file_path: Option<&str>,
) -> AppResult<Vec<CommitInfo>>;
```

| 场景 | 期望 |
|------|------|
| `git init` 后 0 commit，`branch=None` | `Ok(vec![])` |
| 同上，`branch=Some("main")` | `Ok(vec![])` |
| 已有 commits | `Ok(non-empty)`，分页/skip/file_path 过滤保持 |
| 其它 gix 错误 | `Err`，**不得**落入空列表 |

**禁止**：`is_no_commit_error` 对 Display 的 `contains`；禁止 `"not found"` 兜底。

## C3. `pnpm` 自愈门控

```rust
// cli.rs heal 门控
let is_ignored_builds = matches!(
    &result,
    Err(InstallError::Failed { code, .. })
        if code.as_deref() == Some("ERR_PNPM_IGNORED_BUILDS")
);
```

| 场景 | 期望 |
|------|------|
| `code = Some(ERR_PNPM_IGNORED_BUILDS)` | 触发 ignored-builds 自愈 |
| `code = None`，message 含 "Ignored build scripts" | **不**触发（禁止文案门控） |
| `code = Some(ERR_PNPM_FETCH_401)` | 不触发；kind=RegistryAuth |

`classify_failure` 输出 `(FailureKind, Option<String> /*code*/, String /*message*/)`；`FailureKind` 优先由 code 表驱动。

## C4. `app_manager::extract_reason` → 结构化 reason

```rust
pub(super) fn extract_reason(msg: &str) -> Option<&str>; // 现状（待替换）
// 目标契约
pub fn classify_k8s_reason(reason_field: Option<&str>, message: Option<&str>) -> K8sContainerReason;
```

| 场景 | 期望 |
|------|------|
| `reason=Some("CrashLoopBackOff")` | `CrashLoopBackOff` |
| `reason=None`，message 含 CrashLoopBackOff | 允许（边界收窄白名单）或 `Unknown`——实现二选一，但测试锁定 |
| `reason=None`，message="Back-off restarting..." | 不得靠 `"not found"` 类过宽匹配 |

## C5. `app-cli` supervisord fault

```rust
fn is_no_such_process(&self) -> bool; // 现状含文案兜底
// 目标：仅 faultCode == Some(10)
```

| 场景 | 期望 |
|------|------|
| `faultCode=10` | `true` |
| `faultCode=50` + faultString 含 BAD_NAME | `false`（按官方码） |
| 非 RpcFault 网络错误，文案含 BAD_NAME | `false` |

## C6. 已类型化锁定（不得回退）

- `AppError::ProcessPortInUse` → `PreviewExecutorError::PortInUse`
- `resp.code == "not_found"` → gateway 路由回退
- `AcpError::Timeout` → `ExitCode::Timeout`
