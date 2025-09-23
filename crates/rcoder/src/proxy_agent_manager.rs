//! ACP Proxy Agent Manager
//!
//! 这个模块实现了一个代理管理器，用于解决 ACP 连接的 Send trait 问题。
//!
//! 主要特性：
//! - 使用 tokio LocalSet 隔离非 Send 的 ACP 连接
//! - 通过 MPSC 通道进行跨线程通信
//! - 支持动态创建和管理 Agent 服务
//! - 提供项目工作空间管理

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{info, warn, error, debug};
use uuid::Uuid;
use piper;

use agent_client_protocol as acp;
use agent_client_protocol::{
    Agent, AgentSideConnection, ClientSideConnection, InitializeRequest
};
use anyhow::{anyhow, Result as AnyhowResult};
use codex_acp_agent::{CodexAgent, Config};

// ============================================================================
// Core Types and Errors
// ============================================================================

/// 代理管理器配置
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// 工作空间根目录
    pub workspace_root: PathBuf,
    /// 空闲超时时间（秒）
    pub idle_timeout: u64,
    /// 清理间隔（秒）
    pub cleanup_interval: u64,
    /// 最大并发 Agent 数量
    pub max_concurrent_agents: usize,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            workspace_root: PathBuf::from("./project_workspace"),
            idle_timeout: 3600, // 1小时
            cleanup_interval: 300, // 5分钟
            max_concurrent_agents: 10,
        }
    }
}

/// Agent 服务状态
#[derive(Debug, Clone, PartialEq)]
pub enum AgentServiceStatus {
    /// 已创建
    Created,
    /// 正在运行
    Active,
    /// 已停止
    Stopped,
    /// 错误状态
    Error(String),
}

/// 代理管理器错误类型
#[derive(Debug, thiserror::Error)]
pub enum ProxyAgentError {
    #[error("工作空间错误: {path:?}")]
    WorkspaceError { path: PathBuf },
    #[error("IO错误: {0}")]
    IoError(#[from] std::io::Error),
    #[error("配置错误: {message}")]
    ConfigError { message: String },
    #[error("ACP 协议错误: {message}")]
    AcpError { message: String },
    #[error("会话未找到: {session_id}")]
    SessionNotFound { session_id: String },
    #[error("服务不可用: {message}")]
    ServiceUnavailable { message: String },
    #[error("无效的项目ID: {0}")]
    InvalidProjectId(String),
}

pub type ProxyResult<T> = Result<T, ProxyAgentError>;

// ============================================================================
// Message Types
// ============================================================================

/// 代理请求类型
#[derive(Debug)]
pub enum ProxyRequest {
    /// 发送 prompt
    SendPrompt {
        project_id: String,
        session_id: Option<String>,
        prompt: String,
        response_tx: oneshot::Sender<ProxyResult<(String, String)>>,
    },
}

/// Agent 请求类型
#[derive(Debug)]
pub enum AgentRequest {
    /// 初始化
    Initialize,
    /// 处理 prompt - 内部处理 session_id 逻辑
    Prompt {
        session_id: Option<String>,
        content: String,
        response_tx: oneshot::Sender<ProxyResult<(String, String)>>,
    },
    /// 获取状态
    GetStatus {
        response_tx: oneshot::Sender<AgentServiceStatus>,
    },
    /// 关闭
    Shutdown,
}

/// Agent 响应类型
#[derive(Debug)]
pub enum AgentResponse {
    /// 已初始化
    Initialized,
    /// Prompt 处理结果
    PromptResult(ProxyResult<(String, String)>),
    /// 状态响应
    Status(AgentServiceStatus),
}

// ============================================================================
// ACP Client Implementation
// ============================================================================

/// ACP 客户端连接包装器
pub struct AcpClientConnection {
    /// ACP 客户端连接
    connection: acp::ClientSideConnection,
    /// 当前会话 ID
    session_id: Option<String>,
    /// 消息发送器（用于转发 session_notification 消息到主线程）
    message_tx: Option<mpsc::UnboundedSender<(String, acp::SessionNotification)>>,
}

/// 代理管理器的 ACP 客户端实现
#[derive(Clone)]
pub struct ProxyAcpClient {
    /// 项目 ID
    project_id: String,
    /// 工作空间路径
    workspace_path: PathBuf,
}

#[async_trait::async_trait(?Send)]
impl acp::Client for ProxyAcpClient {
    async fn request_permission(
        &self,
        _args: acp::RequestPermissionRequest,
    ) -> AnyhowResult<acp::RequestPermissionResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn write_text_file(
        &self,
        _args: acp::WriteTextFileRequest,
    ) -> AnyhowResult<acp::WriteTextFileResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn read_text_file(
        &self,
        _args: acp::ReadTextFileRequest,
    ) -> AnyhowResult<acp::ReadTextFileResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn create_terminal(
        &self,
        _args: acp::CreateTerminalRequest,
    ) -> AnyhowResult<acp::CreateTerminalResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn kill_terminal_command(
        &self,
        _args: acp::KillTerminalCommandRequest,
    ) -> AnyhowResult<acp::KillTerminalCommandResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn release_terminal(
        &self,
        _args: acp::ReleaseTerminalRequest,
    ) -> AnyhowResult<acp::ReleaseTerminalResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _args: acp::WaitForTerminalExitRequest,
    ) -> AnyhowResult<acp::WaitForTerminalExitResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn session_notification(
        &self,
        notification: acp::SessionNotification,
    ) -> AnyhowResult<(), acp::Error> {
        // 处理会话通知，记录日志
        match &notification.update {
            acp::SessionUpdate::AgentMessageChunk { content } => {
                debug!("Received agent message for session {}: {:?}", notification.session_id, content);
            }
            acp::SessionUpdate::UserMessageChunk { content } => {
                debug!("Received user message for session {}: {:?}", notification.session_id, content);
            }
            acp::SessionUpdate::AgentThoughtChunk { content } => {
                debug!("Received agent thought for session {}: {:?}", notification.session_id, content);
            }
            acp::SessionUpdate::ToolCall(tool_call) => {
                debug!("Received tool call for session {}: {:?}", notification.session_id, tool_call);
            }
            acp::SessionUpdate::ToolCallUpdate(tool_call_update) => {
                debug!("Received tool call update for session {}: {:?}", notification.session_id, tool_call_update);
            }
            acp::SessionUpdate::Plan(plan) => {
                debug!("Received plan for session {}: {:?}", notification.session_id, plan);
            }
            acp::SessionUpdate::CurrentModeUpdate { current_mode_id } => {
                debug!("Received mode update for session {}: {:?}", notification.session_id, current_mode_id);
            }
            acp::SessionUpdate::AvailableCommandsUpdate { available_commands } => {
                debug!("Received commands update for session {}: {:?}", notification.session_id, available_commands);
            }
        }
        Ok(())
    }

    async fn terminal_output(
        &self,
        _args: acp::TerminalOutputRequest,
    ) -> AnyhowResult<acp::TerminalOutputResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_method(&self, _args: acp::ExtRequest) -> AnyhowResult<acp::ExtResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_notification(&self, _args: acp::ExtNotification) -> AnyhowResult<(), acp::Error> {
        Err(acp::Error::method_not_found())
    }
}

impl AcpClientConnection {
    /// 创建新的 ACP 客户端连接 - 使用嵌入式 Agent 和 piper 管道
    pub async fn new(project_id: String, workspace_path: PathBuf) -> ProxyResult<Self> {
        info!("🔧 Creating embedded ACP connection for project: {}", project_id);

        // 创建项目目录
        if !workspace_path.exists() {
            tokio::fs::create_dir_all(&workspace_path).await
                .map_err(|e| ProxyAgentError::ServiceUnavailable {
                    message: format!("Failed to create workspace directory: {}", e),
                })?;
            info!("📁 Created workspace directory: {:?}", workspace_path);
        }

        // 使用 piper 创建双向管道
        let (client_to_agent_rx, client_to_agent_tx) = piper::pipe(1024);
        let (agent_to_client_rx, agent_to_client_tx) = piper::pipe(1024);

        // 创建会话更新通道
        let (session_update_tx, _session_update_rx) = mpsc::unbounded_channel();

        // 创建客户端操作通道
        let (client_tx, _client_rx) = mpsc::unbounded_channel();

        // 加载配置
        let config = Config::load_from_base_config_with_overrides(
            Default::default(),
            codex_core::config::ConfigOverrides::default(),
            workspace_path.clone(),
        ).map_err(|e| {
            error!("Failed to load config: {}", e);
            ProxyAgentError::ServiceUnavailable {
                message: format!("Failed to load config: {}", e),
            }
        })?;

        // 创建 CodexAgent 实例
        let agent = CodexAgent::with_config(
            session_update_tx.clone(),
            client_tx.clone(),
            config,
        );

        // 创建 ACP 客户端
        let proxy_client = ProxyAcpClient {
            project_id: project_id.clone(),
            workspace_path: workspace_path.clone(),
        };

        // 使用 LocalSet 来运行嵌入式 agent
        let local_set = tokio::task::LocalSet::new();

        // 创建使用 piper 管道的 AgentSideConnection
        let (_server_conn, server_handle_io) = AgentSideConnection::new(
            agent,
            client_to_agent_tx,  // agent 接收来自 client 的数据
            agent_to_client_rx,  // agent 发送数据给 client
            move |fut| {
                tokio::task::spawn_local(fut);
            }
        );

        // 创建使用 piper 管道的 ClientSideConnection
        let (client_conn, client_handle_io) = ClientSideConnection::new(
            proxy_client,
            agent_to_client_tx,  // client 接收来自 agent 的数据
            client_to_agent_rx,  // client 发送数据给 agent
            move |fut| {
                tokio::task::spawn_local(fut);
            }
        );

        // 使用 LocalSet 运行 IO 任务和初始化
        let result = local_set.run_until(async move {
            // 启动服务端 IO 处理任务
            let _server_io_handle = tokio::task::spawn_local(server_handle_io);

            // 启动客户端 IO 处理任务
            let _client_io_handle = tokio::task::spawn_local(client_handle_io);

            // 初始化连接
            client_conn.initialize(InitializeRequest {
                protocol_version: acp::V1,
                client_capabilities: acp::ClientCapabilities::default(),
                meta: None,
            }).await
                .map_err(|e| {
                    error!("Failed to initialize ACP connection: {}", e);
                    ProxyAgentError::ServiceUnavailable {
                        message: format!("Failed to initialize ACP connection: {}", e),
                    }
                })?;

            Ok(client_conn)
        }).await;

        match result {
            Ok(connection) => {
                info!("✅ Embedded ACP connection established for project: {}", project_id);
                Ok(Self {
                    connection,
                    session_id: None,
                    message_tx: None,
                })
            }
            Err(e) => {
                error!("❌ Failed to establish embedded ACP connection for project {}: {}", project_id, e);
                Err(e)
            }
        }
    }

    /// 设置会话 ID
    pub fn set_session_id(&mut self, session_id: String) {
        self.session_id = Some(session_id);
    }

    /// 获取客户端的可变引用以设置会话 ID
    pub fn get_client_mut(&mut self) -> Option<&mut ProxyAcpClient> {
        // 这是一个复杂的操作，因为我们无法直接访问内部的 client
        // 在实际应用中，可能需要重新设计结构或使用其他方法
        None
    }
}

impl AcpClientConnection {
    /// 设置消息发送器
    pub fn set_message_sender(&mut self, message_tx: mpsc::UnboundedSender<(String, acp::SessionNotification)>) {
        self.message_tx = Some(message_tx);
    }

    /// 创建新会话
    pub async fn new_session(&mut self) -> ProxyResult<String> {
        let response = self
            .connection
            .new_session(acp::NewSessionRequest {
                mcp_servers: Vec::new(),
                cwd: std::env::current_dir().unwrap_or_default(),
                meta: None,
            })
            .await
            .map_err(|e| ProxyAgentError::ServiceUnavailable {
                message: format!("Failed to create session: {}", e),
            })?;

        let session_id = response.session_id.to_string();
        self.session_id = Some(session_id.clone());

        info!("Created new session: {}", session_id);
        Ok(session_id)
    }

    /// 加载现有会话
    pub async fn load_session(&mut self, session_id: &str) -> ProxyResult<String> {
        let response = self
            .connection
            .load_session(acp::LoadSessionRequest {
                session_id: acp::SessionId(session_id.into()),
                mcp_servers: Vec::new(),
                cwd: std::env::current_dir().unwrap_or_default(),
                meta: None,
            })
            .await
            .map_err(|e| ProxyAgentError::ServiceUnavailable {
                message: format!("Failed to load session: {}", e),
            })?;

        // 加载成功，更新内部 session_id
        self.session_id = Some(session_id.to_string());
        info!("Loaded existing session: {}", session_id);
        Ok(session_id.to_string())
    }

    /// 发送提示
    pub async fn send_prompt(&mut self, prompt: &str) -> ProxyResult<String> {
        let session_id = if let Some(ref id) = self.session_id {
            id.clone()
        } else {
            // 如果没有会话，创建一个
            self.new_session().await?
        };

        // 将提示文本转换为 ContentBlock
        let content_blocks = vec![acp::ContentBlock::Text(acp::TextContent {
            annotations: None,
            text: prompt.to_string(),
            meta: None,
        })];

        let mut response_text = String::new();

        // 发送提示
        let prompt_response = self
            .connection
            .prompt(acp::PromptRequest {
                session_id: acp::SessionId(session_id.clone().into()),
                prompt: content_blocks,
                meta: None,
            })
            .await
            .map_err(|e| ProxyAgentError::ServiceUnavailable {
                message: format!("Failed to send prompt: {}", e),
            })?;

        // 简化处理：将响应转换为文本
        // 在实际的实现中，可能需要更复杂的响应处理
        response_text = format!("ACP response received for prompt: {}", prompt);
        Ok(response_text)
    }

    /// 获取当前会话 ID
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

// ============================================================================
// Service Handles
// ============================================================================

/// Agent 服务句柄
#[derive(Debug)]
pub struct AgentServiceHandle {
    /// 项目ID
    pub project_id: String,
    /// 工作空间路径
    pub workspace_path: PathBuf,
    /// 请求发送器
    request_tx: mpsc::UnboundedSender<AgentRequest>,
    /// 服务任务句柄
    service_task: Option<JoinHandle<()>>,
    /// 创建时间
    pub created_at: Instant,
    /// 最后活动时间
    pub last_activity: Instant,
    /// 服务状态
    pub status: AgentServiceStatus,
    /// 当前会话ID
    session_id: Option<String>,
}

impl AgentServiceHandle {
    /// 创建新的服务句柄
    pub fn new(
        project_id: String,
        workspace_path: PathBuf,
        request_tx: mpsc::UnboundedSender<AgentRequest>,
    ) -> Self {
        Self {
            project_id,
            workspace_path,
            request_tx,
            service_task: None,
            created_at: Instant::now(),
            last_activity: Instant::now(),
            status: AgentServiceStatus::Created,
            session_id: None,
        }
    }

    /// 更新状态
    pub fn update_status(&mut self, status: AgentServiceStatus) {
        self.status = status;
        self.last_activity = Instant::now();
    }

    /// 更新活动时间
    pub fn update_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    /// 获取当前会话ID
    pub fn get_session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// 设置会话ID
    pub fn set_session_id(&mut self, session_id: String) {
        self.session_id = Some(session_id);
        self.last_activity = Instant::now();
    }

    /// 清除会话ID
    pub fn clear_session_id(&mut self) {
        self.session_id = None;
    }

    /// 检查是否空闲
    pub fn is_idle(&self, timeout: Duration) -> bool {
        self.last_activity.elapsed() > timeout
    }

    /// 发送请求
    pub fn send_request(&self, request: AgentRequest) -> ProxyResult<()> {
        self.request_tx.send(request)
            .map_err(|_| ProxyAgentError::ServiceUnavailable {
                message: "Agent service not available".to_string(),
            })?;
        Ok(())
    }

    /// 停止服务
    pub async fn shutdown(&mut self) -> ProxyResult<()> {
        // 发送关闭信号
        let _ = self.send_request(AgentRequest::Shutdown);

        // 等待任务结束
        if let Some(task) = self.service_task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        }

        self.update_status(AgentServiceStatus::Stopped);
        Ok(())
    }
}

/// 项目工作空间
#[derive(Debug, Clone)]
pub struct ProjectWorkspace {
    /// 项目ID
    pub project_id: String,
    /// 工作空间路径
    pub workspace_path: PathBuf,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// 最后访问时间
    pub last_accessed: DateTime<Utc>,
}

impl ProjectWorkspace {
    /// 创建新的项目工作空间
    pub async fn new(workspace_root: &PathBuf, project_id: &str) -> ProxyResult<Self> {
        let workspace_path = workspace_root.join(project_id);

        // 创建工作空间目录
        tokio::fs::create_dir_all(&workspace_path).await
            .map_err(|_| ProxyAgentError::WorkspaceError {
                path: workspace_path.clone(),
            })?;

        Ok(Self {
            project_id: project_id.to_string(),
            workspace_path,
            created_at: Utc::now(),
            last_accessed: Utc::now(),
        })
    }

    /// 获取项目路径
    pub fn get_project_path(&self) -> &std::path::Path {
        &self.workspace_path
    }

    /// 更新访问时间
    pub fn update_access(&mut self) {
        self.last_accessed = Utc::now();
    }

    /// 检查工作空间是否存在
    pub fn exists(&self) -> bool {
        self.workspace_path.exists()
    }

    /// 获取工作空间大小（字节）
    pub async fn get_size(&self) -> ProxyResult<u64> {
        if !self.exists() {
            return Ok(0);
        }

        let mut total_size = 0u64;
        let mut entries = tokio::fs::read_dir(&self.workspace_path).await?;

        while let Some(entry) = entries.next_entry().await? {
            let metadata = entry.metadata().await?;
            if metadata.is_file() {
                total_size += metadata.len();
            }
        }

        Ok(total_size)
    }
}

// ============================================================================
// ProxyAgentManager Implementation
// ============================================================================

/// ACP 代理管理器
#[derive(Debug)]
pub struct ProxyAgentManager {
    /// 服务注册表
    service_registry: Arc<DashMap<String, AgentServiceHandle>>,
    /// 请求发送器
    request_tx: mpsc::UnboundedSender<ProxyRequest>,
    /// 配置
    config: ProxyConfig,
    /// 项目工作空间
    workspaces: Arc<DashMap<String, ProjectWorkspace>>,
}

impl ProxyAgentManager {
    /// 创建新的代理管理器
    pub async fn new(config: ProxyConfig) -> ProxyResult<(Self, mpsc::UnboundedReceiver<ProxyRequest>)> {
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let service_registry = Arc::new(DashMap::new());
        let workspaces = Arc::new(DashMap::new());

        // 创建管理器实例
        let manager = Self {
            service_registry: service_registry.clone(),
            request_tx: request_tx.clone(),
            config: config.clone(),
            workspaces,
        };

        // 启动清理任务
        manager.start_cleanup_task().await;

        info!("ProxyAgentManager created successfully");
        Ok((manager, request_rx))
    }

  
    /// 运行消息分发器
    pub async fn run_message_dispatcher(
        mut request_rx: mpsc::UnboundedReceiver<ProxyRequest>,
        service_registry: Arc<DashMap<String, AgentServiceHandle>>,
        workspaces: Arc<DashMap<String, ProjectWorkspace>>,
        config: ProxyConfig,
    ) -> ProxyResult<()> {
        while let Some(request) = request_rx.recv().await {
            match request {
                ProxyRequest::SendPrompt {
                    project_id,
                    session_id,
                    prompt,
                    response_tx,
                } => {
                    debug!("Dispatching prompt request to project: {} (session: {:?})", project_id, session_id);

                    // 如果服务不存在，先创建服务
                    if !service_registry.contains_key(&project_id) {
                        info!("Creating agent service for project: {}", project_id);
                        if let Err(e) = Self::create_agent_service_static(&project_id, &workspaces, &service_registry, &config).await {
                            let _ = response_tx.send(Err(e));
                            continue;
                        }
                    }

                    // 发送 prompt 请求，session_id 管理逻辑在 AgentService 内部处理
                    if let Some(service_handle) = service_registry.get(&project_id) {
                        let (result_tx, result_rx) = oneshot::channel();

                        if let Err(e) = service_handle.send_request(AgentRequest::Prompt {
                            session_id: session_id.clone(),
                            content: prompt.clone(),
                            response_tx: result_tx,
                        }) {
                            let _ = response_tx.send(Err(e));
                            continue;
                        }

                        // 等待响应，返回 (project_id, session_id)
                        match result_rx.await {
                            Ok(result) => {
                                let _ = response_tx.send(result);
                            }
                            Err(_) => {
                                let _ = response_tx.send(Err(ProxyAgentError::ServiceUnavailable {
                                    message: "Agent service communication failed".to_string(),
                                }));
                            }
                        }
                    } else {
                        let _ = response_tx.send(Err(ProxyAgentError::ServiceUnavailable {
                            message: format!("Agent service not found for project: {}", project_id),
                        }));
                    }
                }
            }
        }

        Ok(())
    }

    /// 启动清理任务
    async fn start_cleanup_task(&self) {
        let service_registry = self.service_registry.clone();
        let workspaces = self.workspaces.clone();
        let idle_timeout = Duration::from_secs(self.config.idle_timeout);
        let cleanup_interval = self.config.cleanup_interval;

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(cleanup_interval));

            loop {
                interval.tick().await;
                debug!("Running cleanup task for idle agents");

                let mut services_to_remove = Vec::new();

                // 检查空闲服务
                for entry in service_registry.iter() {
                    let project_id = entry.project_id.clone();
                    if entry.is_idle(idle_timeout) {
                        info!("Agent service {} is idle, scheduling for removal", project_id);
                        services_to_remove.push(project_id);
                    }
                }

                // 移除空闲服务
                for project_id in services_to_remove {
                    if let Some((_, mut service_handle)) = service_registry.remove(&project_id) {
                        if let Err(e) = service_handle.shutdown().await {
                            warn!("Failed to shutdown agent service {}: {}", project_id, e);
                        }
                    }
                }

                // 清理过期工作空间
                workspaces.retain(|_, workspace| {
                    workspace.workspace_path.exists()
                });
            }
        });
    }

    
    /// 静态版本的服务创建方法（用于线程间调用）
    async fn create_agent_service_static(
        project_id: &str,
        workspaces: &Arc<DashMap<String, ProjectWorkspace>>,
        service_registry: &Arc<DashMap<String, AgentServiceHandle>>,
        config: &ProxyConfig,
    ) -> ProxyResult<()> {
       

        // 创建或获取工作空间
        let workspace = if let Some(existing) = workspaces.get(project_id) {
            existing.clone()
        } else {
            let workspace = ProjectWorkspace::new(&config.workspace_root, project_id).await?;
            workspaces.insert(project_id.to_string(), workspace.clone());
            workspace
        };

        info!("Created project workspace: {:?} for project: {}", workspace.workspace_path, project_id);

        // 创建 LocalSetAgentService
        let service = LocalSetAgentService::new(
            project_id.to_string(),
            workspace.workspace_path.clone(),
        ).await?;

        // 创建 AgentServiceHandle
        let handle = AgentServiceHandle::new(
            project_id.to_string(),
            workspace.workspace_path,
            service.request_tx.clone(),
        );

        // 注册服务
        service_registry.insert(project_id.to_string(), handle);
        info!("Created and registered agent service for project: {}", project_id);

        Ok(())
    }

    /// 发送 prompt 请求
    pub async fn send_prompt(
        &self,
        project_id: &str,
        session_id: Option<&str>,
        prompt: &str,
    ) -> ProxyResult<(String, String)> {
        info!("Sending prompt to project {} (session: {:?}): {}", project_id, session_id, prompt);

          // 发送请求（服务将在消息分发器中按需创建）
        let (response_tx, response_rx) = oneshot::channel();
        let request = ProxyRequest::SendPrompt {
            project_id: project_id.to_string(),
            session_id: session_id.map(|s| s.to_string()),
            prompt: prompt.to_string(),
            response_tx,
        };

        self.request_tx.send(request)
            .map_err(|_| ProxyAgentError::ServiceUnavailable {
                message: "Failed to send prompt request".to_string(),
            })?;

        match response_rx.await {
            Ok(result) => result,
            Err(_) => Err(ProxyAgentError::ServiceUnavailable {
                message: "Prompt request timed out".to_string(),
            }),
        }
    }

    /// 获取服务状态
    pub fn get_service_status(&self, project_id: &str) -> ProxyResult<AgentServiceStatus> {
        if let Some(service_handle) = self.service_registry.get(project_id) {
            Ok(service_handle.status.clone())
        } else {
            Err(ProxyAgentError::ServiceUnavailable {
                message: format!("Agent service not found for project: {}", project_id),
            })
        }
    }

    /// 停止服务
    pub async fn stop_service(&self, project_id: &str) -> ProxyResult<()> {
        if let Some((_, mut service_handle)) = self.service_registry.remove(project_id) {
            service_handle.shutdown().await
        } else {
            Err(ProxyAgentError::ServiceUnavailable {
                message: format!("Agent service not found for project: {}", project_id),
            })
        }
    }

    /// 获取所有活跃的服务
    pub fn get_active_services(&self) -> Vec<(String, AgentServiceStatus)> {
        self.service_registry
            .iter()
            .map(|entry| (entry.project_id.clone(), entry.status.clone()))
            .collect()
    }

    /// 获取所有工作空间路径
    pub fn get_workspace_paths(&self) -> Vec<(String, PathBuf)> {
        self.workspaces
            .iter()
            .map(|entry| (entry.project_id.clone(), entry.workspace_path.clone()))
            .collect()
    }

    /// 关闭代理管理器
    pub async fn shutdown(&self) -> ProxyResult<()> {
        info!("Shutting down proxy agent manager");

        // 停止所有代理服务
        let services: Vec<String> = self.service_registry
            .iter()
            .map(|entry| entry.project_id.clone())
            .collect();

        for project_id in services {
            if let Err(e) = self.stop_service(&project_id).await {
                warn!("Failed to stop service {}: {}", project_id, e);
            }
        }

        info!("Proxy agent manager shutdown complete");
        Ok(())
    }
}

impl ProxyAgentManager {
    /// 获取服务注册表的引用
    pub fn get_service_registry(&self) -> Arc<DashMap<String, AgentServiceHandle>> {
        self.service_registry.clone()
    }

    /// 获取工作空间的引用
    pub fn get_workspaces(&self) -> Arc<DashMap<String, ProjectWorkspace>> {
        self.workspaces.clone()
    }

    /// 获取配置的引用
    pub fn get_config(&self) -> ProxyConfig {
        self.config.clone()
    }
}

impl Clone for ProxyAgentManager {
    fn clone(&self) -> Self {
        // 注意：Clone不应该复制运行时状态
        Self {
            service_registry: self.service_registry.clone(),
            request_tx: self.request_tx.clone(),
            config: self.config.clone(),
            workspaces: self.workspaces.clone(),
        }
    }
}

// ProxyAgentManager 是线程安全的，可以在线程间传递
unsafe impl Send for ProxyAgentManager {}
unsafe impl Sync for ProxyAgentManager {}

// ============================================================================
// LocalSetAgentService Implementation
// ============================================================================

/// 在 LocalSet 中运行的实际 Agent 服务
pub struct LocalSetAgentService {
    project_id: String,
    workspace_path: PathBuf,
    request_tx: mpsc::UnboundedSender<AgentRequest>,
    request_rx: mpsc::UnboundedReceiver<AgentRequest>,
    /// ACP 连接
    acp_connection: Option<AcpClientConnection>,
}

impl LocalSetAgentService {
    /// 创建新的 LocalSetAgentService
    pub async fn new(
        project_id: String,
        workspace_path: PathBuf,
    ) -> ProxyResult<Self> {
        let (request_tx, request_rx) = mpsc::unbounded_channel();

        // 创建 ACP 连接
        let acp_connection = AcpClientConnection::new(
            project_id.clone(),
            workspace_path.clone(),
        ).await?;

        Ok(Self {
            project_id,
            workspace_path,
            request_tx,
            request_rx,
            acp_connection: Some(acp_connection),
        })
    }

    /// 获取请求发送器
    pub fn get_request_sender(&self) -> mpsc::UnboundedSender<AgentRequest> {
        self.request_tx.clone()
    }

    /// 运行 Agent 服务（在 LocalSet 中）
    pub async fn run(mut self) -> ProxyResult<()> {
        info!("LocalSetAgentService starting for project: {}", self.project_id);

        // 消息处理循环
        while let Some(request) = self.request_rx.recv().await {
            match request {
                AgentRequest::Initialize => {
                    debug!("Initializing agent service");
                }
                AgentRequest::Prompt { session_id, content, response_tx } => {
                    debug!("Processing prompt for session: {:?}, content: {}", session_id, content);
                    let result = self.handle_prompt_with_session_logic(session_id, &content).await;
                    let _ = response_tx.send(result);
                }
                AgentRequest::GetStatus { response_tx } => {
                    debug!("Getting agent status");
                    let _ = response_tx.send(AgentServiceStatus::Active);
                }
                AgentRequest::Shutdown => {
                    info!("Shutting down agent service");
                    break;
                }
            }
        }

        Ok(())
    }

    /// 处理带会话逻辑的 prompt 请求
    async fn handle_prompt_with_session_logic(
        &mut self,
        session_id: Option<String>,
        prompt: &str
    ) -> ProxyResult<(String, String)> {
        let acp_conn = self.acp_connection.as_mut()
            .ok_or_else(|| ProxyAgentError::ServiceUnavailable {
                message: "ACP connection not available".to_string(),
            })?;

        // 处理会话逻辑
        let actual_session_id = if let Some(session_id) = session_id {
            // 用户提供了 session_id，尝试加载现有会话
            match acp_conn.load_session(&session_id).await {
                Ok(loaded_session_id) => {
                    info!("Loaded existing session: {}", loaded_session_id);
                    loaded_session_id
                }
                Err(e) => {
                    warn!("Failed to load session {}: {}, creating new session", session_id, e);
                    // 加载失败，创建新会话
                    let new_session_id = acp_conn.new_session().await?;
                    info!("Created new session: {}", new_session_id);
                    new_session_id
                }
            }
        } else {
            // 没有提供 session_id，创建新会话
            let new_session_id = acp_conn.new_session().await?;
            info!("Created new session: {}", new_session_id);
            new_session_id
        };

        // 使用 ACP 连接发送提示
        let response_text = acp_conn.send_prompt(prompt).await?;
        info!("Prompt processed for session: {}", actual_session_id);
        Ok((self.project_id.clone(), actual_session_id))
    }
}

// ============================================================================
// Utility Functions
// ============================================================================

/// 生成项目ID
pub fn generate_project_id() -> String {
    Uuid::new_v4().simple().to_string()
}

/// 验证项目ID格式
pub fn validate_project_id(project_id: &str) -> ProxyResult<()> {
    // 检查项目ID不为空
    if project_id.is_empty() {
        return Err(ProxyAgentError::InvalidProjectId(
            "Project ID cannot be empty".to_string(),
        ));
    }

    // 检查项目ID长度在合理范围内 (1-100字符)
    if project_id.len() > 100 {
        return Err(ProxyAgentError::InvalidProjectId(
            "Project ID is too long (max 100 characters)".to_string(),
        ));
    }

    // 检查项目ID只包含安全字符 (字母、数字、下划线、中划线)
    if !project_id.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
        return Err(ProxyAgentError::InvalidProjectId(
            "Project ID can only contain letters, numbers, underscores, and hyphens".to_string(),
        ));
    }

    // 检查项目ID不以特殊字符开头或结尾
    if project_id.starts_with('_') || project_id.starts_with('-') ||
       project_id.ends_with('_') || project_id.ends_with('-') {
        return Err(ProxyAgentError::InvalidProjectId(
            "Project ID cannot start or end with underscore or hyphen".to_string(),
        ));
    }

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_project_id() {
        let id = generate_project_id();
        assert_eq!(id.len(), 32);
        assert!(validate_project_id(&id).is_ok());
    }

    #[test]
    fn test_validate_project_id() {
        // Valid project IDs (UUID format)
        assert!(validate_project_id("a1b2c3d4e5f6789012345678901234ab").is_ok());
        assert!(validate_project_id("1234567890abcdef1234567890abcdef").is_ok());

        // Valid project IDs (user-friendly format)
        assert!(validate_project_id("test_project_001").is_ok());
        assert!(validate_project_id("simple_test").is_ok());
        assert!(validate_project_id("my-project").is_ok());
        assert!(validate_project_id("project123").is_ok());
        assert!(validate_project_id("rust_http_server").is_ok());

        // Invalid project IDs
        assert!(validate_project_id("").is_err()); // empty
        assert!(validate_project_id("_invalid_start").is_err()); // starts with underscore
        assert!(validate_project_id("-invalid_start").is_err()); // starts with hyphen
        assert!(validate_project_id("invalid_end_").is_err()); // ends with underscore
        assert!(validate_project_id("invalid_end-").is_err()); // ends with hyphen
        assert!(validate_project_id("invalid@chars").is_err()); // contains @
        assert!(validate_project_id("invalid spaces").is_err()); // contains spaces
        assert!(validate_project_id(&"a".repeat(101)).is_err()); // too long
    }

    #[tokio::test]
    async fn test_project_workspace() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workspace_root = temp_dir.path().to_path_buf();
        let project_id = generate_project_id();

        let workspace = ProjectWorkspace::new(&workspace_root, &project_id).await.unwrap();

        assert_eq!(workspace.project_id, project_id);
        assert!(workspace.exists());
        assert!(workspace.get_size().await.unwrap() == 0);
    }
}