# SACP 协议升级技术方案

> 从官方 ACP (`agent-client-protocol`) 迁移到 Symposium ACP (`sacp`) 的技术方案设计

## 1. 背景与动机

### 1.1 当前问题

当前项目使用官方 ACP 协议库 `agent-client-protocol = "0.9.3"`，存在以下核心问题：

1. **Send trait 限制**：`ClientSideConnection` 未实现 `Send` trait，导致：
   - 必须在 `LocalSet` 中运行
   - 必须使用 `spawn_local` 而非标准的 `tokio::spawn`
   - 需要 `spawn_blocking` + 单线程运行时 + `LocalSet` 的复杂组合

2. **并发模型复杂**：
   ```rust
   // 当前代码模式（acp_agent.rs）
   tokio::task::spawn_blocking(move || {
       let rt = tokio::runtime::Builder::new_current_thread()
           .enable_all()
           .build()?;
       rt.block_on(async move {
           let local_set = tokio::task::LocalSet::new();
           local_set.run_until(async move {
               // ACP 连接处理必须在这里
           }).await
       })
   })
   ```

3. **代码耦合度高**：连接建立、消息处理、生命周期管理紧密耦合在 `ClaudeCodeLauncher` 中

### 1.2 SACP 优势

Symposium ACP (`sacp = "10.1.0"`) 提供：

1. **Send + 'static 支持**：`Component<L>` trait 要求类型满足 `Send + 'static`
2. **简化的并发模型**：可直接使用标准 Tokio 多线程运行时
3. **类型安全的链接系统**：`ClientToAgent`, `AgentToClient` 等编译时类型检查
4. **Builder 模式 API**：更简洁、更符合 Rust 习惯的 API 设计
5. **MCP 原生集成**：内置 MCP-over-ACP 支持

## 2. 架构设计

### 2.1 模块结构变化

```
crates/
├── agent_abstraction/           # 抽象层（需要重构）
│   ├── acp/
│   │   ├── mod.rs
│   │   ├── connection.rs        # AgentConnection（保留，调整内部实现）
│   │   └── sacp_adapter.rs      # 新增：SACP 适配器
│   ├── launcher/
│   │   ├── mod.rs
│   │   ├── lifecycle.rs         # AgentLifecycleGuard（保留）
│   │   ├── channel.rs           # 通道处理器（需要适配）
│   │   └── claude_code_sacp.rs  # 新增：SACP 版本启动器
│   └── session/
│       ├── worker.rs            # AgentWorker trait（保留）
│       └── acp_worker.rs        # AcpAgentWorker（需要适配）
│
├── agent_runner/                # 运行时（需要重构）
│   ├── proxy_agent/
│   │   ├── mod.rs
│   │   └── acp_agent.rs         # 移除 LocalSet 依赖
│   └── main.rs                  # 简化运行时架构
│
└── acp_adapter/                 # 新增：ACP 协议适配层
    ├── Cargo.toml
    ├── src/
    │   ├── lib.rs
    │   ├── traits.rs            # 协议无关的抽象 trait
    │   ├── sacp_impl.rs         # SACP 实现
    │   └── legacy_impl.rs       # 官方 ACP 兼容层（可选）
```

### 2.2 核心 Trait 设计

#### 2.2.1 协议无关的客户端抽象

```rust
// crates/acp_adapter/src/traits.rs

use async_trait::async_trait;
use std::path::PathBuf;

/// ACP 客户端抽象 trait
///
/// 这是协议无关的抽象层，支持 SACP 和官方 ACP 的切换
#[async_trait]
pub trait AcpClient: Send + Sync + 'static {
    /// 会话 ID 类型
    type SessionId: Clone + Send + Sync + std::fmt::Display;

    /// 错误类型
    type Error: std::error::Error + Send + Sync + 'static;

    /// 初始化连接
    async fn initialize(&self, client_info: ClientInfo) -> Result<InitializeResponse, Self::Error>;

    /// 创建新会话
    async fn new_session(&self, config: SessionConfig) -> Result<Self::SessionId, Self::Error>;

    /// 发送 Prompt
    async fn prompt(&self, session_id: &Self::SessionId, request: PromptRequest) -> Result<PromptResponse, Self::Error>;

    /// 取消会话
    async fn cancel(&self, session_id: &Self::SessionId) -> Result<(), Self::Error>;

    /// 检查连接是否有效
    fn is_connected(&self) -> bool;
}

/// 客户端信息
#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
    pub title: Option<String>,
}

/// 会话配置
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub working_directory: PathBuf,
    pub mcp_servers: Vec<McpServerConfig>,
    pub meta: Option<serde_json::Value>,
}

/// MCP 服务器配置
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Prompt 请求
#[derive(Debug, Clone)]
pub struct PromptRequest {
    pub messages: Vec<Message>,
    pub session_id: String,
}

/// Prompt 响应
#[derive(Debug, Clone)]
pub struct PromptResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
}
```

#### 2.2.2 SACP 实现

```rust
// crates/acp_adapter/src/sacp_impl.rs

use sacp::{
    ClientToAgent, Channel, JrConnectionCx, JrConnectionBuilder,
    schema::{InitializeRequest, ProtocolVersion, NewSessionRequest, SessionId},
    on_receive_request,
};
use std::sync::Arc;
use tokio::sync::RwLock;

/// SACP 客户端实现
///
/// 使用 SACP 的 Component trait 和 JrConnectionBuilder 构建
pub struct SacpClient {
    /// 连接上下文（线程安全）
    connection_cx: Arc<RwLock<Option<JrConnectionCx<ClientToAgent>>>>,
    /// 连接状态
    connected: Arc<std::sync::atomic::AtomicBool>,
}

impl SacpClient {
    /// 创建新客户端并连接到 Agent
    pub async fn connect(transport: impl sacp::Component<sacp::AgentToClient>) -> Result<Self, sacp::Error> {
        let connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let connected_clone = connected.clone();

        let (channel, server_future) = transport.into_server();

        // 在后台运行服务
        tokio::spawn(server_future);

        // 构建客户端连接
        let client = Self {
            connection_cx: Arc::new(RwLock::new(None)),
            connected,
        };

        Ok(client)
    }
}

#[async_trait]
impl AcpClient for SacpClient {
    type SessionId = SessionId;
    type Error = sacp::Error;

    async fn initialize(&self, client_info: ClientInfo) -> Result<InitializeResponse, Self::Error> {
        // 使用 SACP 的 InitializeRequest
        let request = InitializeRequest::new(ProtocolVersion::LATEST)
            .client_info(sacp::schema::Implementation::new(
                &client_info.name,
                &client_info.version,
            ));

        // 发送请求并等待响应
        // ...
        todo!()
    }

    async fn new_session(&self, config: SessionConfig) -> Result<Self::SessionId, Self::Error> {
        // 使用 SACP 的 NewSessionRequest
        let request = NewSessionRequest::new(config.working_directory);
        // ...
        todo!()
    }

    async fn prompt(&self, session_id: &Self::SessionId, request: PromptRequest) -> Result<PromptResponse, Self::Error> {
        // 使用 SACP 的 PromptRequest
        // ...
        todo!()
    }

    async fn cancel(&self, session_id: &Self::SessionId) -> Result<(), Self::Error> {
        // 使用 SACP 的 CancelNotification
        // ...
        todo!()
    }

    fn is_connected(&self) -> bool {
        self.connected.load(std::sync::atomic::Ordering::Relaxed)
    }
}
```

### 2.3 启动器重构

#### 2.3.1 SACP 版本启动器

```rust
// crates/agent_abstraction/src/launcher/claude_code_sacp.rs

use sacp::{ClientToAgent, ByteStreams, Component};
use std::process::Stdio;
use tokio::process::Command;

/// SACP 版本的 Claude Code 启动器
///
/// 关键变化：
/// 1. 不再需要 LocalSet
/// 2. 使用 SACP 的 Component trait
/// 3. 支持标准 Tokio spawn
pub struct SacpClaudeCodeLauncher<N: SessionNotifier> {
    notifier: Arc<N>,
}

impl<N: SessionNotifier + 'static> SacpClaudeCodeLauncher<N> {
    pub fn new(notifier: Arc<N>) -> Self {
        Self { notifier }
    }

    /// 启动 Claude Code Agent
    ///
    /// 与旧版本的关键区别：
    /// - 使用 `tokio::spawn` 而非 `spawn_local`
    /// - 使用 SACP 的 `ByteStreams` 作为传输层
    /// - 使用 `ClientToAgent::builder()` 构建连接
    pub async fn launch(
        &self,
        project_id: String,
        project_path: PathBuf,
        model_provider: Option<ModelProviderConfig>,
        start_config: AgentStartConfig,
    ) -> Result<SacpConnectionInfo> {
        // 1. 加载配置
        let agent_config = load_agent_config(model_provider.as_ref(), &start_config.service_type).await?;

        // 2. 启动子进程
        let mut child = Command::new(&agent_config.command)
            .args(&agent_config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .current_dir(&project_path)
            .envs(&agent_config.env)
            .spawn()?;

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        // 3. 创建 SACP 传输层（关键变化：ByteStreams 实现了 Component + Send）
        let transport = ByteStreams::new(stdout, stdin);

        // 4. 创建通道
        let (prompt_tx, prompt_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel_token = CancellationToken::new();

        // 5. 在标准 Tokio task 中运行（无需 LocalSet！）
        let notifier = self.notifier.clone();
        let cancel_token_clone = cancel_token.clone();

        let connection_handle = tokio::spawn(async move {
            Self::run_connection(
                transport,
                project_id,
                project_path,
                start_config,
                prompt_rx,
                cancel_rx,
                cancel_token_clone,
                notifier,
            ).await
        });

        // 6. 等待初始化完成并获取 session_id
        // ...

        Ok(SacpConnectionInfo {
            session_id,
            prompt_tx,
            cancel_tx,
            lifecycle_guard: Arc::new(lifecycle_guard),
        })
    }

    /// 运行 SACP 连接
    ///
    /// 使用 SACP 的 `ClientToAgent::builder().run_until()` 模式
    async fn run_connection(
        transport: impl Component<sacp::AgentToClient>,
        project_id: String,
        project_path: PathBuf,
        start_config: AgentStartConfig,
        mut prompt_rx: mpsc::UnboundedReceiver<PromptRequest>,
        mut cancel_rx: mpsc::UnboundedReceiver<CancelNotification>,
        cancel_token: CancellationToken,
        notifier: Arc<impl SessionNotifier>,
    ) -> Result<()> {
        ClientToAgent::builder()
            .name("rcoder-agent-runner")
            // 注册请求处理器（使用 SACP 宏）
            .on_receive_request(
                async |req: PermissionRequest, req_cx: JrRequestCx<_>, _| {
                    // 处理权限请求
                    req_cx.respond(PermissionResponse::default())
                },
                on_receive_request!(),
            )
            // 注册通知处理器
            .on_receive_notification(
                async |notif: SessionUpdate, _| {
                    // 处理会话更新通知
                    Ok(())
                },
                on_receive_notification!(),
            )
            // 运行连接
            .run_until(transport, async |cx| {
                // Step 1: 初始化
                cx.send_request(InitializeRequest::new(ProtocolVersion::LATEST))
                    .block_task()
                    .await?;

                // Step 2: 创建会话
                let meta = start_config.build_meta();
                let session = cx.build_session(NewSessionRequest::new(project_path).meta(meta))
                    .block_task()
                    .await?;

                let session_id = session.session_id();

                // Step 3: 处理消息循环
                loop {
                    tokio::select! {
                        // 处理 Prompt 请求
                        Some(prompt) = prompt_rx.recv() => {
                            session.send_prompt(&prompt.content)?;
                            let response = session.read_to_string().await?;
                            // 通知结果
                            notifier.notify_prompt_end(&project_id, session_id, StopReason::EndTurn, None, None).await?;
                        }
                        // 处理取消请求
                        Some(cancel) = cancel_rx.recv() => {
                            cx.send_notification(CancelNotification::new(session_id.clone()))?;
                        }
                        // 处理取消信号
                        _ = cancel_token.cancelled() => {
                            break;
                        }
                    }
                }

                Ok(())
            })
            .await
    }
}
```

### 2.4 Worker 简化

#### 2.4.1 移除 LocalSet 依赖

```rust
// crates/agent_runner/src/proxy_agent/acp_agent.rs

/// SACP 版本的 Agent Worker
///
/// 关键变化：移除了 LocalSet 和 spawn_blocking
pub async fn agent_worker_sacp(
    mut receiver: mpsc::UnboundedReceiver<AgentRequest>,
    handle: WorkerHandle,
) {
    while let Some(request) = receiver.recv().await {
        // 直接使用 tokio::spawn（无需 spawn_blocking + LocalSet）
        let handle = handle.clone();
        tokio::spawn(async move {
            let result = process_agent_request(request).await;
            // 处理结果
        });
    }
}

/// 处理单个 Agent 请求
///
/// 现在可以直接在标准 Tokio task 中运行
async fn process_agent_request(request: AgentRequest) -> Result<AgentResponse> {
    let worker = AcpAgentWorker::new(/* ... */);

    // 直接调用，无需 LocalSet
    worker.process_request(request.into()).await
}
```

### 2.5 连接信息结构

```rust
// crates/agent_abstraction/src/acp/connection.rs

/// SACP 版本的连接信息
///
/// 与旧版本兼容，但内部使用 SACP
#[derive(Debug)]
pub struct SacpConnectionInfo {
    /// 会话 ID
    pub session_id: sacp::schema::SessionId,
    /// Prompt 发送通道
    pub prompt_tx: mpsc::UnboundedSender<PromptRequest>,
    /// Cancel 发送通道
    pub cancel_tx: mpsc::UnboundedSender<CancelNotification>,
    /// 生命周期守卫
    pub lifecycle_guard: Arc<AgentLifecycleGuard>,
}

impl SacpConnectionInfo {
    /// 发送 Prompt（异步）
    pub async fn send_prompt(&self, request: PromptRequest) -> Result<()> {
        self.prompt_tx.send(request)?;
        Ok(())
    }

    /// 发送取消请求
    pub async fn send_cancel(&self, notification: CancelNotification) -> Result<()> {
        self.cancel_tx.send(notification)?;
        Ok(())
    }

    /// 检查通道是否关闭
    pub fn is_closed(&self) -> bool {
        self.prompt_tx.is_closed() || self.cancel_tx.is_closed()
    }
}
```

## 3. 迁移策略

### 3.1 分阶段迁移

#### 第一阶段：添加 SACP 依赖和适配层

1. 添加 `sacp = "10.1.0"` 依赖
2. 创建 `acp_adapter` crate，定义协议无关的抽象
3. 实现 SACP 适配器
4. 保留官方 ACP 实现作为 fallback

#### 第二阶段：重构启动器

1. 创建 `claude_code_sacp.rs`，实现 SACP 版本启动器
2. 通过 feature flag 控制使用哪个实现
3. 测试 SACP 版本的功能完整性

#### 第三阶段：移除 LocalSet 依赖

1. 修改 `acp_agent.rs`，移除 `spawn_blocking` + `LocalSet`
2. 使用标准 `tokio::spawn`
3. 简化 Worker 架构

#### 第四阶段：清理和优化

1. 移除官方 ACP 依赖（可选保留兼容层）
2. 优化连接管理
3. 添加单元测试和集成测试

### 3.2 Feature Flag 设计

```toml
# crates/agent_abstraction/Cargo.toml

[features]
default = ["sacp"]

# SACP 实现（新版本，推荐）
sacp = ["dep:sacp"]

# 官方 ACP 实现（兼容层）
legacy-acp = ["dep:agent-client-protocol"]

[dependencies]
# SACP 依赖（可选）
sacp = { version = "10.1.0", optional = true }

# 官方 ACP 依赖（可选，保留兼容）
agent-client-protocol = { version = "0.9.3", features = ["unstable"], optional = true }
```

### 3.3 运行时切换

```rust
// crates/agent_abstraction/src/launcher/mod.rs

#[cfg(feature = "sacp")]
pub use claude_code_sacp::SacpClaudeCodeLauncher as ClaudeCodeLauncher;

#[cfg(all(feature = "legacy-acp", not(feature = "sacp")))]
pub use claude_code::ClaudeCodeLauncher;

/// 创建默认启动器
pub fn create_launcher<N: SessionNotifier + 'static>(
    notifier: Arc<N>,
) -> impl AgentLauncher {
    #[cfg(feature = "sacp")]
    {
        SacpClaudeCodeLauncher::new(notifier)
    }

    #[cfg(all(feature = "legacy-acp", not(feature = "sacp")))]
    {
        ClaudeCodeLauncher::new(notifier)
    }
}
```

## 4. 关键数据结构对照

### 4.1 消息类型映射

| 官方 ACP | SACP | 说明 |
|---------|------|------|
| `InitializeRequest` | `sacp::schema::InitializeRequest` | 初始化请求 |
| `NewSessionRequest` | `sacp::schema::NewSessionRequest` | 创建会话 |
| `PromptRequest` | `sacp::schema::PromptRequest` | Prompt 请求 |
| `CancelNotification` | `sacp::schema::CancelNotification` | 取消通知 |
| `SessionId` | `sacp::schema::SessionId` | 会话 ID |
| `StopReason` | `sacp::schema::StopReason` | 停止原因 |

### 4.2 连接类型映射

| 官方 ACP | SACP | 说明 |
|---------|------|------|
| `ClientSideConnection` | `ClientToAgent::builder()` | 客户端连接 |
| `AgentSideConnection` | `AgentToClient::builder()` | Agent 连接 |
| N/A | `ByteStreams` | stdio 传输层 |
| N/A | `Channel` | 进程内通道 |

### 4.3 Trait 映射

| 官方 ACP | SACP | 说明 |
|---------|------|------|
| `Client` | `Component<AgentToClient>` | 客户端组件 |
| `Agent` | `Component<ClientToAgent>` | Agent 组件 |
| N/A | `JrRequest` | 请求 trait |
| N/A | `JrNotification` | 通知 trait |
| N/A | `JrResponsePayload` | 响应 trait |

## 5. 注意事项

### 5.1 兼容性

1. **SessionId 类型**：SACP 的 `SessionId` 与官方 ACP 基本兼容，但需要适配
2. **Meta 字段**：`_meta.claudeCode.options.resume` 等字段格式需要保持一致
3. **MCP 服务器配置**：SACP 内置 MCP 支持，配置方式略有不同

### 5.2 错误处理

1. **SACP 错误类型**：使用 `sacp::Error` 替代 `anyhow::Error`
2. **错误码**：确保错误码与现有 gRPC 接口兼容
3. **降级处理**：Resume 失败时的降级逻辑需要在新架构中重新实现

### 5.3 性能考量

1. **移除 LocalSet 开销**：预期减少线程切换和上下文开销
2. **连接池**：考虑实现 SACP 连接池，复用连接
3. **消息序列化**：SACP 使用 `jsonrpcmsg`，性能与官方 ACP 相当

### 5.4 测试策略

1. **单元测试**：使用 SACP 的 `sacp-test` crate
2. **集成测试**：使用 `Channel::duplex()` 进行进程内测试
3. **E2E 测试**：验证与 Claude Code 子进程的实际通信

## 6. 依赖变更

### 6.1 移除的依赖

```toml
# 移除（或标记为 optional）
agent-client-protocol = { version = "0.9.3", features = ["unstable"] }
```

### 6.2 新增的依赖

```toml
# 新增
sacp = "10.1.0"
sacp-tokio = "10.1.0"  # 可选：用于 AcpAgent 进程管理

# 间接依赖（通过 sacp）
agent-client-protocol-schema = "0.9.3"  # Schema 类型定义
jsonrpcmsg = "..."  # JSON-RPC 消息
```

### 6.3 本地依赖（开发阶段）

```toml
# 使用 vendors 目录的本地源码（便于调试）
sacp = { path = "../vendors/symposium-acp/src/sacp" }
```

## 7. 时间线（建议）

| 阶段 | 内容 | 风险 |
|-----|------|------|
| 阶段一 | 添加适配层，保持双实现 | 低 |
| 阶段二 | 实现 SACP 启动器 | 中 |
| 阶段三 | 移除 LocalSet，简化架构 | 中 |
| 阶段四 | 清理旧代码，完善测试 | 低 |

## 8. 回滚方案

如果迁移过程中遇到严重问题：

1. 通过 feature flag 快速切回官方 ACP 实现
2. 保留 `legacy-acp` feature 作为长期回退选项
3. 使用条件编译隔离新旧代码

```toml
# 回滚：禁用 SACP，启用官方 ACP
[features]
default = ["legacy-acp"]  # 改为默认使用旧实现
```

## 9. 参考资料

- SACP 官方仓库：https://github.com/symposium-dev/symposium-acp
- SACP 文档：`vendors/symposium-acp/md/` 目录
- 官方 ACP 协议：https://agentclientprotocol.com/
- 项目本地源码：`vendors/symposium-acp/src/sacp/`
