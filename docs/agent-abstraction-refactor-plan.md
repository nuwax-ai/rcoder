# agent_abstraction 模块重构文档 — 支持 CLI 复用

## 1. 背景与目标

### 1.1 动机

随着 `rcoder-cli`（ACP 命令行调试工具）的引入，`agent_abstraction` crate 需要同时服务两个消费者：

| 消费者 | 运行模式 | 会话模型 | 通知方式 |
|--------|---------|---------|---------|
| `agent_runner` | 长驻服务 (HTTP/gRPC) | 多会话并发 | SSE 推送 |
| `rcoder-cli` | 短生命周期 CLI | 单会话 | 终端输出 |

当前 `agent_abstraction` 的公开 API 已经具备良好的 trait 抽象（`SessionNotifier`、`SessionRegistry`、`PermissionRequestHandler`），但在以下方面存在改进空间：

1. **一个耦合违规**：`StateAwareNotifier` 直接依赖 `AGENT_REGISTRY` 静态变量，绕过了 `SessionRegistry` trait
2. **缺少面向简单场景的便捷 API**：CLI 只需要单会话，但当前最短路径仍需组装 `AcpSessionManager` + `AcpAgentWorker` + `WorkerRequest` 等完整链路
3. **进程诊断信息封闭**：agent 子进程的 stderr、exit code 等信息被 `run_sacp_connection` 内部消费，外部只能通过 `SessionNotifier` 间接获取，CLI 场景需要更直接的诊断通道

### 1.2 目标

- 修复已知的耦合违规，使 trait 边界更加干净
- 提供面向单会话场景的便捷 API（`AcpClientBuilder`），降低 CLI 的接入成本
- 开放进程诊断通道，支持 CLI 场景的详细错误输出
- **不破坏 `agent_runner` 的现有行为**

### 1.3 原则

- **渐进式重构**：每次改动可独立验证，不做大爆炸式重写
- **向后兼容**：`agent_runner` 的现有调用方式继续有效
- **最小公开面**：只暴露 CLI 实际需要的 API，不为了"通用性"过度暴露

---

## 2. 现状分析

### 2.1 当前依赖图

```
shared_types ← agent_config ← agent_abstraction ← agent_runner
                                     ↑
                                     └── rcoder-cli (新增)
```

**结论**：Cargo 依赖链是干净的 DAG，无循环依赖。`agent_abstraction` 对 `agent_runner` 零引用。trait 边界设计符合依赖倒置原则 (DIP)。

### 2.2 当前公开 API 清单

| 模块 | 公开类型/函数 | CLI 是否需要 |
|------|-------------|-------------|
| `launcher::claude_code_sacp` | `SacpClaudeCodeLauncher` | 是 |
| `launcher::claude_code_sacp` | `SacpAgentLaunchConfig` | 是 |
| `launcher::claude_code_sacp` | `SacpLauncherConnectionInfo` | 是 |
| `launcher::model_env` | `ModelRuntimeEnvResolver` trait | 是 |
| `launcher::model_env` | `DirectModelRuntimeEnvResolver` | 是 |
| `launcher::lifecycle` | `AgentLifecycleGuard` | 是 |
| `session` | `AcpSessionManager` | 可选（复杂场景） |
| `session` | `AcpAgentWorker` | 可选（复杂场景） |
| `session` | `AgentWorker` trait | 可选 |
| `session` | `WorkerRequest` / `WorkerResponse` | 可选 |
| `traits` | `SessionNotifier` trait | 是 |
| `traits` | `SessionRegistry` trait | 是 |
| `traits` | `PermissionRequestHandler` trait | 是 |
| `traits` | `YoloPermissionRequestHandler` | 是 |
| `acp` | `AgentConnection` | 否（遗留抽象） |

### 2.3 `pub(crate)` 内部项

| 文件 | 项 | CLI 是否需要直接访问 |
|------|---|-------------------|
| `connection.rs` | `run_sacp_connection()` | **否** — 通过 `SacpClaudeCodeLauncher::launch()` 间接使用 |
| `connection.rs` | `SacpConnectionParams` | **否** — 内部参数结构体 |
| `types.rs` | `VERSION`, `ENV_*` 常量 | **否** — launcher 内部使用 |
| `env.rs` | 模板渲染、环境变量工具函数 | **否** — launcher 内部使用 |
| `mcp.rs` | MCP server 辅助函数 | **否** — config 转换层内部使用 |
| `process.rs` | `take_stdio` | **否** — launcher 内部使用 |

**结论**：CLI 不需要访问任何 `pub(crate)` 项。所有 `pub(crate)` 项都是 SACP 连接的内部管线，通过 `SacpClaudeCodeLauncher::launch()` 的公开 API 即可触达。

### 2.4 耦合违规点

```
agent_runner/service/state_aware_notifier.rs
    │
    │  直接引用具体静态变量（绕过 SessionRegistry trait）
    ▼
agent_runner/service/agent_registry.rs
    AGENT_REGISTRY.try_update_agent_info()   ← 具体实现，非 trait 方法
```

`StateAwareNotifier` 同时实现了 `SessionNotifier` trait 和直接操作 `AGENT_REGISTRY` 静态变量的逻辑。这违反了 DIP 原则：trait 实现不应该依赖具体的全局状态。

---

## 3. 重构方案

### 3.1 重构项总览

| 编号 | 重构项 | 影响范围 | 复杂度 | 优先级 |
|------|--------|---------|--------|--------|
| R-1 | 修复 StateAwareNotifier 耦合违规 | agent_runner | 低 | P1 |
| R-2 | 新增 `AcpClientBuilder` 便捷 API | agent_abstraction | 中 | P0 |
| R-3 | 开放进程诊断通道 | agent_abstraction | 中 | P0 |
| R-4 | 新增 `InteractivePermissionHandler` | agent_abstraction | 低 | P2 |
| R-5 | 清理遗留 `AgentConnection` 抽象 | agent_abstraction | 低 | P2 |

---

### 3.2 R-1：修复 StateAwareNotifier 耦合违规

#### 问题

`StateAwareNotifier` 在实现 `SessionNotifier` 的同时，直接调用 `AGENT_REGISTRY.try_update_agent_info()` 来更新 agent 状态。这个操作应该通过 `SessionRegistry` trait 完成，而不是直接操作具体的全局变量。

#### 方案

扩展 `SessionRegistry` trait，增加状态更新方法：

```rust
// agent_abstraction/src/traits/session_registry.rs

pub trait SessionRegistry: Send + Sync {
    type Entry: SessionEntry;

    // ... 现有方法 ...

    // ========== 新增：Agent 状态更新方法 ==========

    /// 更新 agent 状态（如 Active → Idle）
    fn update_agent_status(&self, project_id: &str, status: AgentStatus);

    /// 更新最后活动时间
    fn update_last_activity(&self, project_id: &str, activity: chrono::DateTime<chrono::Utc>);
}
```

`StateAwareNotifier` 改为通过注入的 `SessionRegistry` 调用这些方法：

```rust
// agent_runner/service/state_aware_notifier.rs

pub struct StateAwareNotifier<R: SessionRegistry> {
    inner: SseSessionNotifier,
    registry: Arc<R>,  // 通过 trait 引用，不依赖具体类型
}

impl<R: SessionRegistry> SessionNotifier for StateAwareNotifier<R> {
    async fn notify_session_update(&self, ...) {
        self.inner.notify_session_update(...).await;
        // 通过 trait 方法更新状态，不直接操作 AGENT_REGISTRY
        self.registry.update_agent_status(project_id, new_status);
    }
}
```

#### 影响

- `SessionRegistry` trait 新增 2 个方法
- `AgentSessionRegistry`（agent_runner）实现新方法（内部仍然调用 `try_update_agent_info`，但调用点收敛到 trait 实现内）
- `StateAwareNotifier` 改为泛型 `<R: SessionRegistry>`
- `AgentSessionService` 构造 `StateAwareNotifier` 时传入 registry 引用

#### 验证方式

- `agent_runner` 现有测试通过
- `StateAwareNotifier` 不再直接 `use` `AGENT_REGISTRY`

---

### 3.3 R-2：新增 `AcpClientBuilder` 便捷 API

#### 问题

CLI 场景只需要启动一个 agent、发几条 prompt。但当前最短路径需要：

```rust
// 当前：CLI 需要的最小代码 (~40 行)
let env_resolver = DirectModelRuntimeEnvResolver::new(...);
let notifier = TerminalSessionNotifier::new();
let registry = SimpleSessionRegistry::new();
let permission_handler = YoloPermissionRequestHandler;

let launcher = SacpClaudeCodeLauncher::new(
    Arc::new(notifier),
    Arc::new(env_resolver),
    Arc::new(permission_handler),
);

let launch_config = SacpAgentLaunchConfig { ... };  // 组装 10+ 字段
let connection = launcher.launch(launch_config).await?;

// 手动管理 prompt_tx、cancel_tx 通道
let prompt = PromptRequest { ... };  // 组装 prompt
connection.prompt_tx.send(prompt).await?;
```

对于 CLI 场景，这太重了。

#### 方案

在 `agent_abstraction` 中新增 `AcpClientBuilder`，提供流式 API：

```rust
// agent_abstraction/src/client/builder.rs

pub struct AcpClientBuilder<N: SessionNotifier, R: SessionRegistry> {
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    working_dir: PathBuf,
    project_id: String,
    agent_id: String,
    session_id_hint: Option<String>,
    system_prompt: Option<String>,
    mcp_servers: Vec<McpServer>,
    agent_mode: AgentMode,
    model_provider: Option<ModelProviderConfig>,
    timeout: Duration,
    notifier: Arc<N>,
    registry: Arc<R>,
    permission_handler: Arc<dyn PermissionRequestHandler>,
    model_env_resolver: Arc<dyn ModelRuntimeEnvResolver>,
}

impl<N: SessionNotifier + 'static, R: SessionRegistry + 'static> AcpClientBuilder<N, R> {
    pub fn new(notifier: N, registry: R) -> Self { ... }

    pub fn command(mut self, cmd: impl Into<String>) -> Self { ... }
    pub fn args(mut self, args: Vec<String>) -> Self { ... }
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self { ... }
    pub fn working_dir(mut self, dir: impl Into<PathBuf>) -> Self { ... }
    pub fn project_id(mut self, id: impl Into<String>) -> Self { ... }
    pub fn agent_id(mut self, id: impl Into<String>) -> Self { ... }
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self { ... }
    pub fn mcp_server(mut self, server: McpServer) -> Self { ... }
    pub fn model_provider(mut self, config: ModelProviderConfig) -> Self { ... }
    pub fn timeout(mut self, timeout: Duration) -> Self { ... }
    pub fn permission_handler(mut self, handler: Arc<dyn PermissionRequestHandler>) -> Self { ... }
    pub fn resume_session(mut self, session_id: impl Into<String>) -> Self { ... }

    /// 启动 agent 并返回 AcpClient 句柄
    pub async fn start(self) -> Result<AcpClient<N, R>> { ... }
}
```

返回的 `AcpClient` 封装了连接管理：

```rust
// agent_abstraction/src/client/acp_client.rs

pub struct AcpClient<N: SessionNotifier, R: SessionRegistry> {
    session_manager: AcpSessionManager<N, R>,
    project_id: String,
    lifecycle_guard: AgentLifecycleGuard,
}

impl<N: SessionNotifier + 'static, R: SessionRegistry + 'static> AcpClient<N, R> {
    /// 发送 prompt 并等待响应
    pub async fn send_prompt(&self, prompt: impl Into<String>) -> Result<PromptResponse> {
        // 内部组装 PromptRequest，通过 channel 发送，等待完成
    }

    /// 发送带附件的 prompt
    pub async fn send_prompt_with_attachments(
        &self,
        prompt: impl Into<String>,
        attachments: Vec<Attachment>,
    ) -> Result<PromptResponse> { ... }

    /// 取消当前 prompt
    pub async fn cancel(&self) -> Result<()> { ... }

    /// 获取 session_id
    pub fn session_id(&self) -> &str { ... }

    /// 优雅停止 agent
    pub async fn stop(self) -> Result<()> {
        self.lifecycle_guard.graceful_stop().await;
    }
}
```

**CLI 使用示例**（~15 行）：

```rust
let client = AcpClientBuilder::new(
        TerminalSessionNotifier::new(),
        SimpleSessionRegistry::new(),
    )
    .command("python")
    .args(vec!["./my-agent.py".into()])
    .env("DEBUG", "true")
    .working_dir("/workspace")
    .timeout(Duration::from_secs(300))
    .start()
    .await?;

let response = client.send_prompt("分析这段代码").await?;
println!("Agent: {}", response.text);

client.stop().await?;
```

#### 模块结构

```
agent_abstraction/src/
├── client/                    # 新增模块
│   ├── mod.rs                 # pub use
│   ├── builder.rs             # AcpClientBuilder
│   └── acp_client.rs          # AcpClient
```

#### 实现要点

- `AcpClientBuilder::start()` 内部组装 `SacpAgentLaunchConfig` + 创建 `AcpSessionManager` + 调用 `get_or_create_session()`
- `AcpClient::send_prompt()` 内部组装 `WorkerRequest` + 调用 `AcpAgentWorker::process_request()` + 等待响应
- 对 `agent_runner` 无影响 — 这是新增 API，不修改现有 API

#### 验证方式

- 新增单元测试：构建 builder → start → send_prompt → stop
- CLI 原型可以仅依赖 `AcpClientBuilder` + `AcpClient` 运行

---

### 3.4 R-3：开放进程诊断通道

#### 问题

当前 agent 子进程的诊断信息（stderr、exit code、命令路径检查）被封闭在 `run_sacp_connection` 和 `AgentLifecycleGuard` 内部。CLI 场景需要在启动失败时输出：

- agent 进程的 exit code
- stderr 最后 N 行
- 命令路径是否可执行
- 工作目录信息

这些信息目前只有通过 `SessionNotifier::notify_prompt_error` 间接获得，且信息量不足。

#### 方案

新增 `ProcessDiagnostics` 结构体和 `DiagnosticsListener` trait：

```rust
// agent_abstraction/src/diagnostics/mod.rs

/// Agent 进程诊断信息
#[derive(Debug, Clone)]
pub struct ProcessDiagnostics {
    /// agent 命令
    pub command: String,
    /// 命令参数
    pub args: Vec<String>,
    /// 工作目录
    pub working_dir: PathBuf,
    /// 进程 PID
    pub pid: u32,
    /// 退出码（如果已退出）
    pub exit_code: Option<i32>,
    /// stderr 最后 N 行
    pub stderr_tail: Vec<String>,
    /// 命令是否存在（which 检查结果）
    pub command_exists: bool,
    /// 启动耗时（毫秒）
    pub startup_duration_ms: u64,
    /// ACP 初始化是否成功
    pub acp_init_success: bool,
    /// 错误描述
    pub error_message: Option<String>,
}

/// 诊断事件监听器 trait
pub trait DiagnosticsListener: Send + Sync {
    /// agent 进程启动时调用
    fn on_process_started(&self, pid: u32, command: &str);

    /// ACP 初始化完成时调用
    fn on_acp_initialized(&self, session_id: &str);

    /// agent 进程退出时调用
    fn on_process_exited(&self, diagnostics: &ProcessDiagnostics);

    /// agent 进程异常时调用（启动失败、ACP 连接断开等）
    fn on_process_error(&self, diagnostics: &ProcessDiagnostics);
}
```

在 `AcpClientBuilder` 中注入：

```rust
let client = AcpClientBuilder::new(notifier, registry)
    .command("python")
    .args(vec!["./my-agent.py".into()])
    .diagnostics_listener(Arc::new(TerminalDiagnosticsListener::new()))
    .start()
    .await?;
```

**CLI 的 `TerminalDiagnosticsListener` 实现**：

```rust
impl DiagnosticsListener for TerminalDiagnosticsListener {
    fn on_process_error(&self, diag: &ProcessDiagnostics) {
        eprintln!("[ACP] Agent 进程异常:");
        eprintln!("  命令: {} {}", diag.command, diag.args.join(" "));
        eprintln!("  工作目录: {}", diag.working_dir.display());
        if let Some(code) = diag.exit_code {
            eprintln!("  退出码: {}", code);
        }
        if !diag.stderr_tail.is_empty() {
            eprintln!("  stderr 输出:");
            for line in &diag.stderr_tail {
                eprintln!("    {}", line);
            }
        }
        if !diag.command_exists {
            eprintln!("  诊断: 命令不存在 (which 未找到)");
        }
    }
}
```

#### 实现要点

- `DiagnosticsListener` 是可选注入（`Option<Arc<dyn DiagnosticsListener>>`），`agent_runner` 不注入即可，零影响
- `SacpClaudeCodeLauncher::launch()` 中已有 `stderr_reader_task` 收集 stderr，增加回调点即可
- `ProcessDiagnostics` 在进程退出时由 `AgentLifecycleGuard` 的 reaper task 组装

#### 验证方式

- CLI 场景：agent 启动失败时能看到 stderr 和 exit code
- agent_runner 场景：不注入 listener，行为不变

---

### 3.5 R-4：新增 `InteractivePermissionHandler`

#### 问题

CLI 交互模式下，`YoloPermissionRequestHandler` 自动批准所有权限请求（包括危险操作），不适合开发者调试需要审查工具调用的场景。

#### 方案

在 `agent_abstraction::traits::permission_handler` 中新增：

```rust
/// 交互式权限处理器 — 在终端中提示用户确认
///
/// 由 CLI 消费者实现具体的终端交互逻辑（通过 PermissionPrompt trait）
pub struct InteractivePermissionHandler<P: PermissionPrompt> {
    prompt: Arc<P>,
}

/// 终端交互抽象 — CLI 实现此 trait 来渲染权限确认 UI
#[async_trait]
pub trait PermissionPrompt: Send + Sync {
    /// 展示权限请求，等待用户选择
    async fn prompt_user(
        &self,
        context: &PermissionRequestContext,
        options: &[PermissionOption],
    ) -> Result<PermissionOptionKind>;
}

/// 权限选项（展示给用户的选择）
pub struct PermissionOption {
    pub label: String,
    pub kind: PermissionOptionKind,
    pub description: Option<String>,
}
```

CLI 实现 `PermissionPrompt`：

```rust
struct TerminalPermissionPrompt;

#[async_trait]
impl PermissionPrompt for TerminalPermissionPrompt {
    async fn prompt_user(&self, context: &PermissionRequestContext, options: &[PermissionOption]) -> Result<PermissionOptionKind> {
        eprintln!("\n[ACP] Agent 请求权限:");
        eprintln!("  工具: {}", context.tool_name);
        eprintln!("  参数: {}", serde_json::to_string_pretty(&context.tool_input)?);
        eprintln!();
        for (i, opt) in options.iter().enumerate() {
            eprintln!("  [{}] {}", i + 1, opt.label);
        }
        eprint!("请选择 (1-{}): ", options.len());
        // 读取用户输入...
    }
}
```

#### 影响

- `agent_abstraction` 新增 trait 和泛型 handler（无具体终端依赖）
- CLI 实现 `PermissionPrompt` trait
- `agent_runner` 不受影响（继续使用 `YoloPermissionRequestHandler` 或 `PermissionManager`）

---

### 3.6 R-5：清理遗留 `AgentConnection` 抽象

#### 问题

`agent_abstraction::acp::connection` 中的 `AgentConnection` 是一个遗留的抽象层，封装了 `prompt_tx` 和 `cancel_tx`。但当前代码路径已经直接使用 `SessionHandles`（包含 `prompt_tx`、`cancel_tx`、`lifecycle_guard` 等），`AgentConnection` 没有实际消费者。

#### 方案

1. 搜索 `AgentConnection` 的所有引用
2. 如果确认无消费者，标记为 `#[deprecated]` 或直接删除
3. 如果仍有引用，将引用点迁移到 `SessionHandles`

#### 影响

- 减少 `agent_abstraction` 的公开面
- 降低新消费者的理解成本

---

## 4. 重构后的模块结构

### 4.1 重构后的 agent_abstraction 模块树

```
agent_abstraction/src/
├── lib.rs
├── acp/                           # 保留（清理遗留 AgentConnection）
│   ├── mod.rs
│   └── connection.rs              # [R-5] 清理或标记 deprecated
├── client/                        # [R-2] 新增
│   ├── mod.rs
│   ├── builder.rs                 # AcpClientBuilder
│   └── acp_client.rs              # AcpClient
├── diagnostics/                   # [R-3] 新增
│   ├── mod.rs
│   ├── types.rs                   # ProcessDiagnostics
│   └── listener.rs                # DiagnosticsListener trait
├── error/                         # 保留
├── launcher/                      # 保留
│   ├── mod.rs
│   ├── lifecycle.rs               # [R-3] 增加诊断回调点
│   ├── model_env.rs
│   └── claude_code_sacp/          # 保留（内部实现不变）
├── session/                       # 保留
├── traits/                        # [R-1][R-4] 扩展
│   ├── mod.rs
│   ├── agent.rs
│   ├── permission_handler.rs      # [R-4] 新增 InteractivePermissionHandler
│   ├── session_notifier.rs
│   └── session_registry.rs        # [R-1] 新增 update_agent_status 等方法
├── mirror_env/                    # 保留
└── path_env/                      # 保留
```

### 4.2 重构后的消费者使用方式

**agent_runner（不变）**：

```rust
// agent_runner 继续使用完整链路
let session_manager = AcpSessionManager::<StateAwareNotifier<R>, AgentSessionRegistry>::with_dependencies(
    Arc::new(StateAwareNotifier::new(registry.clone())),  // [R-1] 传入 registry
    AGENT_REGISTRY.clone(),
    model_env_resolver,
    PERMISSION_MANAGER.clone(),
);

let worker = AcpAgentWorker::new(session_manager);
let response = worker.process_request(worker_request).await?;
```

**rcoder-cli（简化）**：

```rust
// CLI 使用 AcpClientBuilder 便捷 API [R-2]
let client = AcpClientBuilder::new(
        TerminalSessionNotifier::new(),
        SimpleSessionRegistry::new(),
    )
    .command("python")
    .args(vec!["./my-agent.py".into()])
    .working_dir("/workspace")
    .diagnostics_listener(Arc::new(TerminalDiagnosticsListener::new()))  // [R-3]
    .start()
    .await?;

client.send_prompt("hello").await?;
client.stop().await?;
```

---

## 5. 执行顺序与依赖关系

```
         R-5 (清理遗留)
              │
              │ (独立，可并行)
              ▼
         R-1 (修复耦合) ──────→ agent_runner 回归测试
              │
              │ (R-2 依赖 R-1 的 trait 扩展)
              ▼
         R-2 (AcpClientBuilder) ──→ CLI 原型验证
              │
              │ (R-3 与 R-2 协同)
              ▼
         R-3 (诊断通道) ──────→ CLI 错误输出验证
              │
              │ (独立，可延后)
              ▼
         R-4 (交互权限) ──────→ CLI 交互模式验证
```

| 阶段 | 重构项 | 预计工作量 | 验证方式 |
|------|--------|-----------|---------|
| 第一阶段 | R-5 + R-1 | 1-2 天 | agent_runner 全量测试通过 |
| 第二阶段 | R-2 | 2-3 天 | CLI 原型可运行单次 prompt |
| 第三阶段 | R-3 | 1-2 天 | CLI 启动失败时输出诊断 |
| 第四阶段 | R-4 | 1 天 | CLI 交互模式权限确认 |

---

## 6. 风险与缓解

### 6.1 风险

| 风险 | 影响 | 概率 |
|------|------|------|
| R-1 的 trait 扩展导致 agent_runner 编译失败 | agent_runner 需要实现新方法 | 中（需要修改 `AgentSessionRegistry`） |
| R-2 的 `AcpClientBuilder` API 设计不满足 CLI 需求 | 需要返工 | 低（CLI 原型验证） |
| R-3 的诊断回调引入新的并发问题 | 死锁或数据竞争 | 低（回调是无状态的 listener） |

### 6.2 缓解策略

- **每个重构项独立 PR**：R-1 到 R-5 各自一个 PR，独立 review 和测试
- **先写 CLI 原型再重构**：用当前公开 API 写一个最小 CLI 原型，验证哪些地方确实需要重构，避免过度设计
- **agent_runner 回归测试**：每个 PR 必须跑通 `cargo test -p agent_runner` 和 `cargo test -p agent_abstraction`

---

## 7. 验收标准

1. **AC-1**：`StateAwareNotifier` 不再直接引用 `AGENT_REGISTRY` 静态变量（R-1）
2. **AC-2**：`AcpClientBuilder` API 可在 ~15 行代码内完成 agent 启动 + prompt 发送 + 停止（R-2）
3. **AC-3**：agent 启动失败时，`DiagnosticsListener` 能收到 exit code 和 stderr 内容（R-3）
4. **AC-4**：`agent_runner` 现有功能不受影响，全量测试通过（所有重构项）
5. **AC-5**：`rcoder-cli` 可以仅依赖 `agent_abstraction` 的公开 API 实现核心功能（R-2 + R-3）
