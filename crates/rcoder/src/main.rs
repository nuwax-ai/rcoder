use anyhow::Result;
use axum::{
    extract::{Path, State}, response::{sse::Event, IntoResponse, Sse}, routing::{get, post}, Json, Router
};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_stream::{wrappers::UnboundedReceiverStream, StreamExt};
use tower_http::cors::CorsLayer;
use tracing::{info, warn, error, Span};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

mod codex_agent_client;
use codex_agent_client::{GlobalAgentManager, SerializedSessionNotification};

use opentelemetry::trace::TraceContextExt;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use futures::stream::Stream;
use acp_adapter::mention::ResourceUri;
use acp_adapter::types::StreamUpdate;
use acp_adapter::SessionManager;
use agent_client_protocol::SessionId as AcpSessionId;

mod http_result;
use http_result::HttpResult;

mod middleware;
use middleware::TracingMiddleware;

mod multipart_chat;
use multipart_chat::{handle_multipart_chat, CodeSnippet};

mod acp_multipart_chat;
use acp_multipart_chat::handle_acp_multipart_chat;

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

/// 多媒体聊天请求结构 - 用于处理文件上传
#[derive(Debug)]
struct MultipartChatRequest {
    /// 用户输入的 prompt
    prompt: String,
    /// 用户 ID
    user_id: String,
    /// 可选的项目 ID
    project_id: Option<String>,
    /// 可选的会话 ID
    session_id: Option<String>,
    /// 上传的文件列表
    files: Vec<UploadedFile>,
    /// 代码片段列表
    code_snippets: Vec<CodeSnippet>,
    /// 选中的代码段引用
    code_references: Vec<ResourceUri>,
}

/// 上传的文件信息
#[derive(Debug)]
struct UploadedFile {
    /// 原文件名
    filename: String,
    /// MIME 类型
    content_type: String,
    /// 文件内容
    content: Vec<u8>,
    /// 文件大小
    size: usize,
    /// 生成的资源URI
    resource_uri: ResourceUri,
}

/// 服务响应结构
#[derive(Debug, Serialize)]
struct ChatResponse {
    /// 会话 ID
    session_id: String,
    /// AI 响应内容
    response: String,
    /// 响应状态
    status: String,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(PartialEq)]
enum AgentType {
    Codex,
    Claude,
}

impl std::fmt::Display for AgentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentType::Codex => write!(f, "codex"),
            AgentType::Claude => write!(f, "claude"),
        }
    }
}

/// AI 任务进度事件
#[derive(Debug, Clone, Serialize)]
pub struct ProgressEvent {
    /// 事件类型
    pub event_type: ProgressEventType,
    /// 事件消息
    pub message: String,
    /// 时间戳
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// 会话ID
    pub session_id: String,
    /// 可选的数据
    pub data: Option<serde_json::Value>,
}

/// 进度事件子类型
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressEventSubType {
    UserMessageChunk,
    AgentMessageChunk,
    AgentThoughtChunk,
    ToolCall,
    ToolCallUpdate,
    PlanUpdate,
    AvailableCommandsUpdate,
    CurrentModeUpdate,
    PromptCompleted,
    Error,
    FullUpdate,
    EntryStatusUpdate,
    EntryAdded,
    EntryRemoved,
    StatsUpdate,
}

impl std::fmt::Display for ProgressEventSubType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProgressEventSubType::UserMessageChunk => write!(f, "user_message_chunk"),
            ProgressEventSubType::AgentMessageChunk => write!(f, "agent_message_chunk"),
            ProgressEventSubType::AgentThoughtChunk => write!(f, "agent_thought_chunk"),
            ProgressEventSubType::ToolCall => write!(f, "tool_call"),
            ProgressEventSubType::ToolCallUpdate => write!(f, "tool_call_update"),
            ProgressEventSubType::PlanUpdate => write!(f, "plan_update"),
            ProgressEventSubType::AvailableCommandsUpdate => write!(f, "available_commands_update"),
            ProgressEventSubType::CurrentModeUpdate => write!(f, "current_mode_update"),
            ProgressEventSubType::PromptCompleted => write!(f, "prompt_completed"),
            ProgressEventSubType::Error => write!(f, "error"),
            ProgressEventSubType::FullUpdate => write!(f, "full_update"),
            ProgressEventSubType::EntryStatusUpdate => write!(f, "entry_status_update"),
            ProgressEventSubType::EntryAdded => write!(f, "entry_added"),
            ProgressEventSubType::EntryRemoved => write!(f, "entry_removed"),
            ProgressEventSubType::StatsUpdate => write!(f, "stats_update"),
        }
    }
}

/// 进度事件类型
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressEventType {
    /// 任务开始
    TaskStarted,
    /// 执行中
    Executing,
    /// 命令输出
    CommandOutput,
    /// 任务完成
    TaskCompleted,
    /// 任务失败
    TaskFailed,
    /// 连接保持活跃
    KeepAlive,
    /// Plan相关事件
    PlanUpdate,
    /// Plan条目状态更新
    PlanEntryUpdate,
    /// Plan统计信息更新
    PlanStatsUpdate,
    /// 可用命令更新
    AvailableCommandsUpdate,
    /// 当前模式更新
    CurrentModeUpdate,
    /// 工具调用
    ToolCall,
    /// 工具调用更新
    ToolCallUpdate,
}

impl std::fmt::Display for ProgressEventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProgressEventType::TaskStarted => write!(f, "task_started"),
            ProgressEventType::Executing => write!(f, "executing"),
            ProgressEventType::CommandOutput => write!(f, "command_output"),
            ProgressEventType::TaskCompleted => write!(f, "task_completed"),
            ProgressEventType::TaskFailed => write!(f, "task_failed"),
            ProgressEventType::KeepAlive => write!(f, "keep_alive"),
            ProgressEventType::PlanUpdate => write!(f, "plan_update"),
            ProgressEventType::PlanEntryUpdate => write!(f, "plan_entry_update"),
            ProgressEventType::PlanStatsUpdate => write!(f, "plan_stats_update"),
            ProgressEventType::AvailableCommandsUpdate => write!(f, "available_commands_update"),
            ProgressEventType::CurrentModeUpdate => write!(f, "current_mode_update"),
            ProgressEventType::ToolCall => write!(f, "tool_call"),
            ProgressEventType::ToolCallUpdate => write!(f, "tool_call_update"),
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
    session_id: String,
    response_tx: tokio::sync::oneshot::Sender<Result<String, String>>,
}

/// 应用状态
#[derive(Clone)]
struct AppState {
    /// 活跃的会话映射
    sessions: Arc<DashMap<String, SessionInfo>>,
    /// 应用配置
    config: AppConfig,
    /// 进度事件广播通道（session_id -> 发送者列表）
    progress_senders: Arc<DashMap<String, Vec<mpsc::UnboundedSender<ProgressEvent>>>>,
    /// ACP会话管理器
    session_manager: Arc<SessionManager>,
    /// 全局 Codex 管理器（MPMC 架构）- 使用全局单例
    codex_manager: Arc<GlobalAgentManager>,
    /// 本地任务发送器
    local_task_sender: tokio::sync::mpsc::UnboundedSender<LocalTaskRequest>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("sessions", &self.sessions)
            .field("config", &self.config)
            .field("progress_senders", &self.progress_senders)
            .field("session_manager", &"SessionManager")
            .field("codex_manager", &"GlobalAgentManager")
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
    let (event_type, message, session_id, data) = match stream_update {
        StreamUpdate::UserMessageChunk { session_id, content } => {
            (
                ProgressEventType::TaskStarted,
                "用户消息已接收".to_string(),
                session_id.0.to_string(),
                Some(serde_json::json!({
                    "type": ProgressEventSubType::UserMessageChunk.to_string(),
                    "content": content
                }))
            )
        }
        StreamUpdate::AgentMessageChunk { session_id, content } => {
            (
                ProgressEventType::Executing,
                "AI正在生成回复".to_string(),
                session_id.0.to_string(),
                Some(serde_json::json!({
                    "type": ProgressEventSubType::AgentMessageChunk.to_string(),
                    "content": content
                }))
            )
        }
        StreamUpdate::AgentThoughtChunk { session_id, content } => {
            (
                ProgressEventType::Executing,
                "AI正在思考".to_string(),
                session_id.0.to_string(),
                Some(serde_json::json!({
                    "type": ProgressEventSubType::AgentThoughtChunk.to_string(),
                    "content": content
                }))
            )
        }
        StreamUpdate::ToolCall { session_id, tool_call } => {
            (
                ProgressEventType::Executing,
                format!("正在执行工具调用: {}", tool_call.title),
                session_id.0.to_string(),
                Some(serde_json::json!({
                    "type": ProgressEventSubType::ToolCall.to_string(),
                    "tool_call": tool_call
                }))
            )
        }
        StreamUpdate::ToolCallUpdate { session_id, tool_call_update } => {
            (
                ProgressEventType::Executing,
                format!("工具调用更新: {}", tool_call_update.title),
                session_id.0.to_string(),
                Some(serde_json::json!({
                    "type": ProgressEventSubType::ToolCallUpdate.to_string(),
                    "tool_call_update": tool_call_update
                }))
            )
        }
        StreamUpdate::Plan { session_id, plan } => {
            (
                ProgressEventType::PlanUpdate,
                "Plan已更新".to_string(),
                session_id.0.to_string(),
                Some(serde_json::json!({
                    "type": ProgressEventSubType::PlanUpdate.to_string(),
                    "plan": plan
                }))
            )
        }
        StreamUpdate::AvailableCommandsUpdate { session_id, available_commands } => {
            (
                ProgressEventType::AvailableCommandsUpdate,
                format!("可用命令已更新，共{}个命令", available_commands.len()),
                session_id.0.to_string(),
                Some(serde_json::json!({
                    "type": ProgressEventSubType::AvailableCommandsUpdate.to_string(),
                    "commands": available_commands
                }))
            )
        }
        StreamUpdate::CurrentModeUpdate { session_id, current_mode_id } => {
            (
                ProgressEventType::CurrentModeUpdate,
                format!("当前模式已更新为: {}", current_mode_id.0),
                session_id.0.to_string(),
                Some(serde_json::json!({
                    "type": ProgressEventSubType::CurrentModeUpdate.to_string(),
                    "mode_id": current_mode_id.0
                }))
            )
        }
        StreamUpdate::PromptCompleted { session_id, stop_reason } => {
            (
                ProgressEventType::TaskCompleted,
                format!("任务完成: {:?}", stop_reason),
                session_id.0.to_string(),
                Some(serde_json::json!({
                    "type": ProgressEventSubType::PromptCompleted.to_string(),
                    "stop_reason": stop_reason
                }))
            )
        }
        StreamUpdate::Error { session_id, error } => {
            (
                ProgressEventType::TaskFailed,
                format!("任务失败: {}", error),
                session_id.0.to_string(),
                Some(serde_json::json!({
                    "type": ProgressEventSubType::Error.to_string(),
                    "error": error
                }))
            )
        }
        _ => {
            // 对于其他不常用的事件类型，返回通用的任务执行事件
            (
                ProgressEventType::Executing,
                "任务执行中".to_string(),
                "unknown".to_string(),
                None
            )
        }
    };
    
    ProgressEvent {
        event_type,
        message,
        timestamp: chrono::Utc::now(),
        session_id,
        data,
    }
}

/// 将SessionNotification转换为ProgressEvent
fn session_notification_to_progress_event(notification: SerializedSessionNotification) -> ProgressEvent {

    let (event_type, message, data) = match notification.update_type.as_str() {
        "AgentMessageChunk" => {
            (
                ProgressEventType::Executing,
                "Agent响应片段".to_string(),
                notification.content.map(|content| serde_json::json!({
                    "type": "AgentMessageChunk",
                    "content": content
                }))
            )
        }
        "UserMessageChunk" => {
            (
                ProgressEventType::Executing,
                "用户消息片段".to_string(),
                notification.content.map(|content| serde_json::json!({
                    "type": "UserMessageChunk",
                    "content": content
                }))
            )
        }
        "AgentThoughtChunk" => {
            (
                ProgressEventType::Executing,
                "Agent思考片段".to_string(),
                notification.content.map(|content| serde_json::json!({
                    "type": "AgentThoughtChunk",
                    "content": content
                }))
            )
        }
        "ToolCall" => {
            (
                ProgressEventType::ToolCall,
                "工具调用".to_string(),
                notification.raw_data.or_else(|| Some(serde_json::json!({
                    "type": "ToolCall"
                })))
            )
        }
        "ToolCallUpdate" => {
            (
                ProgressEventType::ToolCallUpdate,
                "工具调用更新".to_string(),
                notification.raw_data.or_else(|| Some(serde_json::json!({
                    "type": "ToolCallUpdate"
                })))
            )
        }
        "Plan" => {
            (
                ProgressEventType::PlanUpdate,
                "计划更新".to_string(),
                notification.raw_data
            )
        }
        "AvailableCommandsUpdate" => {
            (
                ProgressEventType::AvailableCommandsUpdate,
                "可用命令更新".to_string(),
                notification.raw_data
            )
        }
        "CurrentModeUpdate" => {
            (
                ProgressEventType::CurrentModeUpdate,
                "当前模式更新".to_string(),
                notification.raw_data
            )
        }
        _ => {
            (
                ProgressEventType::Executing,
                format!("Session通知: {}", notification.update_type),
                notification.raw_data
            )
        }
    };

    ProgressEvent {
        event_type,
        message,
        timestamp: notification.timestamp,
        session_id: notification.session_id,
        data,
    }
}

/// 广播进度事件给所有监听者
fn broadcast_progress_event(state: &SharedState, session_id: &str, event: ProgressEvent) {
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

/// 处理聊天请求 - 使用 ACP 协议集成
async fn handle_chat(
    State(state): State<SharedState>,
    Json(request): Json<ChatRequest>,
) -> HttpResult<ChatResponse> {
    info!(
        "Received chat request: user_id={}, project_id={:?}, session_id={:?}",
        request.user_id, request.project_id, request.session_id
    );

    // 生成或使用现有的会话ID
    let session_id = request.session_id.clone()
        .unwrap_or_else(|| {
            let new_session_id = Uuid::new_v4().to_string();
            // 创建新会话
            let session_info = SessionInfo {
                session_id: new_session_id.clone(),
                user_id: request.user_id.clone(),
                project_id: request.project_id.clone(),
                agent_type: state.config.default_agent.clone(),
                created_at: chrono::Utc::now(),
                last_activity: chrono::Utc::now(),
            };
            state.sessions.insert(new_session_id.clone(), session_info);
            new_session_id
        });

    // 更新会话活动时间
    update_session_activity(&state, &session_id).await;

    // 创建单向通道用于处理 AI 请求
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<String, String>>();

    // 克隆需要的数据发送到后台任务
    let agent_type = state.config.default_agent.clone();
    let request_clone = request.clone();
    let config_clone = state.config.clone();
    let state_clone = state.clone();
    let session_id_clone = session_id.clone();

    // 使用全局 LocalSet 执行任务
    if let Err(e) = state.local_task_sender.send(LocalTaskRequest {
        agent_type,
        request: request_clone,
        config: config_clone,
        state: state_clone,
        session_id: session_id_clone,
        response_tx: tx,
    }) {
        error!("Failed to send task to local executor: {}", e);
        return HttpResult::<ChatResponse>::error("AI003", "Failed to queue AI task");
    }

    // 等待后台任务完成
    match rx.await {
        Ok(Ok(response)) => {
            HttpResult::success(ChatResponse {
                session_id: session_id.clone(),
                response,
                status: "success".to_string(),
                error: None,
            })
        }
        Ok(Err(e)) => {
            error!("Failed to execute AI command: {}", e);
            HttpResult::<ChatResponse>::error("AI001", &format!("AI command execution failed: {}", e))
        }
        Err(_) => {
            error!("AI command execution cancelled");
            HttpResult::<ChatResponse>::error("AI002", "AI command execution cancelled")
        }
    }
}

/// 获取会话信息
#[tracing::instrument(skip(state), fields(session_id = %session_id))]
async fn get_session(
    State(state): State<SharedState>,
    Path(session_id): Path<String>,
) -> HttpResult<SessionInfo> {
    match state.sessions.get(&session_id) {
        Some(session) => HttpResult::success(session.clone()),
        None => HttpResult::error(
            "SES001", 
            &format!("Session '{}' not found", session_id),
        ),
    }
}

/// 获取用户的所有会话
async fn get_user_sessions(
    State(state): State<SharedState>,
    Path(user_id): Path<String>,
) -> HttpResult<Vec<SessionInfo>> {
    let user_sessions: Vec<SessionInfo> = state.sessions
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
            HttpResult::success(
                format!("Session '{}' deleted successfully", session_id),
            )
        }
        None => HttpResult::error(
            "SES002",
            &format!("Session '{}' not found", session_id),
        ),
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
        let mut senders = state.progress_senders.entry(session_id.clone()).or_insert_with(Vec::new);
        senders.push(tx.clone());
    }

    info!("新的 SSE 连接已建立为 session: {}", session_id);

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
        warn!("⚠️  未为session {} 找到ACP会话，无法订阅StreamUpdate事件", session_id);
    }

    // 订阅SessionNotification事件（来自Codex Agent）
    let tx_notification = tx.clone();
    let agent_manager = state.codex_manager.clone();
    let session_id_for_notifications = session_id.clone();
    tokio::spawn(async move {
        // 首先发送历史通知
        let historical_notifications = agent_manager.get_session_notifications(&session_id_for_notifications).await;
        for notification in historical_notifications {
            let progress_event = session_notification_to_progress_event(notification);
            if let Err(_) = tx_notification.send(progress_event) {
                break;
            }
        }

        // 注册SSE连接以接收实时通知
        let (notification_tx, mut notification_rx) = mpsc::unbounded_channel();
        if let Ok(_) = agent_manager.register_sse_connection(&session_id_for_notifications, notification_tx).await {
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
    let connect_event = ProgressEvent {
        event_type: ProgressEventType::KeepAlive,
        message: "SSE connection established".to_string(),
        timestamp: chrono::Utc::now(),
        session_id: session_id.clone(),
        data: None,
    };
    
    if let Err(e) = tx.send(connect_event) {
        error!("发送初始连接事件失败: {}", e);
    }
    
    // 创建流
    let stream = UnboundedReceiverStream::new(rx)
        .map(|event| {
            let json_data = serde_json::to_string(&event).unwrap_or_default();
            Ok(Event::default()
                .event(&event.event_type.to_string())
                .data(&json_data))
        });
    
    Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(30))
                .text("keep-alive-text")
        )
}

// ==================== 辅助函数 ====================

/// 创建新会话
async fn create_new_session(state: &SharedState, request: &ChatRequest) -> String {
    let session_id = Uuid::new_v4().to_string();
    let session_info = SessionInfo {
        session_id: session_id.clone(),
        user_id: request.user_id.clone(),
        project_id: request.project_id.clone(),
        agent_type: state.config.default_agent.clone(),
        created_at: chrono::Utc::now(),
        last_activity: chrono::Utc::now(),
    };

    state.sessions.insert(session_id.clone(), session_info);

    // 如果是 Codex agent，还需要创建 ACP 会话
    if state.config.default_agent == AgentType::Codex {
        let _project_path = if let Some(ref project_id) = request.project_id {
            state.config.projects_dir.join(project_id)
        } else {
            state.config.projects_dir.join("default")
        };

        info!("Created new Codex ACP session for HTTP session: {}", session_id);
    }

    info!("Created new session: {}", session_id);
    session_id
}

/// 更新会话活动时间
async fn update_session_activity(state: &SharedState, session_id: &str) {
    if let Some(mut session) = state.sessions.get_mut(session_id) {
        session.last_activity = chrono::Utc::now();
    }
}

/// 启动本地任务执行器
async fn start_local_task_executor(mut receiver: tokio::sync::mpsc::UnboundedReceiver<LocalTaskRequest>) {
    while let Some(task_request) = receiver.recv().await {
        // 在 LocalSet 中处理每个任务
        let result = execute_ai_command(
            &task_request.agent_type,
            &task_request.request,
            &task_request.config,
            &task_request.state,
            &task_request.session_id,
        ).await.map_err(|e| e.to_string());

        // 发送结果回调用者
        let _ = task_request.response_tx.send(result);
    }
}



/// 执行 AI 命令
#[tracing::instrument(skip(request, config, state), fields(agent_type = %agent_type, session_id = %session_id))]
async fn execute_ai_command(
    agent_type: &AgentType,
    request: &ChatRequest,
    config: &AppConfig,
    state: &SharedState,
    session_id: &str,
) -> anyhow::Result<String> {
    // 添加 span 属性
    let current_span = Span::current();
    current_span.record("ai.agent_type", agent_type.to_string());
    current_span.record("ai.prompt_length", request.prompt.len());
    
    // 发送任务开始事件
    let start_event = ProgressEvent {
        event_type: ProgressEventType::TaskStarted,
        message: format!("开始执行 AI 任务: {}", agent_type),
        timestamp: chrono::Utc::now(),
        session_id: session_id.to_string(),
        data: Some(serde_json::json!({
            "agent_type": agent_type.to_string(),
            "prompt": request.prompt
        })),
    };
    broadcast_progress_event(state, session_id, start_event);

    match agent_type {
        AgentType::Codex => {
            // 使用 MPMC 架构通过 ACP 协议调用 codex
            info!("Using Codex ACP protocol with MPMC architecture");

            // 获取项目ID
            let project_id = request.project_id
                .as_ref()
                .map(|id| id.as_str())
                .unwrap_or_else(|| {
                    // 如果没有提供项目ID，使用session_id作为项目ID
                    session_id
                });

            // 发送执行中事件
            let executing_event = ProgressEvent {
                event_type: ProgressEventType::Executing,
                message: format!("正在通过 MPMC 架构调用 Codex: {}", request.prompt),
                timestamp: chrono::Utc::now(),
                session_id: session_id.to_string(),
                data: Some(serde_json::json!({
                    "protocol": "ACP",
                    "architecture": "MPMC",
                    "agent": "codex",
                    "project_id": project_id,
                    "prompt": request.prompt
                })),
            };
            broadcast_progress_event(state, session_id, executing_event);

            // 使用全局 Codex 管理器处理请求
            let response = state.codex_manager.send_prompt(project_id, &request.prompt).await?;

            info!("Codex MPMC request completed successfully");
            Ok(response)
        }
        AgentType::Claude => {
            // Claude 仍然使用 shell 命令方式（保持向后兼容）
            info!("Using Claude shell command");

            let mut cmd = tokio::process::Command::new("claude");

            // 如果有项目 ID，设置工作目录
            if let Some(ref project_id) = request.project_id {
                let project_path = config.projects_dir.join(project_id);
                cmd.current_dir(project_path);
            }

            // 添加 prompt 作为参数
            cmd.arg(&request.prompt);

            info!("Executing Claude command: claude {:?}", request.prompt);

            // 发送执行中事件
            let executing_event = ProgressEvent {
                event_type: ProgressEventType::Executing,
                message: format!("正在执行 Claude 命令: {}", request.prompt),
                timestamp: chrono::Utc::now(),
                session_id: session_id.to_string(),
                data: Some(serde_json::json!({
                    "command": "claude",
                    "args": [request.prompt.clone()]
                })),
            };
            broadcast_progress_event(state, session_id, executing_event);

            // 执行命令并获取输出
            let output = cmd.output().await?;

            if output.status.success() {
                let response = String::from_utf8_lossy(&output.stdout).to_string();
                info!("Claude command completed successfully");
                Ok(response)
            } else {
                let error = String::from_utf8_lossy(&output.stderr).to_string();
                error!("Claude command failed: {}", error);
                Err(anyhow::anyhow!("Claude command failed: {}", error))
            }
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
    
    info!("Loaded config: default_agent={}, port={}, projects_dir={:?}", 
          config.default_agent, config.port, config.projects_dir);
    
    config
}

/// 创建 Axum 路由
fn create_router(state: SharedState) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/chat", post(handle_chat))
        .route("/chat/multipart", post(handle_multipart_chat))
        .route("/chat/acp-multipart", post(handle_acp_multipart_chat))
        .route("/sessions/{session_id}", get(get_session).delete(delete_session))
        .route("/users/{user_id}/sessions", get(get_user_sessions))
        .route("/progress/{session_id}", get(progress_stream))
        .layer(CorsLayer::permissive())
        // 自定义追踪中间件 - 自动生成和管理 trace_id
        .layer(axum::middleware::from_fn(middleware::tracing_middleware::tracing_middleware_handler))
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

    let state = Arc::new(AppState {
        sessions: Arc::new(DashMap::new()),
        config: config.clone(),
        progress_senders: Arc::new(DashMap::new()),
        session_manager,
        codex_manager: Arc::new(GlobalAgentManager::global()),
        local_task_sender,
    });

    // 创建路由
    let app = create_router(state.clone());

    // 启动 HTTP 服务器
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", config.port))
        .await
        .unwrap();

    info!("Server starting on port {}", config.port);
    info!("API endpoints:");
    info!("  POST /chat - Send chat message to AI agent");
    info!("  POST /chat/multipart - Send multipart chat with files (legacy)");
    info!("  POST /chat/acp-multipart - Send multipart chat with ACP native content blocks");
    info!("  GET  /sessions/:session_id - Get session info");
    info!("  GET  /users/:user_id/sessions - Get user's sessions");
    info!("  DELETE /sessions/:session_id - Delete session");
    info!("  GET  /progress/:session_id - SSE progress stream for AI tasks (unified stream)");
    info!("  GET  /health - Health check");
    info!("  NOTE: Plan data is delivered via the unified /progress/{{session_id}} SSE stream");

    // 在单独的线程中启动 LocalSet 任务执行器
    let local_task_handle = std::thread::spawn(move || {
        let local = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to build local runtime");

        local.block_on(async {
            let local_set = tokio::task::LocalSet::new();
            local_set.run_until(async {
                start_local_task_executor(local_task_receiver).await;
                Ok::<(), anyhow::Error>(())
            }).await
        }).expect("Local task executor failed");
    });

    // 运行 HTTP 服务器（使用多线程）
    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("Server error: {}", e))?;
    info!("Server shutdown complete");

    // 等待本地任务执行器线程结束
    local_task_handle.join().expect("Local task thread panicked");

    Ok(())
}

/// 初始化遥测系统
fn init_telemetry() -> anyhow::Result<()> {
    // 简化的 OpenTelemetry 设置，只使用 tracing 和基本的 span 功能
    // 设置全局文本传播器（用于 trace context 传播）
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new()
    );
    
    // 初始化 tracing subscriber
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rcoder=debug,tower_http=debug,axum_tracing_opentelemetry=info".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .init();
        
    info!("✓ Tracing 初始化成功，支持 trace_id 生成和传播");
    
    Ok(())
}