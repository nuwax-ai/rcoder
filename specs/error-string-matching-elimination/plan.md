# 消除错误字符串匹配反模式 + ensure_lockfile 边界修复

> 状态：**待 Codex 审核**
> 作者：ZCode
> 日期：2026-09-17

## 1. 背景与动机

### 1.1 问题

rcoder 项目中有多处代码通过匹配错误消息的字符串内容来做逻辑控制（重试决策、错误分类、
路由降级等）。这种模式脆弱——外部库升级或内部消息改措辞就会静默破坏分支逻辑，且难以
通过编译器或测试发现。

同时 app-cli 的 `ensure_lockfile` 有一个边界 case：当 `node_modules` 存在但 lockfile
缺失时跳过生成，导致后续 devbuild 的 `--frozen-lockfile` 必然失败。

### 1.2 已发现的错误字符串匹配点（全量）

| # | 位置 | 匹配内容 | 控制什么 | 风险 |
|---|------|---------|---------|------|
| 1 | `download_utils/src/error.rs:53` | `msg.contains("HTTP 4")` | 重试决策 | HIGH |
| 2 | `agent_provisioning/src/error.rs:51` | `msg.contains("HTTP 4")` | 重试决策 | HIGH |
| 3 | `rcoder-gateway/src/cluster_cache.rs:58` | `msg.contains("not_found")` | 路由降级 | HIGH |
| 4 | `app-cli/src/xmlrpc.rs:217` | `format!("{error:#}").contains(...)` | 清理时错误抑制 | HIGH |
| 5 | `rcoder-cli/src/commands/chat.rs:180` | `err_str.contains("timed out")` | 退出码分类 | HIGH |
| 6 | `file-server/src/service/dev_server/coordinated.rs:163` | `message.contains("--strictPort 不自动换端口")` | 端口冲突分类 | MEDIUM |
| 7 | `file-server/src/service/pnpm/cli.rs:51` | `message.contains("ERR_PNPM_IGNORED_BUILDS")` | heal/retry | MEDIUM |
| 8 | `file-server/src/service/git/read.rs:299-304` | 多个 `.contains()` | 空仓库检测 | 已修复(dead code) |

### 1.3 设计原则

1. **结构化重构与重试策略变更分开**——本次只做类型化，不改变重试/降级策略
2. **错误从产生处保留类型**——不只改最后一个消费函数
3. **外部协议读真实字段**——Gateway 用服务端已有的 `code` 字段
4. **外部工具只有文本时，在边界集中解析**——业务层不再匹配文案
5. **lockfile 修复单独交付**——补真实生成及后续 frozen 构建测试

---

## 2. 逐项实施方案

### 2.1 DownloadError::Http 携带状态码

**问题**：`DownloadError::Http(String)` 把 HTTP 状态码序列化进字符串，`is_retryable`
用 `msg.contains("HTTP 4")` 判断 4xx。这会漏判/误判。

**方案**：

```rust
// crates/download_utils/src/error.rs
#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("HTTP {status}: {message}")]
    Http {
        message: String,
        status: Option<u16>,  // Some = 服务端返回了状态码；None = 连接级错误
    },
    // ... 其他变体不变
}

impl DownloadError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Http { status: Some(code), .. } => code / 100 != 4,
            Self::Http { status: None, .. } => true,  // 连接级错误可重试
            Self::Io(_) | Self::StreamTruncated => true,
            Self::ChecksumMismatch { .. } => true,
            _ => false,
        }
    }
}
```

**构造处更新**（全部，共 ~15 处）：

| 文件 | 行 | 当前 | status 值来源 |
|------|-----|------|-------------|
| `downloader.rs:180` | `Http(format!("GET {}: HTTP {}", url, status))` | `Some(status.as_u16())` |
| `downloader.rs:185` | 同上 | `Some(status.as_u16())` |
| `downloader.rs:97/98/99` | 客户端构建/任务失败 | `None`（连接级） |
| `downloader.rs:135` | GET 请求失败 | 从 `reqwest::Error::status()` 提取，或 `None` |
| `downloader.rs:280` | hash task 失败 | `None` |
| `downloader.rs:351` | read body 失败 | `None` |
| `downloader.rs:394` | redirect GET 失败 | 从 reqwest error 提取，或 `None` |
| `lib.rs:88` | 客户端构建 | `None` |
| `lib.rs:95` | HEAD 请求失败 | 从 reqwest error 提取，或 `None` |
| `memory.rs:40` | 客户端构建 | `None` |
| `memory.rs:52` | send 失败 | `None` |
| `memory.rs:55` | `error_for_status()` | **从 `reqwest::Error::status()` 提取真实状态码** |
| `memory.rs:70` | read body | `None` |
| `memory.rs:88` | UTF-8 解码 | `None` |

**注意**：这不是"等价重构"——旧逻辑用字符串匹配可能漏判（如 reqwest 格式不同）
或误判（URL 含 "HTTP 4"），新逻辑按真实状态分类，是行为修正。

**兼容性**：枚举没有 Serde 派生，不涉及 JSON 迁移；但 tuple variant → struct variant
是 Rust 源码不兼容变更，需更新所有消费者（已在上表列出）。

---

### 2.2 AgentDownloadError 同步

**问题**：`AgentDownloadError::InstallFailed(String)` 重复了同样的 `contains("HTTP 4")`
模式。

**前置检查**：先 grep 确认 `InstallFailed` 是否有实际构造点。

- **如果有构造点**：改为 `InstallFailed { message: String, http_status: Option<u16> }`，
  `is_retryable` 用 `http_status` 判断。明确：`http_status=None` 时返回 true（兼容
  策略，非逻辑保证——"安装失败且无 HTTP 状态"不天然意味着可重试）。
- **如果没有构造点**：删除该变体，`is_retryable` 只委托给 `Download(DownloadError)`。

**不要**把下载错误字符串化后再手工复制状态码到安装错误——下载错误应通过
`Download(#[from] DownloadError)` 保留原始类型。

---

### 2.3 xmlrpc RpcFault 结构化匹配

**问题**：`is_no_such_process` 用 `format!("{error:#}")` 序列化整个错误链后再匹配
字符串，损失了已解析的结构化信息。

**方案**：

```rust
// crates/app-cli/src/xmlrpc.rs
fn is_no_such_process(error: &anyhow::Error) -> bool {
    error.downcast_ref::<RpcFault>().is_some_and(|fault| {
        let code = fault.0.get("faultCode").and_then(serde_json::Value::as_i64);
        let msg = fault.0.get("faultString")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // faultCode 优先（supervisord 官方客户端也用 Faults.BAD_NAME 分类）；
        // faultString 作辅助（版本差异兼容）
        matches!(code, Some(10))
            || msg.contains("no process group")
            || msg.contains("BAD_NAME")
    })
}
```

**安全性**：`xmlrpc.rs:95` 会把"不存在"解释为停止/删除成功。必须补反例测试：
- 普通网络错误文案含 `BAD_NAME` → 不应判为 no_such_process
- 非目标 faultCode 但消息含 "no process group" → 不应判为 no_such_process
- malformed fault（缺字段）→ 不应判为 no_such_process

---

### 2.4 ViteStartupError 传播链

**问题**：`ViteStartupError::PortInUse` 在 `support.rs:13` 经 `into_app_error()` 转为
`AppError::system(String)` 后丢失类型，`coordinated.rs:163` 再用中文字符串匹配恢复。

**约束**：`executor_error(AppError)` 同时服务 start/stop/verify/read_log，后三者不是
ViteStartupError。不能统一改签名。

**方案**：启动链路径保留 ViteStartupError 分类信息，其他操作保持原样。

选项 A（推荐）：`ViteStartupError::into_app_error` 返回时，在 AppError 中保留分类。
可通过 AppError 的扩展字段或专用 variant 实现。`coordinated.rs` 的启动适配处匹配该
variant。

选项 B：`support.rs` 启动失败时，把 `ViteStartupError` 和 `AppError` 一起传给
`executor_error`，函数内根据 ViteStartupError 分类。

**验收**：修改中文提示后，PortInUse 分类不变。

---

### 2.5 AcpError::Timeout

**问题**：`acp_client.rs` 用 `anyhow::bail!("Prompt timed out...")` 抛超时错误，
`chat.rs` 用 `format!("{}", e).contains("timed out")` 分类退出码。

**方案**：复用仓库已有的 `crates/agent_abstraction/src/acp/mod.rs` 的 `AcpError`，
添加 Timeout 变体：

```rust
// crates/agent_abstraction/src/acp/mod.rs
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    // ... 已有变体 ...
    #[error("prompt completion timed out after {timeout:?}")]
    Timeout { timeout: std::time::Duration },
}
```

在 `acp_client.rs` 等待超时处构造此变体。调用方 `chat.rs` 匹配：

```rust
Err(AcpError::Timeout { .. }) => ExitCode::Timeout,
```

**注意**：此变体仅表示**等待完成超时**，不覆盖发送/连接/cancel 超时。原来宽泛的
`"timed out"` 匹配可能把它们混在一起，改后 CLI 退出码存在行为变化，应明确测试。

---

### 2.6 EnsurePodResponse.code

**问题**：`cluster_cache.rs` 用 `msg.contains("not_found")` 匹配 API 响应消息。

**事实修正**：服务端已经返回 `code` 字段（非 `error_code`）：

```json
{
  "success": false,
  "code": "not_found",
  "message": "RCoder container not found, route through control plane"
}
```

依据：`rcoder/src/handler/internal_handler.rs:145`、`shared_types/src/model/http_result.rs:35`

**方案**：

```rust
// crates/rcoder-gateway/src/control_plane_client.rs
pub struct EnsurePodResponse {
    pub success: bool,
    pub code: Option<String>,  // ← 用 code，匹配服务端实际字段名
    pub data: Option<EnsurePodData>,
    pub message: Option<String>,
}
```

```rust
// crates/rcoder-gateway/src/cluster_cache.rs
if resp.code.as_deref() == Some("not_found") {
```

普通派生反序列化下，缺失的 Option 字段默认 None，不需要额外 `#[serde(default)]`。
不要把缺失或未知 code 默认解释为 not_found。

**注意**：当前 `gateway_proxy.rs:258` 对所有 ensure 错误都会回退控制面。本次只修正
分类和诊断，**不改变降级策略**。若要实现"只有 not_found 才降级"，需另行定义。

---

### 2.7 pnpm ERR_PNPM_IGNORED_BUILDS

**问题**：`cli.rs:51` 双通道检查——typed code + message fallback。

**方案**：只保留 typed code 通道：

```rust
// crates/file-server/src/service/pnpm/cli.rs
let is_ignored_builds = matches!(
    &result,
    Err(InstallError::Failed { code, .. })
        if code.as_deref() == Some("ERR_PNPM_IGNORED_BUILDS")
);
```

**保留 `classify.rs` 的边界文本解析**——它从原始输出提取 code 的逻辑不动。
目标是"边界解析外部文本一次，内部使用结构化结果"，不是删除所有字符串解析。

需验证：
- NDJSON 输出正确触发
- 纯文本错误经 classify 解析后正确触发
- 普通 message 仅提及该错误码，不触发自动批准

---

### 2.8 ensure_lockfile 三个补丁

**问题**：
1. 生成失败只打印警告，devbuild 继续执行必然失败
2. 有 `node_modules` 但无 lockfile 时跳过
3. `pnpm install` 有副作用（安装依赖、执行脚本）

**补丁 ①**：生成失败 → 本服务计为 failed 并跳过构建

```rust
if let Err(e) = ensure_lockfile(&task.project_path) {
    println!("❌ [{}] lockfile 生成失败: {e:#}", task.service_id);
    failed += 1;
    continue;
}
```

**补丁 ②**：lockfile 精确匹配——只检查 devbuild 命令对应的包管理器

如果 devbuild 命令包含 `pnpm`，只检查 `pnpm-lock.yaml`。
如果包含 `npm`，只检查 `package-lock.json`。
如果包含 `yarn`，只检查 `yarn.lock`。
同时识别 monorepo 根目录已有的共享 lockfile。

**补丁 ③**：用 `--lockfile-only --no-frozen-lockfile` 只生成 lockfile 不安装依赖

```rust
let status = Command::new(pm)
    .args(["install", "--lockfile-only", "--no-frozen-lockfile"])
    .current_dir(project_path)
    .status()
    .with_context(|| format!("spawn {pm} install --lockfile-only"))?;
```

生成后确认 lockfile 确实存在。删除 `node_modules` 存在即跳过的条件。

---

## 3. 不在本次范围

| 项 | 原因 |
|---|---|
| `file-server/git/read.rs` is_no_commit_error | 已修复为 `is_unborn()` 前置检查，函数已标 dead_code |
| `rcoder-storage/writer.rs` PG SQLSTATE 匹配 | 协议级稳定（SQL 标准），无需改动 |
| pnpm `classify.rs` 边界文本解析 | 属于"边界集中解析"，是正确模式，不删除 |

---

## 4. 验证计划

### 4.1 编译检查

```bash
cargo check --workspace --all-features
```

### 4.2 聚焦测试（每项改动后）

```bash
cargo nextest run -p download_utils --no-fail-fast --all-features
cargo nextest run -p agent_provisioning --no-fail-fast --all-features
cargo nextest run --manifest-path crates/app-cli/Cargo.toml --no-fail-fast --all-features
cargo nextest run -p file-server --no-fail-fast --all-features
cargo nextest run -p rcoder-gateway --no-fail-fast --all-features
cargo nextest run -p agent_abstraction --no-fail-fast --all-features
```

### 4.3 全量检查

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo nextest run --workspace --no-fail-fast --all-features
```

### 4.4 特殊验收

- **#4 ViteStartupError**：修改中文提示后，PortInUse 分类不变
- **#5 AcpError::Timeout**：CLI 退出码行为测试
- **#8 ensure_lockfile**：zip 导入无 lockfile → devbuild 成功的端到端验证

---

## 5. 实施顺序

建议按依赖关系和风险排序：

1. **#6 EnsurePodResponse.code**（最简单，字段名修正）
2. **#7 pnpm 去 message fallback**（一行改动）
3. **#3 xmlrpc RpcFault**（局部改动，需补测试）
4. **#1 DownloadError::Http**（改动面最广，~15 处构造点）
5. **#2 AgentDownloadError**（依赖 #1 的类型变更）
6. **#5 AcpError::Timeout**（新增变体，需测试退出码）
7. **#4 ViteStartupError 传播链**（设计最复杂）
8. **#8 ensure_lockfile 三补丁**（独立于上述各项）
