# Design Document

## Overview

ACP代理管理系统采用代理模式和消息传递架构来解决AgentSideConnection和ClientSideConnection不实现Send trait的问题。系统通过ProxyAgentManager作为中介，使用tokio::task::LocalSet隔离非Send逻辑，并通过MPSC通道实现与Axum HTTP处理器的安全通信。

## Architecture

### 高层架构

```
┌─────────────────┐    ┌──────────────────┐    ┌─────────────────┐
│   Axum HTTP     │    │  ProxyAgent      │    │   LocalSet      │
│   Handlers      │◄──►│  Manager         │◄──►│   Runtime       │
│   (Send)        │    │  (Send)          │    │   (Non-Send)    │
└─────────────────┘    └──────────────────┘    └─────────────────┘
                              │                          │
                              ▼                          ▼
                       ┌──────────────┐         ┌─────────────────┐
                       │   MPSC       │         │  ACP Agent      │
                       │   Channels   │         │  Services       │
                       └──────────────┘         │  (per project)  │
                                               └─────────────────┘
```

### 组件架构

```
ProxyAgentManager
├── AgentServiceRegistry (Arc<DashMap<ProjectId, AgentServiceHandle>>)
├── MessageDispatcher (MPSC Sender/Receiver)
├── LocalSetRuntime (tokio::task::LocalSet)
└── ServiceLifecycleManager

AgentServiceHandle
├── ProjectId
├── WorkspacePath
├── ServiceStatus
├── LastActivity
├── RequestChannel (MPSC Sender<AgentRequest>)
└── ResponseChannel (MPSC Receiver<AgentResponse>)

LocalSetRuntime
├── AgentSideConnection (Non-Send)
├── ClientSideConnection (Non-Send)
├── SessionManager
└── MessageProcessor
```

## Components and Interfaces

### 1. ProxyAgentManager

主要的代理管理器，负责协调所有Agent服务。

```rust
pub struct ProxyAgentManager {
    // Agent服务注册表
    service_registry: Arc<DashMap<String, AgentServiceHandle>>,
    
    // 消息分发通道
    request_sender: mpsc::UnboundedSender<ProxyRequest>,
    
    // LocalSet运行时句柄
    local_set_handle: tokio::task::JoinHandle<()>,
    
    // 配置
    config: ProxyConfig,
    
    // 生命周期管理器
    lifecycle_manager: Arc<ServiceLifecycleManager>,
}

impl ProxyAgentManager {
    pub async fn new(config: ProxyConfig) -> Result<Self>;
    pub async fn send_prompt(&self, project_id: &str, session_id: Option<&str>, prompt: &str) -> Result<(String, String)>; // 返回(response, session_id)
    pub async fn get_or_create_agent(&self, project_id: &str) -> Result<()>;
    pub async fn cleanup_idle_agents(&self) -> Result<()>;
    pub async fn shutdown(&self) -> Result<()>;
}
```

### 2. AgentServiceHandle

单个Agent服务的句柄，包含服务状态和通信通道。

```rust
pub struct AgentServiceHandle {
    pub project_id: String,
    pub workspace_path: PathBuf,
    pub status: AgentServiceStatus,
    pub last_activity: Instant,
    pub created_at: Instant,
    
    // 会话管理
    pub active_sessions: Arc<DashMap<String, SessionInfo>>,
    
    // 与LocalSet中Agent服务通信的通道
    request_tx: mpsc::UnboundedSender<AgentRequest>,
    response_rx: Arc<Mutex<mpsc::UnboundedReceiver<AgentResponse>>>,
    
    // 服务任务句柄
    service_task: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Debug, Clone)]
pub enum AgentServiceStatus {
    Initializing,
    Active,
    Idle,
    Error(String),
    Shutdown,
}
```

### 3. LocalSetAgentService

在LocalSet中运行的实际Agent服务，处理ACP协议通信。

```rust
pub struct LocalSetAgentService {
    project_id: String,
    workspace_path: PathBuf,
    
    // ACP连接（Non-Send）
    agent_connection: Option<AgentSideConnection>,
    client_connection: Option<ClientSideConnection>,
    
    // 会话管理
    session_manager: SessionManager,
    active_sessions: DashMap<String, AcpSessionId>,
    
    // 消息处理通道
    request_rx: mpsc::UnboundedReceiver<AgentRequest>,
    response_tx: mpsc::UnboundedSender<AgentResponse>,
}

impl LocalSetAgentService {
    pub async fn new(
        project_id: String,
        workspace_path: PathBuf,
        request_rx: mpsc::UnboundedReceiver<AgentRequest>,
        response_tx: mpsc::UnboundedSender<AgentResponse>,
    ) -> Result<Self>;
    
    pub async fn run(self) -> Result<()>;
    pub async fn handle_prompt(&mut self, session_id: Option<&str>, prompt: &str) -> Result<(String, String)>; // 返回(response, session_id)
    pub async fn initialize_connections(&mut self) -> Result<()>;
    
    // 内部方法，自动判断是new_session还是load_session
    async fn ensure_session(&mut self, session_id: Option<&str>) -> Result<String>;
    async fn create_new_session(&mut self) -> Result<String>;
    async fn load_existing_session(&mut self, session_id: &str) -> Result<()>;
}
```

### 4. 消息类型定义

```rust
#[derive(Debug)]
pub enum ProxyRequest {
    SendPrompt {
        project_id: String,
        session_id: Option<String>,
        prompt: String,
        response_tx: oneshot::Sender<Result<(String, String)>>, // (response, session_id)
    },
    CreateAgent {
        project_id: String,
        response_tx: oneshot::Sender<Result<()>>,
    },
    GetAgentStatus {
        project_id: String,
        response_tx: oneshot::Sender<Result<AgentServiceStatus>>,
    },
    Shutdown,
}

#[derive(Debug)]
pub enum AgentRequest {
    Initialize,
    Prompt {
        session_id: Option<String>,
        content: String,
        response_tx: oneshot::Sender<Result<(String, String)>>, // (response, session_id)
    },
    GetStatus,
    Shutdown,
}

#[derive(Debug)]
pub enum AgentResponse {
    Initialized,
    PromptResult(Result<(String, String)>), // (response, session_id)
    Status(AgentServiceStatus),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub acp_session_id: AcpSessionId,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_activity: chrono::DateTime<chrono::Utc>,
}
```

## Data Models

### 配置模型

```rust
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// 项目工作空间根目录
    pub workspace_root: PathBuf,
    
    /// Agent服务空闲超时时间（秒）
    pub idle_timeout: Duration,
    
    /// 清理检查间隔（秒）
    pub cleanup_interval: Duration,
    
    /// 最大并发Agent服务数量
    pub max_concurrent_agents: usize,
    
    /// LocalSet运行时配置
    pub local_set_config: LocalSetConfig,
}

#[derive(Debug, Clone)]
pub struct LocalSetConfig {
    /// 是否启用调试模式
    pub debug_mode: bool,
    
    /// 消息队列大小
    pub message_queue_size: usize,
    
    /// 连接超时时间
    pub connection_timeout: Duration,
}
```

### 项目工作空间模型

```rust
#[derive(Debug, Clone)]
pub struct ProjectWorkspace {
    pub project_id: String,
    pub workspace_path: PathBuf,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_accessed: chrono::DateTime<chrono::Utc>,
}

impl ProjectWorkspace {
    pub async fn create(project_id: &str, root_path: &Path) -> Result<Self>;
    pub async fn ensure_directory_structure(&self) -> Result<()>;
    pub fn get_project_path(&self) -> &Path;
}
```

## Error Handling

### 错误类型定义

```rust
#[derive(Debug, thiserror::Error)]
pub enum ProxyAgentError {
    #[error("Agent service not found for project: {project_id}")]
    AgentNotFound { project_id: String },
    
    #[error("Failed to create agent service: {source}")]
    AgentCreationFailed { source: Box<dyn std::error::Error + Send + Sync> },
    
    #[error("Agent service communication error: {message}")]
    CommunicationError { message: String },
    
    #[error("LocalSet runtime error: {source}")]
    LocalSetError { source: Box<dyn std::error::Error + Send + Sync> },
    
    #[error("Workspace creation failed: {path}")]
    WorkspaceError { path: PathBuf },
    
    #[error("Configuration error: {message}")]
    ConfigError { message: String },
    
    #[error("Timeout error: operation timed out after {duration:?}")]
    TimeoutError { duration: Duration },
}

pub type Result<T> = std::result::Result<T, ProxyAgentError>;
```

### 错误恢复策略

1. **Agent服务失败**: 自动重启Agent服务，最多重试3次
2. **通信超时**: 实现超时重试机制，指数退避
3. **工作空间错误**: 尝试重新创建目录结构
4. **LocalSet崩溃**: 重新启动LocalSet运行时
5. **资源耗尽**: 清理空闲服务，释放资源

## Testing Strategy

### 单元测试

1. **ProxyAgentManager测试**
   - Agent服务创建和管理
   - 消息路由和分发
   - 生命周期管理

2. **LocalSetAgentService测试**
   - ACP协议通信
   - 会话管理
   - 错误处理

3. **工作空间管理测试**
   - 目录创建和管理
   - 权限处理
   - 清理机制

### 集成测试

1. **端到端流程测试**
   - HTTP请求 → ProxyAgentManager → LocalSetAgentService → ACP协议
   - 多项目并发处理
   - 服务生命周期完整流程

2. **并发测试**
   - 多线程安全性
   - 竞态条件检测
   - 性能压力测试

3. **故障恢复测试**
   - Agent服务崩溃恢复
   - 网络中断处理
   - 资源耗尽场景

### 性能测试

1. **吞吐量测试**: 测试系统在高并发下的处理能力
2. **延迟测试**: 测试请求响应时间
3. **内存使用测试**: 监控内存泄漏和资源使用
4. **长期稳定性测试**: 长时间运行的稳定性验证

## Request Flow

### 首次请求流程（无session_id）

1. HTTP请求到达 `/chat` 端点，包含prompt和可选的project_id
2. 如果没有project_id，系统生成UUID（去掉中划线）作为project_id
3. 在 `./project_workspace/{project_id}` 创建工作目录
4. ProxyAgentManager检查是否存在对应的Agent服务
5. 如果不存在，创建新的LocalSetAgentService
6. 调用send_prompt(project_id, None, prompt)
7. LocalSetAgentService内部调用ensure_session(None)，自动执行ACP的new_session
8. 处理prompt并返回响应和新创建的session_id

### 后续请求流程（有session_id）

1. HTTP请求包含project_id和session_id
2. ProxyAgentManager路由到对应的Agent服务
3. 调用send_prompt(project_id, Some(session_id), prompt)
4. LocalSetAgentService内部调用ensure_session(Some(session_id))
5. 如果会话存在，直接使用；如果不存在，调用ACP的load_session
6. 处理prompt并返回响应和session_id

## Implementation Notes

### 线程安全考虑

1. **共享状态管理**: 使用Arc<DashMap>管理Agent服务注册表
2. **消息传递**: 使用tokio::sync::mpsc确保消息顺序
3. **生命周期同步**: 使用Arc<Mutex>保护关键资源

### 性能优化

1. **连接池**: 复用ACP连接，避免频繁创建销毁
2. **消息批处理**: 批量处理消息，减少上下文切换
3. **内存管理**: 及时清理不活跃的Agent服务
4. **异步优化**: 使用tokio的异步原语优化性能

### 可扩展性设计

1. **插件架构**: 支持不同类型的Agent实现
2. **配置驱动**: 通过配置文件调整系统行为
3. **监控接口**: 提供监控和管理接口
4. **水平扩展**: 支持分布式部署（未来扩展）