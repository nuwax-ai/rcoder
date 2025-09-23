use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Path, State},
    response::{IntoResponse, Sse, sse::Event},
    routing::{get, post},
};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_stream::{StreamExt, wrappers::UnboundedReceiverStream};
use tower_http::cors::CorsLayer;
use tracing::{Span, debug, error, info, instrument, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

mod model;
mod proxy_agent;

use crate::proxy_agent::*;
use model::*;

mod codex_agent_client;
use codex_agent_client::{GlobalAgentManager, SerializedSessionNotification};

mod proxy_agent_manager;
use proxy_agent_manager::{ProxyAgentManager, ProxyConfig, ProxyRequest, ProxyResult};

mod progress_events;
use progress_events::{
    ProgressEvent, ProgressEventSubType, ProgressEventType, SessionMessageManager,
};

use acp_adapter::SessionManager;
use acp_adapter::mention::ResourceUri;
use acp_adapter::types::StreamUpdate;
use agent_client_protocol::SessionId as AcpSessionId;
use futures::stream::Stream;
// use opentelemetry::trace::TraceContextExt;
// use tracing_opentelemetry::OpenTelemetrySpanExt;
mod middleware;

// ==================== 数据结构定义 ====================

/// 用户请求结构 - 支持多媒体内容
#[derive(Debug, Deserialize, Serialize, Clone)]
struct ChatRequest {
    /// 用户输入的 prompt
    prompt: String,
    /// 用户 ID
    user_id: String,
    /// 可选的项目 ID
    project_id: Option<String>,
    /// 可选的会话 ID，如果不提供则创建新会话
    session_id: Option<String>,
}

/// 服务响应结构
#[derive(Debug, Serialize)]
struct ChatResponse {
    /// 项目 ID
    project_id: String,
    /// 会话 ID
    session_id: String,
    /// 可选的错误信息
    error: Option<String>,
}

/// 会话信息结构
#[derive(Debug, Clone, Serialize)]
struct SessionInfo {
    session_id: String,
    user_id: String,
    project_id: Option<String>,
    agent_type: AgentType,
    created_at: chrono::DateTime<chrono::Utc>,
    last_activity: chrono::DateTime<chrono::Utc>,
}

/// AI 代理类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
enum AgentType {
    Codex,
    Claude,
    Proxy,
}

impl std::fmt::Display for AgentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentType::Codex => write!(f, "codex"),
            AgentType::Claude => write!(f, "claude"),
            AgentType::Proxy => write!(f, "proxy"),
        }
    }
}

/// 应用配置
#[derive(Debug, Clone)]
struct AppConfig {
    /// 默认使用的 AI 代理类型
    default_agent: AgentType,
    /// 项目工作目录
    projects_dir: PathBuf,
    /// 服务端口
    port: u16,
}

/// 本地任务响应 - 包含关键字段用于立即返回
#[derive(Debug, Clone, Serialize)]
struct LocalTaskResponse {
    /// 项目 ID
    project_id: String,
    /// 会话 ID
    session_id: String,
    /// 任务状态
    status: String,
    /// 可选的错误信息
    error: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_agent: AgentType::Codex,
            projects_dir: PathBuf::from("./project_workspace"),
            port: 3000,
        }
    }
}

/// 本地任务请求
#[derive(Debug)]
struct LocalTaskRequest {
    agent_type: AgentType,
    request: ChatRequest,
    config: AppConfig,
    state: SharedState,
    session_id: Option<String>,
    response_tx: tokio::sync::oneshot::Sender<Result<LocalTaskResponse, String>>,
}

/// 应用状态
#[derive(Clone)]
struct AppState {
    /// 活跃的会话映射, project_id -> SessionInfo
    sessions: Arc<DashMap<String, SessionInfo>>,
    /// 应用配置
    config: AppConfig,
    /// 进度事件广播通道（session_id -> 发送者列表）
    progress_senders: Arc<DashMap<String, Vec<mpsc::UnboundedSender<ProgressEvent>>>>,
    /// Session 消息管理器 - 为每个 session_id 维护循环数组缓存
    message_manager: Arc<SessionMessageManager>,
    /// ACP会话管理器
    session_manager: Arc<SessionManager>,
    /// 全局 Codex 管理器（MPMC 架构）- 使用全局单例
    codex_manager: Arc<GlobalAgentManager>,
    /// ACP 代理管理器
    proxy_manager: Arc<ProxyAgentManager>,
    /// 本地任务发送器
    local_task_sender: mpsc::UnboundedSender<LocalSetAgentRequest>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("sessions", &self.sessions)
            .field("config", &self.config)
            .field("progress_senders", &self.progress_senders)
            .field("message_manager", &"SessionMessageManager")
            .field("session_manager", &"SessionManager")
            .field("codex_manager", &"GlobalAgentManager")
            .field("proxy_manager", &"ProxyAgentManager")
            .field("local_task_sender", &"UnboundedSender<LocalTaskRequest>")
            .finish()
    }
}

type SharedState = Arc<AppState>;

// ==================== 辅助函数 ====================

/// 获取当前请求的 trace_id - 生成唯一标识符
fn get_trace_id() -> Option<String> {
    // 简化实现：为每个请求生成唯一的 trace_id
    // 在实际的分布式追踪系统中，这个 ID 应该从上游服务传递过来
    // 或者从 OpenTelemetry 上下文中提取，但为了避免复杂性，这里直接生成
    Some(Uuid::new_v4().simple().to_string())
}

/// 将StreamUpdate转换为ProgressEvent
fn stream_update_to_progress_event(stream_update: StreamUpdate) -> ProgressEvent {
    let (event_type, sub_type, session_id, content) = match stream_update {
        StreamUpdate::UserMessageChunk {
            session_id,
            content,
        } => (
            ProgressEventType::TaskStarted,
            ProgressEventSubType::UserMessageChunk,
            session_id.0.to_string(),
            content,
        ),
        StreamUpdate::AgentMessageChunk {
            session_id,
            content,
        } => (
            ProgressEventType::Executing,
            ProgressEventSubType::AgentMessageChunk,
            session_id.0.to_string(),
            content,
        ),
        StreamUpdate::AgentThoughtChunk {
            session_id,
            content,
        } => (
            ProgressEventType::Executing,
            ProgressEventSubType::AgentThoughtChunk,
            session_id.0.to_string(),
            content,
        ),
        StreamUpdate::ToolCall {
            session_id,
            tool_call,
        } => (
            ProgressEventType::ToolCall,
            ProgressEventSubType::ToolCall,
            session_id.0.to_string(),
            format!("正在执行工具调用: {}", tool_call.title),
        ),
        StreamUpdate::ToolCallUpdate {
            session_id,
            tool_call_update,
        } => (
            ProgressEventType::ToolCallUpdate,
            ProgressEventSubType::ToolCallUpdate,
            session_id.0.to_string(),
            format!("工具调用更新: {}", tool_call_update.title),
        ),
        StreamUpdate::Plan { session_id, plan } => (
            ProgressEventType::PlanUpdate,
            ProgressEventSubType::PlanUpdate,
            session_id.0.to_string(),
            "Plan已更新".to_string(),
        ),
        StreamUpdate::AvailableCommandsUpdate {
            session_id,
            available_commands,
        } => (
            ProgressEventType::AvailableCommandsUpdate,
            ProgressEventSubType::AvailableCommandsUpdate,
            session_id.0.to_string(),
            format!("可用命令已更新，共{}个命令", available_commands.len()),
        ),
        StreamUpdate::CurrentModeUpdate {
            session_id,
            current_mode_id,
        } => (
            ProgressEventType::CurrentModeUpdate,
            ProgressEventSubType::CurrentModeUpdate,
            session_id.0.to_string(),
            format!("当前模式已更新为: {}", current_mode_id.0),
        ),
        StreamUpdate::PromptCompleted {
            session_id,
            stop_reason,
        } => (
            ProgressEventType::TaskCompleted,
            ProgressEventSubType::PromptCompleted,
            session_id.0.to_string(),
            format!("任务完成: {:?}", stop_reason),
        ),
        StreamUpdate::Error { session_id, error } => (
            ProgressEventType::TaskFailed,
            ProgressEventSubType::Error,
            session_id.0.to_string(),
            format!("任务失败: {}", error),
        ),
        _ => {
            // 对于其他不常用的事件类型，返回通用的任务执行事件
            (
                ProgressEventType::Executing,
                ProgressEventSubType::Unknown,
                "unknown".to_string(),
                "任务执行中".to_string(),
            )
        }
    };

    ProgressEvent::new(session_id, event_type, sub_type, content)
}

/// 将SessionNotification转换为ProgressEvent
fn session_notification_to_progress_event(
    notification: SerializedSessionNotification,
) -> ProgressEvent {
    let (event_type, sub_type, content) = match notification.update_type.as_str() {
        "AgentMessageChunk" => (
            ProgressEventType::Executing,
            ProgressEventSubType::AgentMessageChunk,
            notification
                .content
                .unwrap_or_else(|| "Agent响应片段".to_string()),
        ),
        "UserMessageChunk" => (
            ProgressEventType::Executing,
            ProgressEventSubType::UserMessageChunk,
            notification
                .content
                .unwrap_or_else(|| "用户消息片段".to_string()),
        ),
        "AgentThoughtChunk" => (
            ProgressEventType::Executing,
            ProgressEventSubType::AgentThoughtChunk,
            notification
                .content
                .unwrap_or_else(|| "Agent思考片段".to_string()),
        ),
        "ToolCall" => (
            ProgressEventType::ToolCall,
            ProgressEventSubType::ToolCall,
            "工具调用".to_string(),
        ),
        "ToolCallUpdate" => (
            ProgressEventType::ToolCallUpdate,
            ProgressEventSubType::ToolCallUpdate,
            "工具调用更新".to_string(),
        ),
        "Plan" => (
            ProgressEventType::PlanUpdate,
            ProgressEventSubType::PlanUpdate,
            "计划更新".to_string(),
        ),
        "AvailableCommandsUpdate" => (
            ProgressEventType::AvailableCommandsUpdate,
            ProgressEventSubType::AvailableCommandsUpdate,
            "可用命令更新".to_string(),
        ),
        "CurrentModeUpdate" => (
            ProgressEventType::CurrentModeUpdate,
            ProgressEventSubType::CurrentModeUpdate,
            "当前模式更新".to_string(),
        ),
        _ => (
            ProgressEventType::Executing,
            ProgressEventSubType::Unknown,
            format!("Session通知: {}", notification.update_type),
        ),
    };

    ProgressEvent::new(notification.session_id, event_type, sub_type, content)
}

/// 广播进度事件给所有监听者
fn broadcast_progress_event(state: &SharedState, session_id: &str, event: ProgressEvent) {
    // 首先将事件添加到消息管理器缓存
    let message_manager = state.message_manager.clone();
    let session_id_owned = session_id.to_string();
    let event_clone = event.clone();

    // 在后台任务中添加到缓存，避免阻塞
    tokio::spawn(async move {
        message_manager
            .add_message(&session_id_owned, event_clone)
            .await;
    });

    // 然后广播给所有 SSE 连接
    if let Some(senders) = state.progress_senders.get(session_id) {
        let senders = senders.value();
        let mut failed_indices = Vec::new();

        for (index, sender) in senders.iter().enumerate() {
            if let Err(_) = sender.send(event.clone()) {
                failed_indices.push(index);
            }
        }

        // 清理失败的发送者
        if !failed_indices.is_empty() {
            let _ = senders; // 释放引用
            if let Some(mut senders) = state.progress_senders.get_mut(session_id) {
                // 从后往前删除，避免索引偏移
                for &index in failed_indices.iter().rev() {
                    if index < senders.len() {
                        senders.remove(index);
                    }
                }
            }
        }
    }
}

// ==================== HTTP 处理器 ====================

/// 生成不带中划线的随机项目ID
fn generate_project_id() -> String {
    Uuid::new_v4().to_string().replace("-", "")
}

/// 获取 project_id 的 workspace_path
async fn get_project_workspace(project_id: &str) -> Result<PathBuf> {
    let workspace_dir = PathBuf::from("./project_workspace");
    let project_dir = workspace_dir.join(project_id);
    Ok(project_dir)
}

/// 创建项目工作目录
async fn create_project_workspace(project_id: &str) -> Result<PathBuf> {
    let workspace_dir = PathBuf::from("./project_workspace");

    // 创建 project_workspace 目录（如果不存在）
    tokio::fs::create_dir_all(&workspace_dir).await?;

    // 创建项目目录
    let project_dir = workspace_dir.join(project_id);
    tokio::fs::create_dir_all(&project_dir).await?;

    info!("📁 创建项目工作目录: {:?}", project_dir);
    Ok(project_dir)
}

/// 处理聊天请求 - 使用 ACP 协议集成
#[axum::debug_handler]
#[instrument(skip(state))]
async fn handle_chat(
    State(state): State<SharedState>,
    Json(request): Json<ChatRequest>,
) -> Result<HttpResult<ChatResponse>, AppError> {
    info!(
        "🚀 [DEBUG] handle_chat 开始处理请求: user_id={}, project_id={:?}, session_id={:?}, prompt={}",
        request.user_id, request.project_id, request.session_id, request.prompt
    );

    // 检查是否需要生成项目ID
    let project_id = if request.project_id.is_some() {
        debug!("📝 [DEBUG] 使用请求中的项目ID: {:?}", request.project_id);
        request.project_id.clone().unwrap()
    } else {
        let new_project_id = generate_project_id();
        debug!("🆕 [DEBUG] 生成新的项目ID: {}", new_project_id);

        // 创建项目工作目录
        create_project_workspace(&new_project_id).await?;
        new_project_id
    };

    // 获取项目工作目录
    let project_workspace = get_project_workspace(&project_id).await?;

    let chat_prompt = ChatPrompt {
        project_id: project_id.clone(),
        project_path: project_workspace,
        session_id: request.session_id.clone(),
        prompt: request.prompt.clone(),
    };

    let (local_task_request, chat_prompt_rx) = LocalSetAgentRequest::new(chat_prompt);
    state.local_task_sender.send(local_task_request)?;

    let result = match chat_prompt_rx.await {
        Ok(chat_prompt_response) => {
            info!(
                "✅ 收到 agent 执行结果: project_id={}, session_id={}",
                chat_prompt_response.project_id, chat_prompt_response.session_id,
            );
            HttpResult::success(ChatResponse {
                project_id: chat_prompt_response.project_id,
                session_id: chat_prompt_response.session_id,
                error: None,
            })
        }
        Err(e) => {
            error!("❌ 收到 agent 执行结果失败: {}", e);
            HttpResult::error("LOCAL001", &format!("Local task sender send error: {}", e))
        }
    };
    Ok(result)
}

/// 获取会话信息
#[tracing::instrument(skip(state), fields(session_id = %session_id))]
async fn get_session(
    State(state): State<SharedState>,
    Path(session_id): Path<String>,
) -> HttpResult<SessionInfo> {
    match state.sessions.get(&session_id) {
        Some(session) => HttpResult::success(session.clone()),
        None => HttpResult::error("SES001", &format!("Session '{}' not found", session_id)),
    }
}

/// 获取用户的所有会话
async fn get_user_sessions(
    State(state): State<SharedState>,
    Path(user_id): Path<String>,
) -> HttpResult<Vec<SessionInfo>> {
    let user_sessions: Vec<SessionInfo> = state
        .sessions
        .iter()
        .filter(|entry| entry.value().user_id == user_id)
        .map(|entry| entry.value().clone())
        .collect();

    HttpResult::success(user_sessions)
}

/// 删除会话
async fn delete_session(
    State(state): State<SharedState>,
    Path(session_id): Path<String>,
) -> HttpResult<String> {
    match state.sessions.remove(&session_id) {
        Some(_) => {
            info!("Session {} deleted", session_id);
            HttpResult::success(format!("Session '{}' deleted successfully", session_id))
        }
        None => HttpResult::error("SES002", &format!("Session '{}' not found", session_id)),
    }
}
/// 健康检查端点
async fn health_check() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "status": "healthy",
        "timestamp": chrono::Utc::now(),
        "service": "rcoder-ai-service"
    }))
}

/// SSE 进度推送端点 - 统一处理所有agent数据（通过ACP StreamUpdate和SessionNotification）
async fn progress_stream(
    State(state): State<SharedState>,
    Path(session_id): Path<String>,
) -> Sse<impl Stream<Item = Result<Event, axum::Error>>> {
    let (tx, rx) = mpsc::unbounded_channel();

    // 将新的发送者添加到广播列表中
    {
        let mut senders = state
            .progress_senders
            .entry(session_id.clone())
            .or_insert_with(Vec::new);
        senders.push(tx.clone());
    }

    info!("新的 SSE 连接已建立为 session: {}", session_id);

    // 首先发送历史缓存消息
    let historical_messages = state.message_manager.get_messages(&session_id).await;
    if !historical_messages.is_empty() {
        info!(
            "发送 {} 条历史消息给 session: {}",
            historical_messages.len(),
            session_id
        );
        for message in historical_messages {
            if let Err(_) = tx.send(message) {
                warn!("发送历史消息失败，连接可能已断开");
                break;
            }
        }
    }

    // 订阅ACP StreamUpdate事件 - 包括Plan更新
    let acp_session_id = AcpSessionId(session_id.clone().into());
    if let Some(session_handle) = state.session_manager.get_session(&acp_session_id) {
        let mut stream_update_rx = session_handle.subscribe_to_updates().await;
        let tx_acp = tx.clone();
        tokio::spawn(async move {
            while let Some(stream_update) = stream_update_rx.recv().await {
                let progress_event = stream_update_to_progress_event(stream_update);
                if let Err(_) = tx_acp.send(progress_event) {
                    break; // 连接已断开
                }
            }
        });
        info!("✅ 已订阅ACP StreamUpdate事件为session: {}", session_id);
    } else {
        debug!(
            "ℹ️  Session {} 没有ACP会话，可能是Proxy Agent会话",
            session_id
        );
        // 对于Proxy Agent，发送一个提示事件
        let info_event = ProgressEvent::new(
            session_id.clone(),
            ProgressEventType::Message,
            ProgressEventSubType::Message,
            "Proxy Agent会话已建立，等待任务执行...".to_string(),
        )
        .with_metadata("agent_type".to_string(), serde_json::json!("proxy"));
        let _ = tx.send(info_event);
    }

    // 订阅SessionNotification事件（来自Codex Agent）
    let tx_notification = tx.clone();
    let agent_manager = state.codex_manager.clone();
    let session_id_for_notifications = session_id.clone();
    tokio::spawn(async move {
        // 首先发送历史通知
        let historical_notifications = agent_manager
            .get_session_notifications(&session_id_for_notifications)
            .await;
        for notification in historical_notifications {
            let progress_event = session_notification_to_progress_event(notification);
            if let Err(_) = tx_notification.send(progress_event) {
                break;
            }
        }

        // 注册SSE连接以接收实时通知
        let (notification_tx, mut notification_rx) = mpsc::unbounded_channel();
        if let Ok(_) = agent_manager
            .register_sse_connection(&session_id_for_notifications, notification_tx)
            .await
        {
            // 接收实时通知
            while let Some(notification) = notification_rx.recv().await {
                let progress_event = session_notification_to_progress_event(notification);
                if let Err(_) = tx_notification.send(progress_event) {
                    break;
                }
            }
        }
    });

    // 发送初始连接事件
    let connect_event = ProgressEvent::new(
        session_id.clone(),
        ProgressEventType::KeepAlive,
        ProgressEventSubType::KeepAlive,
        "SSE connection established".to_string(),
    );

    if let Err(e) = tx.send(connect_event) {
        error!("发送初始连接事件失败: {}", e);
    }

    // 创建流
    let stream = UnboundedReceiverStream::new(rx).map(|event| {
        let json_data = serde_json::to_string(&event).unwrap_or_default();
        Ok(Event::default()
            .event(&event.event_type.to_string())
            .data(&json_data))
    });

    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(30))
            .text("keep-alive-text"),
    )
}

// ==================== 辅助函数 ====================

/// 更新会话活动时间（根据project_id）
async fn update_session_activity_by_project(state: &SharedState, project_id: &str) {
    // 查找所有与该项目ID相关的会话，并更新其活动时间
    for mut session in state.sessions.iter_mut() {
        if let Some(session_project_id) = &session.project_id {
            if session_project_id == project_id {
                session.last_activity = chrono::Utc::now();
                debug!(
                    "🔄 [DEBUG] 更新项目 {} 的会话活动时间: {}",
                    project_id, session.session_id
                );
            }
        }
    }
}

/// 启动代理管理器分发器
async fn start_proxy_manager_dispatcher(
    request_rx: mpsc::UnboundedReceiver<ProxyRequest>,
    proxy_manager: Arc<ProxyAgentManager>,
) -> ProxyResult<()> {
    info!("Starting proxy manager dispatcher with shared components");

    // 使用主代理管理器的组件
    let service_registry = proxy_manager.get_service_registry();
    let workspaces = proxy_manager.get_workspaces();
    let config = proxy_manager.get_config();

    // 运行消息分发器
    ProxyAgentManager::run_message_dispatcher(request_rx, service_registry, workspaces, config)
        .await
}

/// 启动本地任务执行器
async fn start_local_task_executor(
    mut receiver: tokio::sync::mpsc::UnboundedReceiver<LocalTaskRequest>,
) {
    info!("🔧 [DEBUG] LocalSet 任务执行器启动");

    while let Some(task_request) = receiver.recv().await {
        debug!(
            "📥 [DEBUG] 收到任务请求: session_id={:?}, agent_type={:?}",
            task_request.session_id, task_request.agent_type
        );
        debug!(
            "📝 [DEBUG] 任务详情: prompt={}",
            task_request.request.prompt
        );

        // 在后台异步执行实际的 AI 命令
        let agent_type = task_request.agent_type.clone();
        let request = task_request.request.clone();
        let config = task_request.config.clone();
        let state = task_request.state.clone();
        let session_id = task_request.session_id.clone();
        let response_tx = task_request.response_tx;

        debug!("⚙️ [DEBUG] 开始执行 AI 命令...");

        // 在 LocalSet 中异步执行 AI 任务
        // 注意：这里使用 tokio::task::spawn_local 来支持非 Send 的 LocalSet
        let result = tokio::task::spawn_local(async move {
            let result = execute_ai_command_background(
                &agent_type,
                &request,
                &config,
                &state,
                session_id.as_deref(),
            )
            .await;

            match result {
                Ok(task_response) => {
                    debug!("✅ [DEBUG] AI 命令执行成功，发送结果");
                    let _ = response_tx.send(Ok(task_response));
                }
                Err(e) => {
                    error!("❌ [DEBUG] AI 命令执行失败: {}", e);
                    let _ = response_tx.send(Err(e.to_string()));
                }
            }
        })
        .await;

        if let Err(e) = result {
            error!("❌ [DEBUG] 启动后台任务失败: {}", e);
            // 如果启动失败，我们无法通过 response_tx 发送错误，因为它已经被移动了
            // 这种情况下，我们只能记录错误，调用者会得到超时
        }

        debug!("🔄 [DEBUG] 任务已提交到后台执行，等待下一个任务...");
    }

    warn!("⚠️ [DEBUG] LocalSet 任务执行器结束");
}

/// 执行 AI 命令
#[tracing::instrument(skip(request, config, state), fields(agent_type = %agent_type, session_id = ?session_id))]
async fn execute_ai_command(
    agent_type: &AgentType,
    request: &ChatRequest,
    config: &AppConfig,
    state: &SharedState,
    session_id: Option<&str>,
) -> anyhow::Result<String> {
    debug!(
        "🤖 [DEBUG] execute_ai_command 开始执行: agent_type={:?}, session_id={:?}",
        agent_type, session_id
    );
    debug!("📝 [DEBUG] AI 命令详情: prompt={}", request.prompt);

    // 添加 span 属性
    let current_span = Span::current();
    current_span.record("ai.agent_type", agent_type.to_string());
    current_span.record("ai.prompt_length", request.prompt.len());

    match agent_type {
        AgentType::Codex => {
            debug!("🧠 [DEBUG] 选择 Codex agent，使用 ACP 协议");
            info!("Using Codex ACP protocol with MPMC architecture");

            // 获取项目ID
            let project_id = request
                .project_id
                .as_ref()
                .map(|id| id.as_str())
                .unwrap_or_else(|| {
                    // 如果没有提供项目ID，使用session_id作为项目ID
                    session_id.unwrap_or("default")
                });
            debug!("📁 [DEBUG] 项目ID: {}", project_id);

            // 使用全局 Codex 管理器处理请求
            debug!("📤 [DEBUG] 调用 codex_manager.send_prompt...");
            let response = state
                .codex_manager
                .send_prompt(project_id, &request.prompt)
                .await?;
            debug!(
                "✅ [DEBUG] codex_manager.send_prompt 成功返回: {} 字符",
                response.len()
            );

            info!("Codex MPMC request completed successfully");
            Ok(response)
        }
        AgentType::Claude => {
            debug!("🤖 [DEBUG] 选择 Claude agent，使用 shell 命令");
            // Claude 仍然使用 shell 命令方式（保持向后兼容）
            info!("Using Claude shell command");

            let mut cmd = tokio::process::Command::new("claude");

            // 如果有项目 ID，设置工作目录
            if let Some(ref project_id) = request.project_id {
                let project_path = config.projects_dir.join(project_id);
                cmd.current_dir(&project_path);
                debug!("📁 [DEBUG] 设置工作目录: {:?}", project_path);
            }

            // 添加 prompt 作为参数
            cmd.arg(&request.prompt);

            debug!("📤 [DEBUG] 执行 Claude 命令: claude {:?}", request.prompt);
            info!("Executing Claude command: claude {:?}", request.prompt);

            // 执行命令并获取输出
            debug!("⏳ [DEBUG] 等待 Claude 命令执行完成...");
            let output = cmd.output().await?;

            if output.status.success() {
                let response = String::from_utf8_lossy(&output.stdout).to_string();
                debug!(
                    "✅ [DEBUG] Claude 命令执行成功，输出: {} 字符",
                    response.len()
                );
                info!("Claude command completed successfully");
                Ok(response)
            } else {
                let error = String::from_utf8_lossy(&output.stderr).to_string();
                error!("❌ [DEBUG] Claude 命令执行失败: {}", error);
                error!("Claude command failed: {}", error);
                Err(anyhow::anyhow!("Claude command failed: {}", error))
            }
        }
        AgentType::Proxy => {
            debug!("🔗 [DEBUG] 选择 Proxy agent，使用 ACP 代理管理器");
            info!("Using ACP Proxy Agent Manager");

            // 获取项目ID
            let project_id = request
                .project_id
                .as_ref()
                .map(|id| id.as_str())
                .unwrap_or_else(|| {
                    // 如果没有提供项目ID，使用session_id作为项目ID
                    session_id.unwrap_or("unknown")
                });
            debug!("📁 [DEBUG] 项目ID: {}", project_id);

            // 使用代理管理器处理请求
            debug!("📤 [DEBUG] 调用 proxy_manager.send_prompt...");
            let (response, actual_session_id) = state
                .proxy_manager
                .send_prompt(project_id, request.session_id.as_deref(), &request.prompt)
                .await?;
            debug!(
                "✅ [DEBUG] proxy_manager.send_prompt 成功返回: {} 字符",
                response.len()
            );

            // 如果成功获取到了实际的 session_id，创建 SessionInfo
            if !actual_session_id.is_empty() {
                let session_info = SessionInfo {
                    session_id: actual_session_id.clone(),
                    user_id: request.user_id.clone(),
                    project_id: request.project_id.clone(),
                    agent_type: AgentType::Proxy,
                    created_at: chrono::Utc::now(),
                    last_activity: chrono::Utc::now(),
                };
                state
                    .sessions
                    .insert(actual_session_id.clone(), session_info);
                debug!(
                    "🆕 [DEBUG] 创建了新的 SessionInfo: session_id={}, project_id={:?}",
                    actual_session_id, request.project_id
                );
            }

            info!("ACP Proxy request completed successfully");
            Ok(response)
        }
    }
}

/// 后台执行 AI 命令并返回结果
#[tracing::instrument(skip(request, config, state), fields(agent_type = %agent_type, session_id = ?session_id))]
async fn execute_ai_command_background(
    agent_type: &AgentType,
    request: &ChatRequest,
    config: &AppConfig,
    state: &SharedState,
    session_id: Option<&str>,
) -> anyhow::Result<LocalTaskResponse> {
    debug!(
        "🤖 [DEBUG] execute_ai_command_background 开始执行: agent_type={:?}, session_id={:?}",
        agent_type, session_id
    );
    debug!("📝 [DEBUG] AI 命令详情: prompt={}", request.prompt);

    // 添加 span 属性
    let current_span = Span::current();
    current_span.record("ai.agent_type", agent_type.to_string());
    current_span.record("ai.prompt_length", request.prompt.len());

    match agent_type {
        AgentType::Codex => {
            debug!("🧠 [DEBUG] 选择 Codex agent，使用 ACP 协议");
            info!("Using Codex ACP protocol with MPMC architecture (background)");

            // 获取项目ID
            let project_id = request
                .project_id
                .as_ref()
                .map(|id| id.as_str())
                .unwrap_or_else(|| {
                    // 如果没有提供项目ID，使用session_id作为项目ID
                    session_id.unwrap_or("default")
                });
            debug!("📁 [DEBUG] 项目ID: {}", project_id);

            // 使用全局 Codex 管理器处理请求
            debug!("📤 [DEBUG] 调用 codex_manager.send_prompt...");
            let _response = state
                .codex_manager
                .send_prompt(project_id, &request.prompt)
                .await?;
            debug!("✅ [DEBUG] codex_manager.send_prompt 后台执行完成");

            info!("Codex MPMC background request completed successfully");

            // 返回成功响应，包含真实的 session_id
            Ok(LocalTaskResponse {
                project_id: project_id.to_string(),
                session_id: session_id.unwrap_or("pending").to_string(),
                status: "completed".to_string(),
                error: None,
            })
        }
        AgentType::Claude => {
            debug!("🤖 [DEBUG] 选择 Claude agent，使用 shell 命令");
            info!("Using Claude shell command (background)");

            let mut cmd = tokio::process::Command::new("claude");

            // 如果有项目 ID，设置工作目录
            if let Some(ref project_id) = request.project_id {
                let project_path = config.projects_dir.join(project_id);
                cmd.current_dir(&project_path);
                debug!("📁 [DEBUG] 设置工作目录: {:?}", project_path);
            }

            // 添加 prompt 作为参数
            cmd.arg(&request.prompt);

            debug!("📤 [DEBUG] 执行 Claude 命令: claude {:?}", request.prompt);
            info!(
                "Executing Claude command (background): claude {:?}",
                request.prompt
            );

            // 执行命令并获取输出
            debug!("⏳ [DEBUG] 等待 Claude 命令执行完成...");
            let output = cmd.output().await?;

            if output.status.success() {
                debug!("✅ [DEBUG] Claude 命令后台执行成功");
                info!("Claude background command completed successfully");

                // 返回成功响应，Claude 没有 session_id 概念
                Ok(LocalTaskResponse {
                    project_id: request
                        .project_id
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string()),
                    session_id: session_id.unwrap_or("claude_session").to_string(),
                    status: "completed".to_string(),
                    error: None,
                })
            } else {
                let error = String::from_utf8_lossy(&output.stderr).to_string();
                error!("❌ [DEBUG] Claude 命令后台执行失败: {}", error);
                error!("Claude background command failed: {}", error);

                // 返回错误响应
                Ok(LocalTaskResponse {
                    project_id: request
                        .project_id
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string()),
                    session_id: session_id.unwrap_or("claude_session").to_string(),
                    status: "failed".to_string(),
                    error: Some(error),
                })
            }
        }
        AgentType::Proxy => {
            debug!("🔗 [DEBUG] 选择 Proxy agent，使用 ACP 代理管理器");
            info!("Using ACP Proxy Agent Manager (background)");

            // 获取项目ID
            let project_id = request
                .project_id
                .as_ref()
                .map(|id| id.as_str())
                .unwrap_or_else(|| {
                    // 如果没有提供项目ID，使用session_id作为项目ID
                    session_id.unwrap_or("unknown")
                });
            debug!("📁 [DEBUG] 项目ID: {}", project_id);

            // 使用代理管理器处理请求
            debug!("📤 [DEBUG] 调用 proxy_manager.send_prompt...");
            let (_response, actual_session_id) = state
                .proxy_manager
                .send_prompt(project_id, request.session_id.as_deref(), &request.prompt)
                .await?;
            debug!("✅ [DEBUG] proxy_manager.send_prompt 后台执行完成");

            // 如果成功获取到了实际的 session_id，创建 SessionInfo
            if !actual_session_id.is_empty() {
                let session_info = SessionInfo {
                    session_id: actual_session_id.clone(),
                    user_id: request.user_id.clone(),
                    project_id: request.project_id.clone(),
                    agent_type: AgentType::Proxy,
                    created_at: chrono::Utc::now(),
                    last_activity: chrono::Utc::now(),
                };
                state.sessions.insert(project_id.to_string(), session_info);
                debug!(
                    "🆕 [DEBUG] 创建了新的 SessionInfo: session_id={}, project_id={:?}",
                    actual_session_id, request.project_id
                );
            }

            info!("ACP Proxy background request completed successfully");

            // 返回成功响应，包含真实的 session_id
            Ok(LocalTaskResponse {
                project_id: request
                    .project_id
                    .clone()
                    .unwrap_or_else(|| project_id.to_string()),
                session_id: if actual_session_id.is_empty() {
                    session_id.unwrap_or("pending").to_string()
                } else {
                    actual_session_id
                },
                status: "completed".to_string(),
                error: None,
            })
        }
    }
}

/// 加载配置
fn load_config() -> AppConfig {
    let mut config = AppConfig::default();

    // 从环境变量读取配置
    if let Ok(agent) = std::env::var("DEFAULT_AGENT") {
        config.default_agent = match agent.to_lowercase().as_str() {
            "claude" => AgentType::Claude,
            _ => AgentType::Codex,
        };
    }

    if let Ok(port) = std::env::var("PORT") {
        config.port = port.parse().unwrap_or(3000);
    }

    if let Ok(projects_dir) = std::env::var("PROJECTS_DIR") {
        config.projects_dir = PathBuf::from(projects_dir);
    }

    info!(
        "Loaded config: default_agent={}, port={}, projects_dir={:?}",
        config.default_agent, config.port, config.projects_dir
    );

    config
}

/// 创建 Axum 路由
fn create_router(state: SharedState) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/chat", post(handle_chat))
        .route(
            "/sessions/{session_id}",
            get(get_session).delete(delete_session),
        )
        .route("/users/{user_id}/sessions", get(get_user_sessions))
        .route("/progress/{session_id}", get(progress_stream))
        .layer(CorsLayer::permissive())
        // 自定义追踪中间件 - 自动生成和管理 trace_id
        .layer(axum::middleware::from_fn(
            middleware::tracing_middleware::tracing_middleware_handler,
        ))
        // OpenTelemetry tracing layer for automatic trace context propagation
        .layer(axum_tracing_opentelemetry::middleware::OtelAxumLayer::default())
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // 初始化 OpenTelemetry
    init_telemetry()?;

    info!("Starting rcoder - AI-powered development platform");

    // 加载配置
    let config = load_config();

    // 创建项目工作目录
    tokio::fs::create_dir_all(&config.projects_dir).await?;
    info!("Projects directory: {:?}", config.projects_dir);

    // 初始化应用状态
    let session_manager = Arc::new(SessionManager::new());

    // 创建本地任务通道
    let (local_task_sender, local_task_receiver) = tokio::sync::mpsc::unbounded_channel();

    // 创建代理管理器配置
    let proxy_config = ProxyConfig {
        workspace_root: config.projects_dir.clone(),
        idle_timeout: 3600,    // 1小时空闲超时（秒）
        cleanup_interval: 300, // 5分钟清理间隔（秒）
        max_concurrent_agents: 10,
    };

    // // 创建项目与 Agent 服务池
    // PROJECT_AND_AGENT_INFO_MAP.get_or_init(|| DashMap::new());

    // 在独立 OS 线程中启动单线程 tokio 运行时 + LocalSet，驻留运行 agent_worker（!Send）
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to build single-thread runtime for LocalSet agents");
        rt.block_on(async move {
            let local_set = tokio::task::LocalSet::new();
            local_set
                .run_until(async move {
                    if let Err(e) = proxy_agent::agent_worker(local_task_receiver).await {
                        error!("Failed to run agent worker: {}", e);
                    }
                    warn!("Agent worker stopped");
                })
                .await;
        });
    });

    // 创建代理管理器并获取请求接收器
    let (proxy_manager, proxy_request_rx) = ProxyAgentManager::new(proxy_config).await?;

    let state = Arc::new(AppState {
        sessions: Arc::new(DashMap::new()),
        config: config.clone(),
        progress_senders: Arc::new(DashMap::new()),
        message_manager: Arc::new(SessionMessageManager::new(1000)), // 缓存最近1000条消息
        session_manager,
        codex_manager: Arc::new(GlobalAgentManager::global()),
        proxy_manager: Arc::new(proxy_manager),
        local_task_sender,
    });

    // proxy_manager 不需要直接访问 app_state，通过参数传递即可

    // 创建路由
    let app = create_router(state.clone());

    // 启动 HTTP 服务器
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", config.port))
        .await
        .unwrap();

    info!("Server starting on port {}", config.port);
    info!("API endpoints:");
    info!("  POST /chat - Send chat message to AI agent (legacy)");
    info!("  POST /chat/proxy - Send chat message via ACP Proxy Agent Manager (NEW)");
    info!("  POST /chat/multipart - Send multipart chat with files (legacy)");
    info!("  POST /chat/acp-multipart - Send multipart chat with ACP native content blocks");
    info!("  GET  /sessions/:session_id - Get session info");
    info!("  GET  /users/:user_id/sessions - Get user's sessions");
    info!("  DELETE /sessions/:session_id - Delete session");
    info!("  GET  /progress/:session_id - SSE progress stream for AI tasks (unified stream)");
    info!("  GET  /health - Health check");
    info!("  NOTE: Plan data is delivered via the unified /progress/{{session_id}} SSE stream");

    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("Server error: {}", e));

    // 关闭代理管理器
    if let Err(e) = state.proxy_manager.shutdown().await {
        error!("Failed to shutdown proxy manager: {}", e);
    }

    Ok(())
}

/// 初始化遥测系统
fn init_telemetry() -> anyhow::Result<()> {
    // 简化的 OpenTelemetry 设置，只使用 tracing 和基本的 span 功能
    // 设置全局文本传播器（用于 trace context 传播）
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    // 初始化 tracing subscriber
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "rcoder=debug,tower_http=debug,axum_tracing_opentelemetry=info".into()
            }),
        )
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .init();

    info!("✓ Tracing 初始化成功，支持 trace_id 生成和传播");

    Ok(())
}
