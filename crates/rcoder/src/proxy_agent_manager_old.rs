use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::pin::Pin;

use anyhow::Result;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{info, warn, error, debug};
use uuid::Uuid;

use agent_client_protocol::{
    AgentSideConnection, ClientSideConnection, SessionId,
    InitializeRequest, NewSessionRequest, PromptRequest, PromptResponse,
    V1, ClientCapabilities, Client, Agent,
    RequestPermissionRequest, RequestPermissionResponse,
    WriteTextFileRequest, WriteTextFileResponse,
    ReadTextFileRequest, ReadTextFileResponse,
    CreateTerminalRequest, CreateTerminalResponse,
    TerminalOutputRequest, TerminalOutputResponse,
    ReleaseTerminalRequest, ReleaseTerminalResponse,
    WaitForTerminalExitRequest, WaitForTerminalExitResponse,
    KillTerminalCommandRequest, KillTerminalCommandResponse,
    SessionNotification, ExtRequest, ExtNotification,
    ContentBlock, TextContent, Error, EmbeddedResource, ResourceUri,
};

// 这些将在实际实现时使用
use codex_acp_agent::{CodexAgent, Config};
use piper;
use codex_core::config::ConfigOverrides;
use async_trait::async_trait;

/// 嵌入式客户端实现
struct EmbeddedClient {}

#[async_trait::async_trait(?Send)]
impl Client for EmbeddedClient {
    async fn request_permission(
        &self,
        _request: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, Error> {
        Err(Error::method_not_found())
    }

    async fn write_text_file(
        &self,
        _request: WriteTextFileRequest,
    ) -> Result<WriteTextFileResponse, Error> {
        Err(Error::method_not_found())
    }

    async fn read_text_file(
        &self,
        _request: ReadTextFileRequest,
    ) -> Result<ReadTextFileResponse, Error> {
        Err(Error::method_not_found())
    }

    async fn create_terminal(
        &self,
        _request: CreateTerminalRequest,
    ) -> Result<CreateTerminalResponse, Error> {
        Err(Error::method_not_found())
    }

    async fn session_notification(
        &self,
        _notification: SessionNotification,
    ) -> Result<(), Error> {
        Err(Error::method_not_found())
    }

    async fn terminal_output(
        &self,
        _request: TerminalOutputRequest,
    ) -> Result<TerminalOutputResponse, Error> {
        Err(Error::method_not_found())
    }

    async fn release_terminal(
        &self,
        _request: ReleaseTerminalRequest,
    ) -> Result<ReleaseTerminalResponse, Error> {
        Err(Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _request: WaitForTerminalExitRequest,
    ) -> Result<WaitForTerminalExitResponse, Error> {
        Err(Error::method_not_found())
    }

    async fn kill_terminal_command(
        &self,
        _request: KillTerminalCommandRequest,
    ) -> Result<KillTerminalCommandResponse, Error> {
        Err(Error::method_not_found())
    }

    async fn ext_method(
        &self,
        _request: ExtRequest,
    ) -> Result<Arc<agent_client_protocol::RawValue>, Error> {
        Err(Error::method_not_found())
    }

    async fn ext_notification(
        &self,
        _notification: ExtNotification,
    ) -> Result<(), Error> {
        Err(Error::method_not_found())
    }
}

// ============================================================================
// Configuration Types
// ============================================================================

/// Configuration for the ProxyAgentManager
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

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            workspace_root: PathBuf::from("./project_workspace"),
            idle_timeout: Duration::from_secs(300), // 5 minutes
            cleanup_interval: Duration::from_secs(60), // 1 minute
            max_concurrent_agents: 10,
            local_set_config: LocalSetConfig::default(),
        }
    }
}

/// Configuration for LocalSet runtime
#[derive(Debug, Clone)]
pub struct LocalSetConfig {
    /// 是否启用调试模式
    pub debug_mode: bool,
    
    /// 消息队列大小
    pub message_queue_size: usize,
    
    /// 连接超时时间
    pub connection_timeout: Duration,
}

impl Default for LocalSetConfig {
    fn default() -> Self {
        Self {
            debug_mode: false,
            message_queue_size: 1000,
            connection_timeout: Duration::from_secs(30),
        }
    }
}

// ============================================================================
// Core Data Structures
// ============================================================================

/// Status of an Agent service
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentServiceStatus {
    Initializing,
    Active,
    Idle,
    Error(String),
    Shutdown,
}

/// Information about a session
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub acp_session_id: SessionId,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
}

/// Handle to an Agent service running in LocalSet
#[derive(Debug)]
pub struct AgentServiceHandle {
    pub project_id: String,
    pub workspace_path: PathBuf,
    pub status: AgentServiceStatus,
    pub last_activity: Instant,
    pub created_at: Instant,
    
    /// 会话管理
    pub active_sessions: Arc<DashMap<String, SessionInfo>>,
    
    /// 与LocalSet中Agent服务通信的通道
    pub request_tx: mpsc::UnboundedSender<AgentRequest>,
    
    /// 服务任务句柄
    pub service_task: Option<tokio::task::JoinHandle<()>>,
}

impl AgentServiceHandle {
    pub fn new(
        project_id: String,
        workspace_path: PathBuf,
        request_tx: mpsc::UnboundedSender<AgentRequest>,
    ) -> Self {
        let now = Instant::now();
        Self {
            project_id,
            workspace_path,
            status: AgentServiceStatus::Initializing,
            last_activity: now,
            created_at: now,
            active_sessions: Arc::new(DashMap::new()),
            request_tx,
            service_task: None,
        }
    }

    /// 更新服务状态
    pub fn update_status(&mut self, status: AgentServiceStatus) {
        self.status = status;
        self.last_activity = Instant::now();
    }

    /// 更新活动时间
    pub fn update_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    /// 检查服务是否空闲
    pub fn is_idle(&self, timeout: Duration) -> bool {
        self.last_activity.elapsed() > timeout
    }

    /// 发送请求到Agent服务
    pub async fn send_request(&self, request: AgentRequest) -> ProxyResult<()> {
        self.request_tx
            .send(request)
            .map_err(|_| ProxyAgentError::CommunicationError {
                message: "Failed to send request to agent service".to_string(),
            })?;
        Ok(())
    }

    /// 关闭服务
    pub async fn shutdown(&mut self) -> ProxyResult<()> {
        self.update_status(AgentServiceStatus::Shutdown);

        // 发送关闭请求
        let (response_tx, mut response_rx) = oneshot::channel();
        let request = AgentRequest::GetStatus { response_tx };

        self.send_request(request).await?;

        // 等待响应或超时
        tokio::select! {
            _ = response_rx => {
                debug!("Agent service shutdown gracefully");
            }
            _ = tokio::time::sleep(Duration::from_secs(5)) => {
                warn!("Agent service shutdown timeout");
            }
        }

        // 取消服务任务
        if let Some(task) = self.service_task.take() {
            task.abort();
        }

        Ok(())
    }
}

/// Project workspace management
#[derive(Debug, Clone)]
pub struct ProjectWorkspace {
    pub project_id: String,
    pub workspace_path: PathBuf,
    pub created_at: DateTime<Utc>,
    pub last_accessed: DateTime<Utc>,
}

impl ProjectWorkspace {
    /// 创建新的项目工作空间（同步版本）
    pub fn new(project_id: String, root_path: &std::path::Path) -> Self {
        let workspace_path = root_path.join(&project_id);
        let now = Utc::now();

        Self {
            project_id,
            workspace_path,
            created_at: now,
            last_accessed: now,
        }
    }

    /// 创建新的项目工作空间（异步版本，自动创建目录）
    pub async fn create_async(project_id: &str, root_path: &std::path::Path) -> ProxyResult<Self> {
        let workspace_path = root_path.join(project_id);
        let now = Utc::now();

        // 创建目录结构
        tokio::fs::create_dir_all(&workspace_path)
            .await
            .map_err(|e| ProxyAgentError::WorkspaceError {
                path: workspace_path.clone()
            })?;

        info!("Created project workspace: {:?} for project: {}", workspace_path, project_id);

        Ok(Self {
            project_id: project_id.to_string(),
            workspace_path,
            created_at: now,
            last_accessed: now,
        })
    }

    /// 确保目录结构存在
    pub async fn ensure_directory_structure(&self) -> ProxyResult<()> {
        if !self.workspace_path.exists() {
            tokio::fs::create_dir_all(&self.workspace_path)
                .await
                .map_err(|e| ProxyAgentError::WorkspaceError {
                    path: self.workspace_path.clone()
                })?;
            info!("Created directory structure: {:?}", self.workspace_path);
        }
        Ok(())
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
            } else if metadata.is_dir() {
                // 简化处理，不递归计算子目录
                total_size += metadata.len();
            }
        }

        Ok(total_size)
    }

    /// 清理工作空间（删除所有文件和目录）
    pub async fn cleanup(&self) -> ProxyResult<()> {
        if self.exists() {
            tokio::fs::remove_dir_all(&self.workspace_path)
                .await
                .map_err(|e| ProxyAgentError::WorkspaceError {
                    path: self.workspace_path.clone()
                })?;
            info!("Cleaned up project workspace: {:?}", self.workspace_path);
        }
        Ok(())
    }
}

// ============================================================================
// Message Types
// ============================================================================

/// Requests sent to the ProxyAgentManager
#[derive(Debug)]
pub enum ProxyRequest {
    SendPrompt {
        project_id: String,
        session_id: Option<String>,
        prompt: String,
        response_tx: oneshot::Sender<std::result::Result<(String, String), ProxyAgentError>>, // (response, session_id)
    },
    CreateAgent {
        project_id: String,
        response_tx: oneshot::Sender<std::result::Result<(), ProxyAgentError>>,
    },
    GetAgentStatus {
        project_id: String,
        response_tx: oneshot::Sender<std::result::Result<AgentServiceStatus, ProxyAgentError>>,
    },
    Shutdown,
}

/// Requests sent to individual Agent services in LocalSet
#[derive(Debug)]
pub enum AgentRequest {
    Initialize,
    Prompt {
        session_id: Option<String>,
        content: String,
        response_tx: oneshot::Sender<std::result::Result<(String, String), ProxyAgentError>>, // (response, session_id)
    },
    GetStatus {
        response_tx: oneshot::Sender<AgentResponse>,
    },
    Shutdown,
}

/// Responses from Agent services
#[derive(Debug)]
pub enum AgentResponse {
    Initialized,
    PromptResult(std::result::Result<(String, String), ProxyAgentError>), // (response, session_id)
    Status(AgentServiceStatus),
    Error(String),
}

/// Responses from the ProxyAgentManager
#[derive(Debug)]
pub enum ProxyResponse {
    /// 提示响应
    PromptResult(std::result::Result<(String, String), ProxyAgentError>), // (response, session_id)
    /// 创建Agent响应
    AgentCreated(std::result::Result<(), ProxyAgentError>),
    /// Agent状态
    AgentStatus(std::result::Result<AgentServiceStatus, ProxyAgentError>),
    /// 错误响应
    Error(String),
}

// ============================================================================
// Error Types
// ============================================================================

/// Error types for the ProxyAgentManager system
#[derive(Debug, thiserror::Error)]
pub enum ProxyAgentError {
    #[error("Agent service not found for project: {project_id}")]
    AgentNotFound { project_id: String },

    #[error("Failed to create agent service: {source}")]
    AgentCreationFailed {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>
    },

    #[error("Agent service communication error: {message}")]
    CommunicationError { message: String },

    #[error("LocalSet runtime error: {source}")]
    LocalSetError {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>
    },

    #[error("Workspace creation failed: {path}")]
    WorkspaceError { path: PathBuf },

    #[error("Configuration error: {message}")]
    ConfigError { message: String },

    #[error("Timeout error: operation timed out after {duration:?}")]
    TimeoutError { duration: Duration },

    #[error("Session error: {message}")]
    SessionError { message: String },

    #[error("ACP protocol error: {message}")]
    AcpError { message: String },

    #[error("Invalid project ID: {project_id}")]
    InvalidProjectId { project_id: String },

    #[error("Session not found: {session_id}")]
    SessionNotFound { session_id: String },

    #[error("IO error: {source}")]
    IoError {
        #[from]
        source: std::io::Error,
    },

    #[error("Channel send error")]
    ChannelSendError,

    #[error("Channel receive error")]
    ChannelReceiveError,
}

impl From<tokio::sync::mpsc::error::SendError<ProxyRequest>> for ProxyAgentError {
    fn from(_: tokio::sync::mpsc::error::SendError<ProxyRequest>) -> Self {
        ProxyAgentError::ChannelSendError
    }
}

impl From<tokio::sync::mpsc::error::SendError<AgentRequest>> for ProxyAgentError {
    fn from(_: tokio::sync::mpsc::error::SendError<AgentRequest>) -> Self {
        ProxyAgentError::ChannelSendError
    }
}

impl From<tokio::sync::mpsc::error::SendError<AgentResponse>> for ProxyAgentError {
    fn from(_: tokio::sync::mpsc::error::SendError<AgentResponse>) -> Self {
        ProxyAgentError::ChannelSendError
    }
}

// Note: oneshot channels don't have a SendError type - they return the value back on failure
// We'll handle oneshot send failures manually in the code where needed

/// Result type alias for ProxyAgentManager operations
pub type ProxyResult<T> = std::result::Result<T, ProxyAgentError>;

// ============================================================================
// Utility Functions
// ============================================================================

/// Generate a project ID using UUID without hyphens
pub fn generate_project_id() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Validate project ID format
pub fn validate_project_id(project_id: &str) -> ProxyResult<()> {
    if project_id.is_empty() {
        return Err(ProxyAgentError::InvalidProjectId {
            project_id: project_id.to_string(),
        });
    }

    // Check if it's a valid UUID without hyphens (32 hex characters)
    if project_id.len() != 32 {
        return Err(ProxyAgentError::InvalidProjectId {
            project_id: project_id.to_string(),
        });
    }

    if !project_id.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ProxyAgentError::InvalidProjectId {
            project_id: project_id.to_string(),
        });
    }

    Ok(())
}

/// Build project workspace path
pub fn build_project_path(root_path: &std::path::Path, project_id: &str) -> PathBuf {
    root_path.join(project_id)
}

/// Generate a unique session ID
pub fn generate_session_id() -> String {
    format!("session_{}_{}", chrono::Utc::now().timestamp(), Uuid::new_v4().simple())
}

/// Format duration for display
pub fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;

    if days > 0 {
        format!("{}d {}h {}m", days, hours % 24, mins % 60)
    } else if hours > 0 {
        format!("{}h {}m {}s", hours, mins % 60, secs % 60)
    } else if mins > 0 {
        format!("{}m {}s", mins, secs % 60)
    } else {
        format!("{}s", secs)
    }
}

// ============================================================================
// Core ProxyAgentManager Implementation
// ============================================================================

/// 代理管理器，负责管理多个Agent服务的生命周期
#[derive(Debug)]
pub struct ProxyAgentManager {
    /// Agent服务注册表
    service_registry: Arc<DashMap<String, AgentServiceHandle>>,

    /// 消息分发通道发送器
    request_tx: mpsc::UnboundedSender<ProxyRequest>,

    /// LocalSet运行时句柄
    local_set_handle: Option<JoinHandle<()>>,

    /// 配置
    config: ProxyConfig,

    /// 项目工作空间管理
    workspaces: Arc<DashMap<String, ProjectWorkspace>>,
}

impl ProxyAgentManager {
    /// 创建新的代理管理器实例
    pub async fn new(config: ProxyConfig) -> ProxyResult<Self> {
        info!("Creating ProxyAgentManager with config: {:?}", config);

        let service_registry = Arc::new(DashMap::new());
        let workspaces = Arc::new(DashMap::new());

        // 创建消息分发通道
        let (request_tx, request_rx) = mpsc::unbounded_channel();

        // 创建管理器实例
        let manager = Self {
            service_registry,
            request_tx,
            local_set_handle: None,
            config,
            workspaces,
        };

        // 启动消息分发器
        manager.start_message_dispatcher(request_rx).await?;

        // 启动清理任务
        manager.start_cleanup_task().await;

        info!("ProxyAgentManager created successfully");
        Ok(manager)
    }

    /// 发送提示到指定的Agent服务
    pub async fn send_prompt(
        &self,
        project_id: &str,
        session_id: Option<&str>,
        prompt: &str,
    ) -> ProxyResult<(String, String)> {
        info!("Sending prompt to project {} (session: {:?}): {}", project_id, session_id, prompt);

        // 验证项目ID
        validate_project_id(project_id)?;

        // 检查是否存在对应的Agent服务
        if !self.service_registry.contains_key(project_id) {
            // 自动创建Agent服务
            self.create_agent_service(project_id).await?;
        }

        // 创建响应通道
        let (response_tx, response_rx) = oneshot::channel();

        // 发送请求到消息分发器
        let request = ProxyRequest::SendPrompt {
            project_id: project_id.to_string(),
            session_id: session_id.map(|s| s.to_string()),
            prompt: prompt.to_string(),
            response_tx,
        };

        self.request_tx.send(request)?;

        // 等待响应
        match response_rx.await {
            Ok(result) => result,
            Err(_) => Err(ProxyAgentError::ChannelReceiveError),
        }
    }

    /// 获取或创建Agent服务
    pub async fn get_or_create_agent(&self, project_id: &str) -> ProxyResult<()> {
        validate_project_id(project_id)?;

        if self.service_registry.contains_key(project_id) {
            debug!("Agent service already exists for project: {}", project_id);
            return Ok(());
        }

        self.create_agent_service(project_id).await
    }

    /// 创建Agent服务
    async fn create_agent_service(&self, project_id: &str) -> ProxyResult<()> {
        info!("Creating agent service for project: {}", project_id);

        // 验证项目ID
        validate_project_id(project_id)?;

        // 检查是否已存在
        if self.service_registry.contains_key(project_id) {
            debug!("Agent service already exists for project: {}", project_id);
            return Ok(());
        }

        // 检查并发限制
        let current_count = self.service_registry.len();
        if current_count >= self.config.max_concurrent_agents {
            return Err(ProxyAgentError::ConfigError {
                message: format!(
                    "Maximum concurrent agents limit reached: {}/{}",
                    current_count, self.config.max_concurrent_agents
                ),
            });
        }

        // 创建或获取项目工作空间
        let workspace = if let Some(existing) = self.workspaces.get(project_id) {
            existing.clone()
        } else {
            let workspace = ProjectWorkspace::create_async(
                project_id,
                &self.config.workspace_root,
            ).await?;
            self.workspaces.insert(project_id.to_string(), workspace.clone());
            workspace
        };

        // 创建 LocalSetAgentService 并在 LocalSet 中运行
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let (response_tx, response_rx) = mpsc::unbounded_channel();

        // 创建 LocalSetAgentService
        let local_set_service = LocalSetAgentService::new(
            project_id.to_string(),
            workspace.workspace_path.clone(),
            request_rx,
            response_tx,
        ).await?;

        // 在 LocalSet 中运行 Agent 服务
        let service_task = tokio::task::spawn_local(async move {
            let local_set = tokio::task::LocalSet::new();
            let _ = local_set.run_until(local_set_service.run()).await;
        });

        // 创建服务句柄
        let mut handle = AgentServiceHandle::new(
            project_id.to_string(),
            workspace.workspace_path.clone(),
            request_tx,
        );
        handle.service_task = Some(service_task);
        handle.update_status(AgentServiceStatus::Active);

        // 注册服务
        self.service_registry.insert(project_id.to_string(), handle);

        info!("Agent service created successfully for project: {}", project_id);
        Ok(())
    }

    /// 启动消息分发器
    async fn start_message_dispatcher(&self, mut request_rx: mpsc::UnboundedReceiver<ProxyRequest>) -> ProxyResult<()> {
        let service_registry = self.service_registry.clone();

        tokio::spawn(async move {
            while let Some(request) = request_rx.recv().await {
                match request {
                    ProxyRequest::SendPrompt {
                        project_id,
                        session_id,
                        prompt,
                        response_tx,
                    } => {
                        debug!("Dispatching prompt request to project: {}", project_id);

                        if let Some(service_handle) = service_registry.get(&project_id) {
                            // 发送实际的 prompt 请求到 LocalSetAgentService
                            let agent_response_tx = service_handle.request_tx.clone();
                            let (internal_response_tx, response_rx) = oneshot::channel();

                            let agent_request = AgentRequest::Prompt {
                                session_id,
                                content: prompt,
                                response_tx: internal_response_tx,
                            };

                            if let Err(e) = agent_response_tx.send(agent_request) {
                                let _ = response_tx.send(Err(ProxyAgentError::CommunicationError {
                                    message: format!("Failed to send prompt to agent service: {}", e),
                                }));
                            } else {
                                // 等待 LocalSetAgentService 的响应
                                tokio::spawn(async move {
                                    match response_rx.await {
                                        Ok(result) => {
                                            let _ = response_tx.send(result);
                                        }
                                        Err(_) => {
                                            let _ = response_tx.send(Err(ProxyAgentError::ChannelReceiveError));
                                        }
                                    }
                                });
                            }
                        } else {
                            let _ = response_tx.send(Err(ProxyAgentError::AgentNotFound {
                                project_id: project_id.clone()
                            }));
                        }
                    }
                    ProxyRequest::CreateAgent {
                        project_id,
                        response_tx,
                    } => {
                        debug!("Create agent request for project: {}", project_id);
                        // TODO: 实现Agent创建逻辑
                        let _ = response_tx.send(Ok(()));
                    }
                    ProxyRequest::GetAgentStatus {
                        project_id,
                        response_tx,
                    } => {
                        debug!("Get agent status request for project: {}", project_id);
                        if let Some(service_handle) = service_registry.get(&project_id) {
                            let _ = response_tx.send(Ok(service_handle.status.clone()));
                        } else {
                            let _ = response_tx.send(Err(ProxyAgentError::AgentNotFound {
                                project_id: project_id.clone()
                            }));
                        }
                    }
                    ProxyRequest::Shutdown => {
                        info!("Received shutdown request");
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    /// 启动清理任务
    async fn start_cleanup_task(&self) {
        let service_registry = self.service_registry.clone();
        let workspaces = self.workspaces.clone();
        let idle_timeout = self.config.idle_timeout;
        let cleanup_interval = self.config.cleanup_interval;

        tokio::spawn(async move {
            let mut interval_timer = tokio::time::interval(cleanup_interval);

            loop {
                interval_timer.tick().await;

                debug!("Running cleanup task for idle agents");

                let mut agents_to_remove = Vec::new();

                // 检查空闲的Agent服务
                for entry in service_registry.iter() {
                    let project_id = entry.key();
                    let service_handle = entry.value();

                    if service_handle.is_idle(idle_timeout) {
                        info!("Agent service for project {} has been idle for {:?}, scheduling cleanup",
                              project_id, service_handle.last_activity.elapsed());
                        agents_to_remove.push(project_id.clone());
                    }
                }

                // 清理空闲的Agent服务
                for project_id in agents_to_remove {
                    if let Some((_, mut service_handle)) = service_registry.remove(&project_id) {
                        info!("Cleaning up idle agent for project: {}", project_id);

                        // 关闭服务
                        if let Err(e) = service_handle.shutdown().await {
                            warn!("Failed to shutdown agent service for project {}: {}", project_id, e);
                        }

                        // 清理工作空间（可选）
                        if let Some(workspace) = workspaces.get(&project_id) {
                            if let Err(e) = workspace.cleanup().await {
                                warn!("Failed to cleanup workspace for project {}: {}", project_id, e);
                            }
                            workspaces.remove(&project_id);
                        }
                    }
                }
            }
        });
    }

    /// 获取Agent服务状态
    pub async fn get_agent_status(&self, project_id: &str) -> ProxyResult<AgentServiceStatus> {
        validate_project_id(project_id)?;

        if let Some(service_handle) = self.service_registry.get(project_id) {
            Ok(service_handle.status.clone())
        } else {
            Err(ProxyAgentError::AgentNotFound {
                project_id: project_id.to_string(),
            })
        }
    }

    /// 获取所有活跃的项目ID
    pub async fn get_active_projects(&self) -> Vec<String> {
        self.service_registry
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    }

    /// 清理空闲的Agent服务
    pub async fn cleanup_idle_agents(&self) -> ProxyResult<usize> {
        let mut cleaned_count = 0;
        let idle_timeout = self.config.idle_timeout;

        let mut agents_to_remove = Vec::new();

        // 收集需要清理的Agent服务
        for entry in self.service_registry.iter() {
            let project_id = entry.key();
            let service_handle = entry.value();

            if service_handle.is_idle(idle_timeout) {
                agents_to_remove.push(project_id.clone());
            }
        }

        // 执行清理
        for project_id in agents_to_remove {
            if let Some((_, mut service_handle)) = self.service_registry.remove(&project_id) {
                info!("Cleaning up idle agent for project: {}", project_id);

                if let Err(e) = service_handle.shutdown().await {
                    warn!("Failed to shutdown agent service for project {}: {}", project_id, e);
                }

                // 清理工作空间
                if let Some(workspace) = self.workspaces.get(&project_id) {
                    if let Err(e) = workspace.cleanup().await {
                        warn!("Failed to cleanup workspace for project {}: {}", project_id, e);
                    }
                    self.workspaces.remove(&project_id);
                }

                cleaned_count += 1;
            }
        }

        info!("Cleaned up {} idle agent services", cleaned_count);
        Ok(cleaned_count)
    }

    /// 优雅关闭
    pub async fn shutdown(&self) -> ProxyResult<()> {
        info!("Shutting down ProxyAgentManager");

        // 关闭所有Agent服务
        let project_ids: Vec<String> = self.service_registry.iter().map(|entry| entry.key().clone()).collect();
        for project_id in project_ids {
            if let Some((_, mut service_handle)) = self.service_registry.remove(&project_id) {
                if let Err(e) = service_handle.shutdown().await {
                    warn!("Failed to shutdown agent service for project {}: {}", project_id, e);
                }
            }
        }

        // 清空注册表
        self.service_registry.clear();
        self.workspaces.clear();

        info!("ProxyAgentManager shutdown completed");
        Ok(())
    }
}

impl Clone for ProxyAgentManager {
    fn clone(&self) -> Self {
        // 注意：Clone不应该复制运行时状态
        Self {
            service_registry: self.service_registry.clone(),
            request_tx: self.request_tx.clone(),
            local_set_handle: None, // 不复制运行时句柄
            config: self.config.clone(),
            workspaces: self.workspaces.clone(),
        }
    }
}

// ============================================================================
// LocalSetAgentService Implementation
// ============================================================================

/// ACP 服务封装器（在 LocalSet 中运行）
struct AcpService {
    client_connection: ClientSideConnection,
    active_sessions: HashMap<String, SessionId>,
}

/// 在 LocalSet 中运行的实际 Agent 服务，处理 ACP 协议通信
pub struct LocalSetAgentService {
    project_id: String,
    workspace_path: PathBuf,

    // 会话管理
    active_sessions: HashMap<String, SessionId>,

    // 消息处理通道
    request_rx: mpsc::UnboundedReceiver<AgentRequest>,
    response_tx: mpsc::UnboundedSender<AgentResponse>,
}

impl LocalSetAgentService {
    /// 创建新的 LocalSetAgentService
    pub async fn new(
        project_id: String,
        workspace_path: PathBuf,
        request_rx: mpsc::UnboundedReceiver<AgentRequest>,
        response_tx: mpsc::UnboundedSender<AgentResponse>,
    ) -> ProxyResult<Self> {
        Ok(Self {
            project_id,
            workspace_path,
            active_sessions: HashMap::new(),
            request_rx,
            response_tx,
        })
    }

    /// 运行 Agent 服务（在 LocalSet 中）
    pub async fn run(mut self) -> ProxyResult<()> {
        info!("LocalSetAgentService starting for project: {}", self.project_id);

        // 创建并初始化 ACP 服务
        let mut agent_service = self.create_acp_service().await?;

        // 消息处理循环
        while let Some(request) = self.request_rx.recv().await {
            match request {
                AgentRequest::Initialize => {
                    debug!("Initializing agent service");
                    self.response_tx.send(AgentResponse::Initialized)?;
                }
                AgentRequest::Prompt { session_id, content, response_tx } => {
                    debug!("Processing prompt for session: {:?}", session_id);
                    let result = self.handle_prompt(&mut agent_service, session_id.as_deref(), &content).await;
                    let _ = response_tx.send(result);
                }
                AgentRequest::GetStatus { response_tx } => {
                    debug!("Getting agent status");
                    let _ = response_tx.send(AgentResponse::Status(AgentServiceStatus::Active));
                }
                AgentRequest::Shutdown => {
                    info!("Shutting down agent service");
                    break;
                }
            }
        }

        Ok(())
    }

    /// 初始化 ACP 连接
    async fn initialize_connections(&mut self) -> ProxyResult<()> {
        info!("Initializing ACP connections for project: {}", self.project_id);

        // 确保工作目录存在
        if !self.workspace_path.exists() {
            tokio::fs::create_dir_all(&self.workspace_path).await
                .map_err(|e| ProxyAgentError::WorkspaceError {
                    path: self.workspace_path.clone()
                })?;
        }

        // 使用 piper 库创建双向管道
        let (client_to_agent_rx, client_to_agent_tx) = piper::pipe(1024);
        let (agent_to_client_rx, agent_to_client_tx) = piper::pipe(1024);

        // 创建会话更新通道
        let (session_update_tx, _session_update_rx) = mpsc::unbounded_channel();

        // 创建客户端操作通道
        let (client_tx, _client_rx) = mpsc::unbounded_channel();

        // 加载配置
        let config = Config::load_with_cli_overrides(
            vec![],
            Default::default(),
        ).map_err(|e| {
            error!("Failed to load config: {}", e);
            ProxyAgentError::ConfigError {
                message: format!("Failed to load config: {}", e),
            }
        })?;

        // 创建 CodexAgent 实例
        let agent = CodexAgent::with_config(
            session_update_tx.clone(),
            client_tx.clone(),
            config,
        );

        // 创建 AgentSideConnection
        let (_server_conn, server_handle_io) = AgentSideConnection::new(
            agent,
            client_to_agent_tx,  // agent 接收来自 client 的数据
            agent_to_client_rx,  // agent 发送数据给 client
            move |fut| {
                tokio::task::spawn_local(fut);
            }
        );

        // 创建 ClientSideConnection
        let (client_conn, client_handle_io) = ClientSideConnection::new(
            EmbeddedClient {},
            agent_to_client_tx,  // client 接收来自 agent 的数据
            client_to_agent_rx,  // client 发送数据给 agent
            move |fut| {
                tokio::task::spawn_local(fut);
            }
        );

        // 存储 ACP 连接
        self.agent_connection = Some(_server_conn);
        self.client_connection = Some(client_conn);

        info!("ACP connections initialized successfully for project: {}", self.project_id);
        Ok(())
    }

    /// 处理 prompt 请求
    async fn handle_prompt(&mut self, session_id: Option<&str>, prompt: &str) -> ProxyResult<(String, String)> {
        // 确保有会话
        let actual_session_id = self.ensure_session(session_id).await?;

        // 创建实际的 ACP PromptRequest
        let prompt_request = PromptRequest {
            session_id: self.active_sessions[&actual_session_id].clone(),
            prompt: vec![ContentBlock::Text(TextContent {
                text: prompt.to_string(),
                annotations: None,
                meta: None,
            })],
            meta: None,
        };

        // 发送 prompt 请求到 Agent
        if let Some(ref mut client_connection) = self.client_connection {
            match client_connection.prompt(prompt_request).await {
                Ok(prompt_response) => {
                    // 处理响应
                    let response_text = self.process_prompt_response(prompt_response).await;
                    info!("Prompt processed for session: {}", actual_session_id);
                    Ok((response_text, actual_session_id))
                }
                Err(e) => {
                    error!("Failed to process prompt: {}", e);
                    Err(ProxyAgentError::AcpError {
                        message: format!("ACP prompt failed: {}", e),
                    })
                }
            }
        } else {
            Err(ProxyAgentError::AcpError {
                message: "Client connection not initialized".to_string(),
            })
        }
    }

    
    /// 确保会话存在（自动判断是新建还是加载）
    async fn ensure_session(&mut self, session_id: Option<&str>) -> ProxyResult<String> {
        match session_id {
            Some(id) => {
                // 尝试加载现有会话
                if self.active_sessions.contains_key(id) {
                    debug!("Using existing session: {}", id);
                    Ok(id.to_string())
                } else {
                    // 会话不存在，尝试加载
                    match self.load_existing_session(id).await {
                        Ok(_) => Ok(id.to_string()),
                        Err(_) => {
                            // 加载失败，创建新会话
                            debug!("Session {} not found, creating new session", id);
                            self.create_new_session().await
                        }
                    }
                }
            }
            None => {
                // 没有提供 session_id，创建新会话
                debug!("No session_id provided, creating new session");
                self.create_new_session().await
            }
        }
    }

    /// 创建新会话
    async fn create_new_session(&mut self) -> ProxyResult<String> {
        let session_id = Uuid::new_v4().simple().to_string();

        // 创建实际的 ACP new_session 请求
        if let Some(ref mut client_connection) = self.client_connection {
            let new_session_request = NewSessionRequest {
                cwd: self.workspace_path.clone(),
                mcp_servers: None,
                meta: None,
            };

            match client_connection.new_session(new_session_request).await {
                Ok(session_response) => {
                    let acp_session_id = session_response.session_id;
                    self.active_sessions.insert(session_id.clone(), acp_session_id);
                    info!("Created new session: {}", session_id);
                    Ok(session_id)
                }
                Err(e) => {
                    error!("Failed to create new session: {}", e);
                    Err(ProxyAgentError::AcpError {
                        message: format!("ACP session creation failed: {}", e),
                    })
                }
            }
        } else {
            Err(ProxyAgentError::AcpError {
                message: "Client connection not initialized".to_string(),
            })
        }
    }

    /// 加载现有会话
    async fn load_existing_session(&mut self, session_id: &str) -> ProxyResult<()> {
        // 检查会话是否已存在于内存中
        if self.active_sessions.contains_key(session_id) {
            debug!("Session {} already loaded", session_id);
            return Ok(());
        }

        // ACP 协议没有直接的 load_session 方法
        // 我们需要检查会话是否仍然有效
        if let Some(ref mut client_connection) = self.client_connection {
            // 尝试通过发送一个简单的请求来验证会话
            // 如果会话无效，会返回错误
            let test_request = PromptRequest {
                session_id: SessionId(session_id.into()),
                prompt: vec![ContentBlock::Text(TextContent {
                    text: "Connection test".to_string(),
                    annotations: None,
                    meta: None,
                })],
                meta: None,
            };

            match client_connection.prompt(test_request).await {
                Ok(_) => {
                    // 会话有效，添加到活跃会话列表
                    self.active_sessions.insert(session_id.to_string(), SessionId(session_id.into()));
                    info!("Loaded existing session: {}", session_id);
                    Ok(())
                }
                Err(_) => {
                    // 会话无效，返回错误
                    Err(ProxyAgentError::SessionNotFound {
                        session_id: session_id.to_string(),
                    })
                }
            }
        } else {
            Err(ProxyAgentError::AcpError {
                message: "Client connection not initialized".to_string(),
            })
        }
    }

    /// 处理 ACP PromptResponse
    async fn process_prompt_response(&self, prompt_response: PromptResponse) -> String {
        // 提取文本内容
        let mut response_text = String::new();

        for content_block in prompt_response.content {
            match content_block {
                ContentBlock::Text(text_content) => {
                    response_text.push_str(&text_content.text);
                }
                ContentBlock::Resource(resource) => {
                    // 处理资源
                    info!("Resource in prompt response: {:?}", resource);
                    response_text.push_str(&format!("[Resource: {}]", resource.uri));
                }
            }
        }

        // 如果没有内容，使用默认响应
        if response_text.is_empty() {
            response_text = "No response content".to_string();
        }

        response_text
    }
}

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
        // Valid project IDs
        assert!(validate_project_id("a1b2c3d4e5f6789012345678901234ab").is_ok());
        assert!(validate_project_id("1234567890abcdef1234567890abcdef").is_ok());

        // Invalid project IDs
        assert!(validate_project_id("").is_err());
        assert!(validate_project_id("invalid-id").is_err());
        assert!(validate_project_id("a1b2c3d4-e5f6-7890-1234-5678901234ab").is_err()); // with hyphens
        assert!(validate_project_id("short").is_err());
        assert!(validate_project_id("a1b2c3d4e5f678901234567890abcdefg").is_err()); // too long
    }

    #[test]
    fn test_agent_service_handle_creation() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let handle = AgentServiceHandle::new(
            "test_project".to_string(),
            PathBuf::from("/tmp/test"),
            tx,
        );

        assert_eq!(handle.project_id, "test_project");
        assert!(matches!(handle.status, AgentServiceStatus::Initializing));
        assert!(handle.active_sessions.is_empty());
    }

    #[test]
    fn test_agent_service_handle_status() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut handle = AgentServiceHandle::new(
            "test_project".to_string(),
            PathBuf::from("/tmp/test"),
            tx,
        );

        // Test initial status
        assert!(matches!(handle.status, AgentServiceStatus::Initializing));

        // Test status update
        handle.update_status(AgentServiceStatus::Active);
        assert!(matches!(handle.status, AgentServiceStatus::Active));

        // Test activity update
        let initial_activity = handle.last_activity;
        handle.update_activity();
        assert!(handle.last_activity > initial_activity);

        // Test idle check
        assert!(!handle.is_idle(Duration::from_secs(0)));
        assert!(handle.is_idle(Duration::from_secs(u64::MAX)));
    }

    #[test]
    fn test_project_workspace_creation() {
        let workspace = ProjectWorkspace::new(
            "test_project".to_string(),
            &PathBuf::from("/tmp"),
        );

        assert_eq!(workspace.project_id, "test_project");
        assert_eq!(workspace.workspace_path, PathBuf::from("/tmp/test_project"));
        assert!(workspace.created_at <= chrono::Utc::now());
        assert!(workspace.last_accessed <= chrono::Utc::now());
    }

    #[test]
    fn test_project_workspace_access_update() {
        let mut workspace = ProjectWorkspace::new(
            "test_project".to_string(),
            &PathBuf::from("/tmp"),
        );

        let initial_access = workspace.last_accessed;
        workspace.update_access();
        assert!(workspace.last_accessed >= initial_access);
    }

    #[test]
    fn test_proxy_config_default() {
        let config = ProxyConfig::default();
        assert_eq!(config.workspace_root, PathBuf::from("./project_workspace"));
        assert_eq!(config.idle_timeout, Duration::from_secs(300));
        assert_eq!(config.cleanup_interval, Duration::from_secs(60));
        assert_eq!(config.max_concurrent_agents, 10);
    }

    #[test]
    fn test_local_set_config_default() {
        let config = LocalSetConfig::default();
        assert!(!config.debug_mode);
        assert_eq!(config.message_queue_size, 1000);
        assert_eq!(config.connection_timeout, Duration::from_secs(30));
    }

    #[test]
    fn test_build_project_path() {
        let root = PathBuf::from("/workspace");
        let project_id = "testproject";
        let path = build_project_path(&root, project_id);
        assert_eq!(path, PathBuf::from("/workspace/testproject"));
    }

    #[test]
    fn test_generate_session_id() {
        let session_id = generate_session_id();
        assert!(session_id.starts_with("session_"));
        assert!(session_id.len() > 10); // Should be longer than just "session_"
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(Duration::from_secs(30)), "30s");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m 30s");
        assert_eq!(format_duration(Duration::from_secs(3661)), "1h 1m 1s");
        assert_eq!(format_duration(Duration::from_secs(90061)), "1d 1h 1m 1s");
    }

    #[tokio::test]
    async fn test_project_workspace_create_async() {
        let temp_dir = tempfile::tempdir().unwrap();
        let project_id = "test_project_async";

        let workspace = ProjectWorkspace::create_async(
            project_id,
            temp_dir.path(),
        ).await.expect("Failed to create workspace");

        assert_eq!(workspace.project_id, project_id);
        assert!(workspace.workspace_path.exists());
        assert!(workspace.workspace_path.is_dir());
    }

    #[tokio::test]
    async fn test_project_workspace_ensure_directory_structure() {
        let temp_dir = tempfile::tempdir().unwrap();
        let project_id = "test_project_ensure";

        let workspace = ProjectWorkspace::new(
            project_id.to_string(),
            temp_dir.path(),
        );

        // Directory should not exist initially
        assert!(!workspace.exists());

        // Ensure directory structure
        workspace.ensure_directory_structure().await.expect("Failed to ensure directory structure");

        // Directory should exist now
        assert!(workspace.exists());
    }
}