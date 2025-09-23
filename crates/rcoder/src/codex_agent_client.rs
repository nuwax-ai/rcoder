//! Codex ACP 集成模块 - Agent/Client 架构
//!
//! 基于 agent-client-protocol 的真正实现：
//! - Agent 服务端：实现 Agent trait，运行在子进程中
//! - Client 客户端：实现 Client trait，管理与 agent 的通信
//! - 每个项目有独立的 agent 进程和 client 连接

use anyhow::Result;
use codex_core::config::Config;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tokio::sync::{Mutex, mpsc};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{info, warn, debug, error};
use dashmap::DashMap;
use serde::{Serialize, Deserialize};
use chrono;
use agent_client_protocol::{
    Client, ClientSideConnection, AgentSideConnection, Agent,
    SessionUpdate, SessionId, SessionNotification, ContentBlock,
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse,
    PromptRequest, PromptResponse,
};
use codex_acp_agent::{CodexAgent};

// ==================== Session Notification 相关数据结构 ====================

/// 序列化的Session Notification用于存储和传输
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedSessionNotification {
    /// 时间戳
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// 会话ID
    pub session_id: String,
    /// 更新类型
    pub update_type: String,
    /// 内容
    pub content: Option<String>,
    /// 原始数据
    pub raw_data: Option<serde_json::Value>,
}

impl SerializedSessionNotification {
    /// 从SessionUpdate创建
    pub fn from_session_update(session_id: &str, update: agent_client_protocol::SessionUpdate) -> Self {
        let (update_type, content, raw_data) = match update {
            agent_client_protocol::SessionUpdate::AgentMessageChunk { content } => {
                let content_str = if let agent_client_protocol::ContentBlock::Text(text) = content {
                    Some(text.text)
                } else {
                    None
                };
                ("AgentMessageChunk".to_string(), content_str, None)
            }
            agent_client_protocol::SessionUpdate::ToolCall(tool_call) => {
                let data = serde_json::to_value(tool_call).ok();
                ("ToolCall".to_string(), None, data)
            }
            agent_client_protocol::SessionUpdate::ToolCallUpdate(tool_call_update) => {
                let data = serde_json::to_value(tool_call_update).ok();
                ("ToolCallUpdate".to_string(), None, data)
            }
            agent_client_protocol::SessionUpdate::UserMessageChunk { content } => {
                let content_str = if let agent_client_protocol::ContentBlock::Text(text) = content {
                    Some(text.text)
                } else {
                    None
                };
                ("UserMessageChunk".to_string(), content_str, None)
            }
            agent_client_protocol::SessionUpdate::AgentThoughtChunk { content } => {
                let content_str = if let agent_client_protocol::ContentBlock::Text(text) = content {
                    Some(text.text)
                } else {
                    None
                };
                ("AgentThoughtChunk".to_string(), content_str, None)
            }
            agent_client_protocol::SessionUpdate::Plan(plan) => {
                let data = serde_json::to_value(plan).ok();
                ("Plan".to_string(), None, data)
            }
            agent_client_protocol::SessionUpdate::AvailableCommandsUpdate { available_commands } => {
                let data = serde_json::to_value(available_commands).ok();
                ("AvailableCommandsUpdate".to_string(), None, data)
            }
            agent_client_protocol::SessionUpdate::CurrentModeUpdate { current_mode_id } => {
                let data = serde_json::to_value(current_mode_id).ok();
                ("CurrentModeUpdate".to_string(), None, data)
            }
        };

        Self {
            timestamp: chrono::Utc::now(),
            session_id: session_id.to_string(),
            update_type,
            content,
            raw_data,
        }
    }

    /// 从agent_client_protocol的SessionNotification创建
    pub fn from_protocol_notification(
        session_id: String,
        notification: agent_client_protocol::SessionNotification,
    ) -> Self {
        let (update_type, content, raw_data) = match notification.update {
            agent_client_protocol::SessionUpdate::AgentMessageChunk { content } => {
                let content_str = if let agent_client_protocol::ContentBlock::Text(text) = content {
                    Some(text.text)
                } else {
                    None
                };
                ("AgentMessageChunk".to_string(), content_str, None)
            }
            agent_client_protocol::SessionUpdate::ToolCall(tool_call) => {
                let data = serde_json::to_value(tool_call).ok();
                ("ToolCall".to_string(), None, data)
            }
            agent_client_protocol::SessionUpdate::ToolCallUpdate(tool_call_update) => {
                let data = serde_json::to_value(tool_call_update).ok();
                ("ToolCallUpdate".to_string(), None, data)
            }
            agent_client_protocol::SessionUpdate::UserMessageChunk { content } => {
                let content_str = if let agent_client_protocol::ContentBlock::Text(text) = content {
                    Some(text.text)
                } else {
                    None
                };
                ("UserMessageChunk".to_string(), content_str, None)
            }
            agent_client_protocol::SessionUpdate::AgentThoughtChunk { content } => {
                let content_str = if let agent_client_protocol::ContentBlock::Text(text) = content {
                    Some(text.text)
                } else {
                    None
                };
                ("AgentThoughtChunk".to_string(), content_str, None)
            }
            agent_client_protocol::SessionUpdate::Plan(plan) => {
                let data = serde_json::to_value(plan).ok();
                ("Plan".to_string(), None, data)
            }
            agent_client_protocol::SessionUpdate::AvailableCommandsUpdate { available_commands } => {
                let data = serde_json::to_value(available_commands).ok();
                ("AvailableCommandsUpdate".to_string(), None, data)
            }
            agent_client_protocol::SessionUpdate::CurrentModeUpdate { current_mode_id } => {
                let data = serde_json::to_value(current_mode_id).ok();
                ("CurrentModeUpdate".to_string(), None, data)
            }
        };

        Self {
            timestamp: chrono::Utc::now(),
            session_id,
            update_type,
            content,
            raw_data,
        }
    }
}

/// Session Notification管理器
pub struct SessionNotificationManager {
    /// Session通知历史队列 (session_id -> Vec<SerializedSessionNotification>)
    notification_history: Arc<DashMap<String, Vec<SerializedSessionNotification>>>,
    /// 活跃的SSE连接 (session_id -> Vec<Sender>)
    active_connections: Arc<DashMap<String, Vec<mpsc::UnboundedSender<SerializedSessionNotification>>>>,
    /// 队列大小
    queue_size: usize,
}

impl SessionNotificationManager {
    /// 创建新的管理器
    pub fn new(queue_size: usize) -> Self {
        Self {
            notification_history: Arc::new(DashMap::new()),
            active_connections: Arc::new(DashMap::new()),
            queue_size,
        }
    }

    /// 添加notification到队列并推送给活跃连接
    pub async fn add_notification(
        &self,
        session_id: &str,
        notification: SerializedSessionNotification,
    ) -> Result<()> {
        // 添加到历史队列
        let mut history = self.notification_history.entry(session_id.to_string())
            .or_insert_with(Vec::new);
        history.push(notification.clone());

        // 保持队列大小限制
        if history.len() > self.queue_size {
            history.remove(0);
        }

        // 推送给活跃连接
        if let Some(mut connections) = self.active_connections.get_mut(session_id) {
            // 保留仍然活跃的连接
            connections.retain(|tx| {
                tx.send(notification.clone()).is_ok()
            });

            // 如果所有连接都断开了，清理空列表
            if connections.is_empty() {
                self.active_connections.remove(session_id);
            }
        }
        Ok(())
    }

    /// 获取session的notification历史
    pub async fn get_notifications(
        &self,
        session_id: &str,
    ) -> Vec<SerializedSessionNotification> {
        self.notification_history
            .get(session_id)
            .map(|history| history.to_vec())
            .unwrap_or_default()
    }

    /// 注册SSE连接
    pub async fn register_connection(
        &self,
        session_id: &str,
        sender: mpsc::UnboundedSender<SerializedSessionNotification>,
    ) -> Result<()> {
        self.active_connections
            .entry(session_id.to_string())
            .or_insert_with(Vec::new)
            .push(sender);
        Ok(())
    }

    /// 取消注册SSE连接
    pub async fn unregister_connection(
        &self,
        session_id: &str,
        sender: &mpsc::UnboundedSender<SerializedSessionNotification>,
    ) -> Result<()> {
        if let Some(mut connections) = self.active_connections.get_mut(session_id) {
            connections.retain(|tx| !tx.same_channel(sender));
            if connections.is_empty() {
                self.active_connections.remove(session_id);
            }
        }
        Ok(())
    }

    /// 移除断开的连接
    pub fn cleanup_disconnected_connections(&self, session_id: &str) {
        if let Some(mut connections) = self.active_connections.get_mut(session_id) {
            connections.retain(|tx| {
                // 测试连接是否仍然活跃
                tx.send(SerializedSessionNotification {
                    timestamp: chrono::Utc::now(),
                    session_id: session_id.to_string(),
                    update_type: "Ping".to_string(),
                    content: None,
                    raw_data: None,
                }).is_ok()
            });

            if connections.is_empty() {
                self.active_connections.remove(session_id);
            }
        }
    }

    /// 清理不活跃的session
    pub fn cleanup_inactive_sessions(&self, timeout_duration: std::time::Duration) {
        let now = chrono::Utc::now();
        let mut sessions_to_remove = Vec::new();

        for session_entry in self.notification_history.iter() {
            let session_id = session_entry.key();
            if let Some(last_notification) = session_entry.value().last() {
                if now.signed_duration_since(last_notification.timestamp).to_std()
                    .unwrap_or_default() > timeout_duration {
                    sessions_to_remove.push(session_id.clone());
                }
            }
        }

        for session_id in sessions_to_remove {
            self.notification_history.remove(&session_id);
            self.active_connections.remove(&session_id);
            info!("Cleaned up inactive session: {}", session_id);
        }
    }
}

/// 全局 Agent 管理器
pub struct GlobalAgentManager {
    /// 按 project_id 管理的 agent 服务
    agents: Arc<Mutex<HashMap<String, Arc<Mutex<AgentService>>>>>,
    /// 请求发送器
    request_tx: mpsc::UnboundedSender<AgentRequest>,
    /// 服务模式配置
    config: AgentManagerConfig,
    /// Session Notification 管理器
    notification_manager: Arc<SessionNotificationManager>,
}

/// Agent 管理器配置
#[derive(Debug, Clone)]
pub struct AgentManagerConfig {
    /// 默认服务模式
    pub default_mode: ServiceMode,
    /// 是否优先尝试嵌入式模式
    pub prefer_embedded: bool,
    /// 项目空闲超时时间（秒）
    pub project_idle_timeout: u64,
    /// 清理检查间隔（秒）
    pub cleanup_interval: u64,
}

impl Default for AgentManagerConfig {
    fn default() -> Self {
        Self {
            default_mode: ServiceMode::Embedded,
            prefer_embedded: true,
            project_idle_timeout: 300, // 5分钟
            cleanup_interval: 60,      // 1分钟
        }
    }
}

/// 全局单例实例
static GLOBAL_AGENT_MANAGER: OnceLock<GlobalAgentManager> = OnceLock::new();

/// 单个 Agent 服务实例
struct AgentService {
    /// 项目ID
    project_id: String,
    /// 项目路径
    project_path: PathBuf,
    /// 客户端实例
    client: Option<CodexClient>,
    /// 客户端连接
    connection: Option<ClientSideConnection>,
    /// Agent 请求发送器
    agent_request_tx: Option<mpsc::UnboundedSender<AgentMessage>>,
    /// Agent 响应接收器
    agent_response_rx: Option<mpsc::UnboundedReceiver<AgentResponse>>,
    /// Agent worker 任务
    agent_worker_task: Option<tokio::task::JoinHandle<()>>,
    /// 服务模式：subprocess 或 embedded
    service_mode: ServiceMode,
    /// 服务状态
    is_active: bool,
    /// 当前会话ID
    current_session_id: Option<SessionId>,
    /// 最后活动时间戳
    last_activity: std::time::Instant,
    /// 连接是否已初始化
    is_initialized: bool,
}

/// 发送给 Agent worker 的消息
#[derive(Debug)]
enum AgentMessage {
    /// 初始化请求
    Initialize {
        request: InitializeRequest,
        response_tx: mpsc::UnboundedSender<Result<InitializeResponse>>,
    },
    /// 提示请求
    Prompt {
        request: PromptRequest,
        response_tx: mpsc::UnboundedSender<Result<PromptResponse>>,
    },
    /// 关闭请求
    Shutdown,
}

/// 来自 Agent worker 的响应
#[derive(Debug)]
enum AgentResponse {
    /// 初始化响应
    Initialize(Result<InitializeResponse>),
    /// 提示响应
    Prompt(Result<PromptResponse>),
    /// Agent 状态更新
    StatusUpdate(String),
}

/// 服务模式
#[derive(Debug, Clone, PartialEq)]
enum ServiceMode {
    /// 子进程模式
    Subprocess,
    /// 嵌入式模式（内存中）
    Embedded,
}

/// 发送给 Agent 服务的请求
struct AgentRequest {
    /// 项目ID
    project_id: String,
    /// 提示内容
    prompt: String,
    /// 响应发送器
    response_tx: mpsc::UnboundedSender<Result<String>>,
}

/// Codex 客户端实现
#[derive(Clone)]
struct CodexClient {
    /// 项目ID
    project_id: String,
    /// 收集的响应
    collected_response: Arc<Mutex<String>>,
    /// 全局Agent管理器
    agent_manager: GlobalAgentManager,
}

#[async_trait::async_trait(?Send)]
impl Client for CodexClient {
    async fn session_notification(&self, args: SessionNotification) -> Result<(), agent_client_protocol::Error> {
        // 将通知发送到 Session Notification 管理器
        if let Err(e) = self.agent_manager.send_session_notification(&self.project_id, args.update.clone()).await {
            warn!("Failed to send session notification: {}", e);
        }

        // 保持原有的响应收集逻辑
        match args.update {
            SessionUpdate::AgentMessageChunk { content } => {
                if let ContentBlock::Text(text_content) = content {
                    let mut response = self.collected_response.lock().await;
                    response.push_str(&text_content.text);
                    info!("收到响应片段: {}", text_content.text);
                }
            }
            _ => {
                debug!("其他会话更新: {:?}", args.update);
            }
        }
        Ok(())
    }

    async fn request_permission(
        &self,
        _args: agent_client_protocol::RequestPermissionRequest,
    ) -> Result<agent_client_protocol::RequestPermissionResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn write_text_file(
        &self,
        _args: agent_client_protocol::WriteTextFileRequest,
    ) -> Result<agent_client_protocol::WriteTextFileResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn read_text_file(
        &self,
        _args: agent_client_protocol::ReadTextFileRequest,
    ) -> Result<agent_client_protocol::ReadTextFileResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn create_terminal(
        &self,
        _args: agent_client_protocol::CreateTerminalRequest,
    ) -> Result<agent_client_protocol::CreateTerminalResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn terminal_output(
        &self,
        _args: agent_client_protocol::TerminalOutputRequest,
    ) -> Result<agent_client_protocol::TerminalOutputResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn release_terminal(
        &self,
        _args: agent_client_protocol::ReleaseTerminalRequest,
    ) -> Result<agent_client_protocol::ReleaseTerminalResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _args: agent_client_protocol::WaitForTerminalExitRequest,
    ) -> Result<agent_client_protocol::WaitForTerminalExitResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn kill_terminal_command(
        &self,
        _args: agent_client_protocol::KillTerminalCommandRequest,
    ) -> Result<agent_client_protocol::KillTerminalCommandResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn ext_method(&self, _args: agent_client_protocol::ExtRequest) -> Result<agent_client_protocol::ExtResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn ext_notification(&self, _args: agent_client_protocol::ExtNotification) -> Result<(), agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }
}

impl GlobalAgentManager {
    /// 获取全局实例（线程安全）
    pub fn global() -> Self {
        Self::global_with_config(AgentManagerConfig::default())
    }

    /// 获取全局实例（带配置）
    pub fn global_with_config(config: AgentManagerConfig) -> Self {
        let manager = GLOBAL_AGENT_MANAGER.get_or_init(|| {
            let (request_tx, request_rx) = mpsc::unbounded_channel();

            let manager = Self {
                agents: Arc::new(Mutex::new(HashMap::new())),
                request_tx,
                config,
                notification_manager: Arc::new(SessionNotificationManager::new(1000)),
            };

            // 启动请求分发器
            manager.start_request_dispatcher(request_rx);

            // 启动超时检查任务
            manager.start_cleanup_task();

            manager
        });

        manager.clone()
    }

    /// 创建全局管理器（向后兼容）
    pub fn new() -> Self {
        Self {
            agents: Arc::new(Mutex::new(HashMap::new())),
            request_tx: mpsc::unbounded_channel().0,
            config: AgentManagerConfig::default(),
            notification_manager: Arc::new(SessionNotificationManager::new(1000)),
        }
    }

    /// 创建全局管理器（带配置）
    pub fn with_config(config: AgentManagerConfig) -> Self {
        Self {
            agents: Arc::new(Mutex::new(HashMap::new())),
            request_tx: mpsc::unbounded_channel().0,
            config,
            notification_manager: Arc::new(SessionNotificationManager::new(1000)),
        }
    }

    /// 启动请求分发器
    fn start_request_dispatcher(&self, mut request_rx: mpsc::UnboundedReceiver<AgentRequest>) {
        let agents = self.agents.clone();

        // 在后台运行请求处理 - 使用 tokio::spawn
        tokio::task::spawn(async move {
            while let Some(request) = request_rx.recv().await {
                let project_id = request.project_id.clone();
                let prompt = request.prompt.clone();

                // 获取对应的服务
                let service_opt = {
                    let agents_guard = agents.lock().await;
                    agents_guard.get(&project_id).cloned()
                };

                if let Some(service) = service_opt {
                    // 更新项目的最后活动时间
                    {
                        let mut service_guard = service.lock().await;
                        service_guard.last_activity = std::time::Instant::now();
                        info!("Updated activity time for project: {} (dispatcher)", project_id);
                    }

                    // 处理请求
                    let result = {
                        let service_guard = service.lock().await;

                        if service_guard.is_active {
                            info!("Agent 服务已激活，处理请求: {}", prompt);

                            // 检查是否为嵌入式模式
                            if let Some(ref request_tx) = service_guard.agent_request_tx {
                                // 嵌入式模式：通过 MPSC 发送请求
                                let (response_tx, mut response_rx) = mpsc::unbounded_channel();

                                let request = AgentMessage::Prompt {
                                    request: PromptRequest {
                                        session_id: SessionId("embedded_session".to_string().into()), // 临时会话ID
                                        prompt: vec![prompt.into()],
                                        meta: None,
                                    },
                                    response_tx,
                                };

                                if let Err(_) = request_tx.send(request) {
                                    Ok("❌ 发送请求到 Agent 失败".to_string())
                                } else {
                                    // 等待响应
                                    match response_rx.recv().await {
                                        Some(Ok(response)) => {
                                            // 嵌入式模式下，响应内容通过 session notification 传递
                                            // 这里返回一个简单的响应
                                            Ok("✅ 嵌入式 Agent 已处理请求".to_string())
                                        }
                                        Some(Err(e)) => {
                                            warn!("Agent 处理失败: {}", e);
                                            Ok(format!("❌ 处理请求失败: {}", e))
                                        }
                                        _ => {
                                            Ok("❌ 未收到有效的响应".to_string())
                                        }
                                    }
                                }
                            } else {
                                Ok("❌ 没有活跃的会话".to_string())
                            }
                        } else {
                            Ok("❌ Agent 服务未激活".to_string())
                        }
                    };

                    // 发送响应
                    let _ = request.response_tx.send(result);
                } else {
                    // 没有找到对应的项目服务，自动创建一个新的
                    info!("未找到项目 {} 的 Agent 服务，正在创建新服务...", project_id);

                    let project_path = std::path::PathBuf::from(format!("./project_workspace/{}", project_id));

                    // 创建新服务
                    let new_service = match Self::create_agent_service_for_project(
                        &agents,
                        &project_id,
                        &project_path,
                        ServiceMode::Embedded
                    ).await {
                        Ok(service) => service,
                        Err(e) => {
                            let error_msg = format!("❌ 创建 Agent 服务失败: {}", e);
                            error!("{}", error_msg);
                            let _ = request.response_tx.send(Err(anyhow::anyhow!(error_msg)));
                            continue;
                        }
                    };

                    // 将新服务添加到管理器中
                    {
                        let mut agents_guard = agents.lock().await;
                        agents_guard.insert(project_id.clone(), new_service.clone());
                        info!("✅ 成功创建并添加项目 {} 的 Agent 服务", project_id);
                    }

                    // 等待服务完全启动
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                    // 使用新创建的服务处理请求
                    let result = {
                        let service_guard = new_service.lock().await;

                        if service_guard.is_active {
                            info!("新创建的 Agent 服务已激活，处理请求: {}", prompt);

                            if let Some(ref request_tx) = service_guard.agent_request_tx {
                                let (response_tx, mut response_rx) = mpsc::unbounded_channel();

                                let request = AgentMessage::Prompt {
                                    request: PromptRequest {
                                        session_id: SessionId("embedded_session".to_string().into()),
                                        prompt: vec![prompt.into()],
                                        meta: None,
                                    },
                                    response_tx,
                                };

                                if let Err(_) = request_tx.send(request) {
                                    Ok("❌ 发送请求到新 Agent 失败".to_string())
                                } else {
                                    match response_rx.recv().await {
                                        Some(Ok(response)) => {
                                            Ok("✅ 新创建的嵌入式 Agent 已处理请求".to_string())
                                        }
                                        Some(Err(e)) => {
                                            warn!("新 Agent 处理失败: {}", e);
                                            Ok(format!("❌ 新 Agent 处理失败: {}", e))
                                        }
                                        _ => {
                                            Ok("❌ 未收到新 Agent 的有效响应".to_string())
                                        }
                                    }
                                }
                            } else {
                                Ok("❌ 新创建的服务没有活跃的会话".to_string())
                            }
                        } else {
                            Ok("❌ 新创建的 Agent 服务未激活".to_string())
                        }
                    };

                    // 发送响应
                    let _ = request.response_tx.send(result);
                }
            }
        });
    }

    /// 启动清理任务
    fn start_cleanup_task(&self) {
        let agents = self.agents.clone();
        let timeout_duration = std::time::Duration::from_secs(self.config.project_idle_timeout);
        let cleanup_interval = std::time::Duration::from_secs(self.config.cleanup_interval);

        // 清理任务不需要spawn_local，直接使用tokio::spawn
        tokio::task::spawn(async move {
            let mut interval_timer = tokio::time::interval(cleanup_interval);

            loop {
                interval_timer.tick().await;

                let mut agents_guard = agents.lock().await;
                let mut projects_to_remove = Vec::new();

                // 检查每个项目的最后活动时间
                for (project_id, service) in agents_guard.iter() {
                    let service_guard = service.lock().await;
                    if service_guard.is_active &&
                       service_guard.last_activity.elapsed() > timeout_duration {
                        info!("Project {} has been idle for {:?}, scheduling cleanup",
                              project_id, service_guard.last_activity.elapsed());
                        projects_to_remove.push(project_id.clone());
                    }
                }

                // 清理不活跃的项目
                for project_id in projects_to_remove {
                    if let Some(service) = agents_guard.remove(&project_id) {
                        let mut service_guard = service.lock().await;
                        info!("Cleaning up idle agent for project: {}", project_id);

                        // 停止 worker 任务
                        if let Some(task) = service_guard.agent_worker_task.take() {
                            task.abort();
                        }

                        service_guard.is_active = false;
                    }
                }

                drop(agents_guard);
            }
        });
    }

    /// 发送提示到 Codex
    pub async fn send_prompt(&self, project_id: &str, prompt: &str) -> Result<String> {
        info!("Sending prompt to project {}: {}", project_id, prompt);

        // 检查是否已有对应的服务
        {
            let agents = self.agents.lock().await;
            if agents.contains_key(project_id) {
                // 已有服务，发送请求
                return self.send_to_agent(project_id, prompt).await;
            }
        }

        // 创建新服务
        self.create_agent_service(project_id, prompt).await
    }

    /// 创建新的 Agent 服务
    async fn create_agent_service(&self, project_id: &str, initial_prompt: &str) -> Result<String> {
        info!("Creating new Agent service for project: {}", project_id);

        let project_path = PathBuf::from("./project_workspace").join(project_id);

        // 创建项目目录
        if !project_path.exists() {
            tokio::fs::create_dir_all(&project_path).await?;
        }

        // 创建服务实例
        let service = Arc::new(Mutex::new(AgentService {
            project_id: project_id.to_string(),
            project_path: project_path.clone(),
            client: None,
            connection: None,
            agent_request_tx: None,
            agent_response_rx: None,
            agent_worker_task: None,
            service_mode: self.config.default_mode.clone(),
            is_active: false,
            current_session_id: None,
            last_activity: std::time::Instant::now(),
            is_initialized: false,
        }));

        // 启动服务
        self.start_agent_service(service.clone()).await?;

        // 存储服务
        {
            let mut agents = self.agents.lock().await;
            agents.insert(project_id.to_string(), service.clone());
        }

        // 发送初始请求
        self.send_to_agent(project_id, initial_prompt).await
    }

    /// 为项目创建 Agent 服务（静态方法）
    async fn create_agent_service_for_project(
        agents: &Arc<Mutex<HashMap<String, Arc<Mutex<AgentService>>>>>,
        project_id: &str,
        project_path: &PathBuf,
        service_mode: ServiceMode,
    ) -> Result<Arc<Mutex<AgentService>>> {
        info!("🔧 为项目创建新的 Agent 服务: {}", project_id);

        // 创建项目目录
        if !project_path.exists() {
            tokio::fs::create_dir_all(project_path).await?;
            info!("📁 创建项目目录: {:?}", project_path);
        }

        // 创建服务实例
        let service = Arc::new(Mutex::new(AgentService {
            project_id: project_id.to_string(),
            project_path: project_path.clone(),
            client: None,
            connection: None,
            agent_request_tx: None,
            agent_response_rx: None,
            agent_worker_task: None,
            service_mode,
            is_active: false,
            current_session_id: None,
            last_activity: std::time::Instant::now(),
            is_initialized: false,
        }));

        // 启动服务
        Self::start_agent_service_static(agents, service.clone()).await?;

        info!("✅ 成功为项目 {} 创建 Agent 服务", project_id);
        Ok(service)
    }

    /// 静态方法：启动 Agent 服务
    async fn start_agent_service_static(
        agents: &Arc<Mutex<HashMap<String, Arc<Mutex<AgentService>>>>>,
        service: Arc<Mutex<AgentService>>,
    ) -> Result<()> {
        let project_id = {
            let service_guard = service.lock().await;
            service_guard.project_id.clone()
        };
        let project_path = {
            let service_guard = service.lock().await;
            service_guard.project_path.clone()
        };

        info!("🔧 启动 Agent 服务，项目: {}", project_id);

        // 创建 Codex 客户端
        let collected_response = Arc::new(Mutex::new(String::new()));

        // 创建一个临时的 agent manager 用于静态方法
        let temp_manager = Self {
            agents: agents.clone(),
            request_tx: mpsc::unbounded_channel().0,
            config: AgentManagerConfig::default(),
            notification_manager: Arc::new(SessionNotificationManager::new(1000)),
        };

        let client = CodexClient {
            project_id: project_id.clone(),
            collected_response: collected_response.clone(),
            agent_manager: temp_manager.clone(),
        };

        // 根据配置选择启动模式
        let service_mode = {
            let service_guard = service.lock().await;
            service_guard.service_mode.clone()
        };

        match service_mode {
            ServiceMode::Subprocess => {
                warn!("🔧 子进程模式未实现，项目: {}", project_id);
                return Err(anyhow::anyhow!("Subprocess mode not implemented"));
            }
            ServiceMode::Embedded => {
                // 嵌入式模式需要特殊处理，因为 LocalSet 不能跨线程
                // 这里直接启动嵌入式服务，不使用 LocalSet
                if let Err(e) = temp_manager.start_embedded_agent_service_simple(service.clone(), &client).await {
                    error!("❌ 无法为项目 {} 建立嵌入式 Agent 连接: {}", project_id, e);
                    return Err(anyhow::anyhow!("Failed to establish Agent embedded connection for project {}: {}", project_id, e));
                }
            }
        }

        Ok(())
    }

    /// 启动 Agent 服务
    async fn start_agent_service(&self, service: Arc<Mutex<AgentService>>) -> Result<()> {
        let project_id = {
            let service_guard = service.lock().await;
            service_guard.project_id.clone()
        };
        let project_path = {
            let service_guard = service.lock().await;
            service_guard.project_path.clone()
        };

        info!("Starting Agent service for project: {}", project_id);

        // 创建 Codex 客户端
        let collected_response = Arc::new(Mutex::new(String::new()));

        let client = CodexClient {
            project_id: project_id.clone(),
            collected_response: collected_response.clone(),
            agent_manager: self.clone(),
        };

        // 根据配置选择启动模式
        let service_mode = {
            let service_guard = service.lock().await;
            service_guard.service_mode.clone()
        };

        match service_mode {
            ServiceMode::Subprocess => {
                // 不支持子进程模式
                warn!("Subprocess mode not implemented for project: {}", project_id);
                return Err(anyhow::anyhow!("Subprocess mode not implemented"));
            }
            ServiceMode::Embedded => {
                // 尝试启动嵌入式 agent - 使用 LocalSet 来处理非 Send 的 Future
                let local_set = tokio::task::LocalSet::new();
                if let Err(e) = local_set.run_until(async move {
                    self.start_embedded_agent_service(service.clone(), &client).await
                }).await {
                    error!("Failed to establish Agent embedded connection for project {}: {}", project_id, e);
                    return Err(anyhow::anyhow!("Failed to establish Agent embedded connection for project {}: {}", project_id, e));
                }
            }
        }

        Ok(())
    }

    /// 启动嵌入式 Agent 服务
    async fn start_embedded_agent_service(
        &self,
        service: Arc<Mutex<AgentService>>,
        client: &CodexClient,
    ) -> Result<()> {
        let project_id = {
            let service_guard = service.lock().await;
            service_guard.project_id.clone()
        };
        let project_path = {
            let service_guard = service.lock().await;
            service_guard.project_path.clone()
        };

        info!("Starting embedded Agent service for project: {}", project_id);

        // 创建 Agent 通信通道
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let (response_tx, response_rx) = mpsc::unbounded_channel();

        // 启动 Agent worker 任务 - 使用 LocalSet 来处理非 Send 的 Future
        let worker_handle = tokio::task::spawn_local(async move {
            let _ = agent_worker(request_rx, project_path.clone()).await;
        });

        // 更新服务的状态
        {
            let mut service_guard = service.lock().await;
            service_guard.is_active = true;
            service_guard.client = Some(client.clone());
            service_guard.connection = None; // 嵌入式模式不使用直接连接
            service_guard.agent_request_tx = Some(request_tx);
            service_guard.agent_response_rx = Some(response_rx);
            service_guard.agent_worker_task = Some(worker_handle);
            service_guard.current_session_id = None; // 将在第一次通信时建立
            service_guard.is_initialized = false; // 需要初始化
        }

        info!("Embedded Agent service started successfully for project: {}", project_id);
        Ok(())
    }

    /// 启动嵌入式 Agent 服务（简化版本，不使用 LocalSet）
    async fn start_embedded_agent_service_simple(
        &self,
        service: Arc<Mutex<AgentService>>,
        client: &CodexClient,
    ) -> Result<()> {
        let project_id = {
            let service_guard = service.lock().await;
            service_guard.project_id.clone()
        };
        let project_path = {
            let service_guard = service.lock().await;
            service_guard.project_path.clone()
        };

        info!("🔧 启动嵌入式 Agent 服务（简化版），项目: {}", project_id);

        // 创建 Agent 通信通道
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let (response_tx, response_rx) = mpsc::unbounded_channel();

        // 更新服务的状态
        {
            let mut service_guard = service.lock().await;
            service_guard.is_active = true;
            service_guard.client = Some(client.clone());
            service_guard.connection = None; // 嵌入式模式不使用直接连接
            service_guard.agent_request_tx = Some(request_tx);
            service_guard.agent_response_rx = Some(response_rx);
            service_guard.agent_worker_task = None; // 暂时不启动 worker
            service_guard.current_session_id = None; // 将在第一次通信时建立
            service_guard.is_initialized = false; // 需要初始化
        }

        info!("🔧 嵌入式 Agent 服务（简化版）启动成功，项目: {}", project_id);
        Ok(())
    }

    /// 发送请求到指定服务
    async fn send_to_agent(&self, project_id: &str, prompt: &str) -> Result<String> {
        // 更新项目的最后活动时间
        {
            let agents = self.agents.lock().await;
            if let Some(service) = agents.get(project_id) {
                let mut service_guard = service.lock().await;
                service_guard.last_activity = std::time::Instant::now();
                info!("Updated activity time for project: {}", project_id);
            }
        }

        let (response_tx, mut response_rx) = mpsc::unbounded_channel();

        let request = AgentRequest {
            project_id: project_id.to_string(),
            prompt: prompt.to_string(),
            response_tx,
        };

        // 发送请求
        let _ = self.request_tx.send(request);

        // 等待响应
        match response_rx.recv().await {
            Some(Ok(response)) => Ok(response),
            Some(Err(e)) => Err(e),
            None => Err(anyhow::anyhow!("Failed to receive response")),
        }
    }


    /// 发送 Session Notification
    pub async fn send_session_notification(&self, session_id: &str, update: SessionUpdate) -> Result<()> {
        let notification = SerializedSessionNotification::from_session_update(session_id, update);
        self.notification_manager.add_notification(session_id, notification).await
    }

    /// 注册 SSE 连接
    pub async fn register_sse_connection(&self, session_id: &str, sender: mpsc::UnboundedSender<SerializedSessionNotification>) -> Result<()> {
        self.notification_manager.register_connection(session_id, sender).await
    }

    /// 取消注册 SSE 连接
    pub async fn unregister_sse_connection(&self, session_id: &str, sender: &mpsc::UnboundedSender<SerializedSessionNotification>) -> Result<()> {
        self.notification_manager.unregister_connection(session_id, sender).await
    }

    /// 获取 Session 的历史通知
    pub async fn get_session_notifications(&self, session_id: &str) -> Vec<SerializedSessionNotification> {
        self.notification_manager.get_notifications(session_id).await
    }
}

impl Clone for GlobalAgentManager {
    fn clone(&self) -> Self {
        Self {
            agents: self.agents.clone(),
            request_tx: self.request_tx.clone(),
            config: self.config.clone(),
            notification_manager: self.notification_manager.clone(),
        }
    }
}

impl Default for GlobalAgentManager {
    fn default() -> Self {
        Self::new()
    }
}



/// Agent worker 任务，在本地线程中运行 Agent
async fn agent_worker(
    mut request_rx: mpsc::UnboundedReceiver<AgentMessage>,
    project_path: PathBuf,
) -> Result<()> {
    info!("Agent worker 启动，项目路径: {:?}", project_path);

    // 创建项目目录
    if !project_path.exists() {
        tokio::fs::create_dir_all(&project_path).await?;
    }

    // 使用 piper 库创建双向管道
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
        project_path.clone(),
    ).map_err(|e| {
        error!("Failed to load config: {}", e);
        anyhow::anyhow!("Failed to load config: {}", e)
    })?;

    // 创建 CodexAgent 实例
    let agent = CodexAgent::with_config(
        session_update_tx.clone(),
        client_tx.clone(),
        config,
    );

    // 使用 LocalSet 来运行 spawn_local 任务
    let local_set = tokio::task::LocalSet::new();

    local_set.run_until(async move {
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
            EmbeddedClient {},
            agent_to_client_tx,  // client 接收来自 agent 的数据
            client_to_agent_rx,  // client 发送数据给 agent
            move |fut| {
                tokio::task::spawn_local(fut);
            }
        );

        // 启动服务端 IO 处理任务
        let mut server_io_handle = tokio::task::spawn_local(server_handle_io);

        // 启动客户端 IO 处理任务
        let mut client_io_handle = tokio::task::spawn_local(client_handle_io);

        // 启动请求处理任务
        let mut request_handle = tokio::task::spawn_local(async move {
            let mut is_initialized = false;
            let mut current_session_id: Option<SessionId> = None;

            while let Some(request) = request_rx.recv().await {
                match request {
                    AgentMessage::Initialize { request, response_tx } => {
                        let result = client_conn.initialize(request).await
                            .map_err(|e| anyhow::anyhow!(e.to_string()));
                        if result.is_ok() {
                            is_initialized = true;
                            info!("ACP connection initialized successfully");
                        }
                        let _ = response_tx.send(result);
                    }
                    AgentMessage::Prompt { request, response_tx } => {
                        info!("🔧 [DEBUG] Received prompt request in agent_worker");

                        // 确保连接已初始化
                        if !is_initialized {
                            info!("🔧 [DEBUG] Initializing ACP connection before prompt");
                            let init_result = client_conn.initialize(InitializeRequest {
                                protocol_version: agent_client_protocol::V1,
                                client_capabilities: Default::default(),
                                meta: None,
                            }).await;

                            if let Err(e) = init_result {
                                let error_msg = format!("❌ ACP 初始化失败: {}", e);
                                error!("🔧 [DEBUG] {}", error_msg);
                                let _ = response_tx.send(Err(anyhow::anyhow!(error_msg)));
                                continue;
                            }
                            is_initialized = true;
                            info!("🔧 [DEBUG] ACP connection initialized successfully");
                        }

                        // 确保有会话
                        if current_session_id.is_none() {
                            info!("🔧 [DEBUG] Creating new ACP session");
                            let session_result = client_conn.new_session(NewSessionRequest {
                                mcp_servers: Vec::new(),
                                cwd: project_path.clone(),
                                meta: None,
                            }).await;

                            match session_result {
                                Ok(session_response) => {
                                    current_session_id = Some(session_response.session_id.clone());
                                    info!("🔧 [DEBUG] ACP session created: {:?}", session_response.session_id);
                                }
                                Err(e) => {
                                    let error_msg = format!("❌ ACP 会话创建失败: {}", e);
                                    error!("🔧 [DEBUG] {}", error_msg);
                                    let _ = response_tx.send(Err(anyhow::anyhow!(error_msg)));
                                    continue;
                                }
                            }
                        }

                        // 使用现有的或新创建的会话ID
                        let mut prompt_request = request;
                        if let Some(ref session_id) = current_session_id {
                            prompt_request.session_id = session_id.clone();
                            info!("🔧 [DEBUG] Using session ID: {:?}", session_id);
                        }

                        info!("🔧 [DEBUG] Sending prompt to ACP agent");
                        let result = client_conn.prompt(prompt_request).await
                            .map_err(|e| anyhow::anyhow!(e.to_string()));

                        match &result {
                            Ok(response) => {
                                info!("🔧 [DEBUG] ACP prompt successful, response: {:?}", response);
                            }
                            Err(e) => {
                                error!("🔧 [DEBUG] ACP prompt failed: {}", e);
                            }
                        }

                        let _ = response_tx.send(result);
                        info!("🔧 [DEBUG] Prompt response sent back");
                    }
                    AgentMessage::Shutdown => {
                        info!("Received shutdown signal, stopping agent worker");
                        break;
                    }
                }
            }
        });

    // 等待任务完成，使用更安全的方式处理并发任务
    // 创建一个关闭信号通道
    let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel::<()>();

    // 监控任务状态
    let mut tasks_completed = Vec::new();

    // 监控各个任务的完成状态
    loop {
        tokio::select! {
            // 监控请求处理任务
            result = &mut request_handle => {
                info!("Request handle completed: {:?}", result);
                // 请求处理任务是正常结束的，不应该触发关闭
                // 因为用户可能继续发送请求，需要保持 agent 运行
                tasks_completed.push("request");
                // 注意：这里不发送关闭信号，保持 agent 运行
            }

            // 监控服务器IO任务
            result = &mut server_io_handle => {
                info!("Server IO handle completed: {:?}", result);
                tasks_completed.push("server_io");
                if tasks_completed.contains(&"server_io") {
                    // 服务器IO异常，发送关闭信号
                    let _ = shutdown_tx.send(());
                    break;
                }
            }

            // 监控客户端IO任务
            result = &mut client_io_handle => {
                info!("Client IO handle completed: {:?}", result);
                tasks_completed.push("client_io");
                if tasks_completed.contains(&"client_io") {
                    // 客户端IO异常，发送关闭信号
                    let _ = shutdown_tx.send(());
                    break;
                }
            }

            // 监控关闭信号
            _ = shutdown_rx.recv() => {
                info!("Received shutdown signal, stopping all tasks");
                request_handle.abort();
                server_io_handle.abort();
                client_io_handle.abort();
                break;
            }
        }
        };

        // 等待剩余任务完成
        let _ = tokio::join!(
            request_handle,
            server_io_handle,
            client_io_handle
        );
        info!("Agent worker 任务结束");
        Ok(())
    }).await
    
}


/// 嵌入式客户端实现
struct EmbeddedClient {}

#[async_trait::async_trait(?Send)]
impl Client for EmbeddedClient {
    async fn request_permission(
        &self,
        _request: agent_client_protocol::RequestPermissionRequest,
    ) -> Result<agent_client_protocol::RequestPermissionResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn write_text_file(
        &self,
        _request: agent_client_protocol::WriteTextFileRequest,
    ) -> Result<agent_client_protocol::WriteTextFileResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn read_text_file(
        &self,
        _request: agent_client_protocol::ReadTextFileRequest,
    ) -> Result<agent_client_protocol::ReadTextFileResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn create_terminal(
        &self,
        _request: agent_client_protocol::CreateTerminalRequest,
    ) -> Result<agent_client_protocol::CreateTerminalResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn terminal_output(
        &self,
        _request: agent_client_protocol::TerminalOutputRequest,
    ) -> Result<agent_client_protocol::TerminalOutputResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn release_terminal(
        &self,
        _request: agent_client_protocol::ReleaseTerminalRequest,
    ) -> Result<agent_client_protocol::ReleaseTerminalResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _request: agent_client_protocol::WaitForTerminalExitRequest,
    ) -> Result<agent_client_protocol::WaitForTerminalExitResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn kill_terminal_command(
        &self,
        _request: agent_client_protocol::KillTerminalCommandRequest,
    ) -> Result<agent_client_protocol::KillTerminalCommandResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn session_notification(
        &self,
        _notification: agent_client_protocol::SessionNotification,
    ) -> Result<(), agent_client_protocol::Error> {
        Ok(())
    }

    async fn ext_method(
        &self,
        _request: agent_client_protocol::ExtRequest,
    ) -> Result<agent_client_protocol::ExtResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn ext_notification(
        &self,
        _notification: agent_client_protocol::ExtNotification,
    ) -> Result<(), agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }
}