use anyhow::Result;
use axum::{
    extract::{Path, State, Multipart},
    response::{Json, Sse, sse::Event},
    routing::{get, post},
    Router,
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
use tracing::{info, warn, error};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;
use futures::stream::Stream;
use acp_adapter::mention::{ResourceUri, ResourceUriBuilder};

mod http_result;
use http_result::HttpResult;

mod multipart_chat;
use multipart_chat::{handle_multipart_chat, CodeSnippet};

// ==================== 数据结构定义 ====================

/// 用户请求结构 - 支持多媒体内容
#[derive(Debug, Deserialize)]
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

/// 应用状态
#[derive(Debug)]
struct AppState {
    /// 活跃的会话映射
    sessions: DashMap<String, SessionInfo>,
    /// 应用配置
    config: AppConfig,
    /// 进度事件广播通道（session_id -> 发送者列表）
    progress_senders: DashMap<String, Vec<mpsc::UnboundedSender<ProgressEvent>>>,
}

type SharedState = Arc<AppState>;

// ==================== 辅助函数 ====================

/// 获取当前请求的 trace_id
fn get_trace_id() -> Option<String> {
    // 使用 UUID v4 生成 trace_id，符合 OpenTelemetry 格式
    // UUID v4 的格式为 32 位十六进制字符串，符合 trace_id 的要求
    let uuid = Uuid::new_v4();
    // 移除连字符，转换为纯十六进制格式
    let trace_id = uuid.to_string().replace("-", "");
    Some(trace_id)
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
            drop(senders); // 释放引用
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

/// 处理聊天请求
async fn handle_chat(
    State(state): State<SharedState>,
    Json(mut request): Json<ChatRequest>,
) -> HttpResult<ChatResponse> {
    let trace_id = get_trace_id();
    
    info!(
        "Received chat request: user_id={}, project_id={:?}, session_id={:?}, trace_id={:?}",
        request.user_id, request.project_id, request.session_id, trace_id
    );

    // 如果没有提供 project_id，则生成一个 UUID v7
    if request.project_id.is_none() {
        let new_project_id = Uuid::now_v7().to_string();
        info!("Generated new project_id: {}", new_project_id);
        request.project_id = Some(new_project_id);
    }

    // 创建项目目录（如果不存在）
    if let Some(ref project_id) = request.project_id {
        let project_path = state.config.projects_dir.join(project_id);
        if !project_path.exists() {
            if let Err(e) = tokio::fs::create_dir_all(&project_path).await {
                error!("Failed to create project directory {:?}: {}", project_path, e);
                return HttpResult::error(
                    "DIR001",
                    &format!("Failed to create project directory: {}", e),
                    trace_id,
                );
            }
            info!("Created project directory: {:?}", project_path);
        }
    }

    // 获取或创建会话
    let session_id = match &request.session_id {
        Some(id) => {
            // 验证现有会话
            if state.sessions.contains_key(id) {
                id.clone()
            } else {
                warn!("Session {} not found, creating new session", id);
                create_new_session(&state, &request).await
            }
        }
        None => create_new_session(&state, &request).await,
    };

    // 获取会话信息以确定使用的代理类型
    let agent_type = {
        state.sessions.get(&session_id)
            .map(|s| s.agent_type.clone())
            .unwrap_or(state.config.default_agent.clone())
    };

    // 调用 AI 代理处理请求
    match execute_ai_command(&agent_type, &request, &state.config, &state, &session_id).await {
        Ok(response) => {
            // 更新会话活动时间
            update_session_activity(&state, &session_id).await;
            
            let chat_response = ChatResponse {
                session_id,
                response,
                status: "success".to_string(),
                error: None,
            };
            
            HttpResult::success(chat_response, trace_id)
        }
        Err(e) => {
            error!("AI command execution failed: {}", e);
            HttpResult::error(
                "AI001",
                &format!("AI command execution failed: {}", e),
                trace_id,
            )
        }
    }
}

/// 获取会话信息
async fn get_session(
    State(state): State<SharedState>,
    Path(session_id): Path<String>,
) -> HttpResult<SessionInfo> {
    let trace_id = get_trace_id();
    
    match state.sessions.get(&session_id) {
        Some(session) => HttpResult::success(session.clone(), trace_id),
        None => HttpResult::error(
            "SES001", 
            &format!("Session '{}' not found", session_id),
            trace_id,
        ),
    }
}

/// 获取用户的所有会话
async fn get_user_sessions(
    State(state): State<SharedState>,
    Path(user_id): Path<String>,
) -> HttpResult<Vec<SessionInfo>> {
    let trace_id = get_trace_id();
    
    let user_sessions: Vec<SessionInfo> = state.sessions
        .iter()
        .filter(|entry| entry.value().user_id == user_id)
        .map(|entry| entry.value().clone())
        .collect();
        
    HttpResult::success(user_sessions, trace_id)
}

/// 删除会话
async fn delete_session(
    State(state): State<SharedState>,
    Path(session_id): Path<String>,
) -> HttpResult<String> {
    let trace_id = get_trace_id();
    
    match state.sessions.remove(&session_id) {
        Some(_) => {
            info!("Session {} deleted", session_id);
            HttpResult::success(
                format!("Session '{}' deleted successfully", session_id),
                trace_id,
            )
        }
        None => HttpResult::error(
            "SES002",
            &format!("Session '{}' not found", session_id),
            trace_id,
        ),
    }
}

/// 健康检查端点
async fn health_check() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "healthy",
        "timestamp": chrono::Utc::now(),
        "service": "rcoder-ai-service"
    }))
}

/// SSE 进度推送端点
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
    
    info!("Created new session: {}", session_id);
    session_id
}

/// 更新会话活动时间
async fn update_session_activity(state: &SharedState, session_id: &str) {
    if let Some(mut session) = state.sessions.get_mut(session_id) {
        session.last_activity = chrono::Utc::now();
    }
}

/// 执行 AI 命令
async fn execute_ai_command(
    agent_type: &AgentType,
    request: &ChatRequest,
    config: &AppConfig,
    state: &SharedState,
    session_id: &str,
) -> anyhow::Result<String> {
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

    let command = match agent_type {
        AgentType::Codex => "codex",
        AgentType::Claude => "claude",
    };

    let mut cmd = tokio::process::Command::new(command);
    
    // 如果有项目 ID，设置工作目录
    if let Some(ref project_id) = request.project_id {
        let project_path = config.projects_dir.join(project_id);
        cmd.current_dir(project_path);
    }
    
    // 添加 prompt 作为参数
    cmd.arg(&request.prompt);
    
    info!("Executing command: {} {:?}", command, request.prompt);
    
    // 发送执行中事件
    let executing_event = ProgressEvent {
        event_type: ProgressEventType::Executing,
        message: format!("正在执行命令: {} {}", command, request.prompt),
        timestamp: chrono::Utc::now(),
        session_id: session_id.to_string(),
        data: Some(serde_json::json!({
            "command": command,
            "args": [request.prompt.clone()]
        })),
    };
    broadcast_progress_event(state, session_id, executing_event);
    
    // 执行命令并获取输出
    let output = cmd.output().await?;
    
    if output.status.success() {
        let response = String::from_utf8_lossy(&output.stdout).to_string();
        info!("AI command completed successfully");
        
        // 发送任务完成事件
        let completed_event = ProgressEvent {
            event_type: ProgressEventType::TaskCompleted,
            message: "AI 任务执行成功".to_string(),
            timestamp: chrono::Utc::now(),
            session_id: session_id.to_string(),
            data: Some(serde_json::json!({
                "response_length": response.len(),
                "success": true
            })),
        };
        broadcast_progress_event(state, session_id, completed_event);
        
        Ok(response)
    } else {
        let error = String::from_utf8_lossy(&output.stderr).to_string();
        error!("AI command failed: {}", error);
        
        // 发送任务失败事件
        let failed_event = ProgressEvent {
            event_type: ProgressEventType::TaskFailed,
            message: format!("AI 任务执行失败: {}", error),
            timestamp: chrono::Utc::now(),
            session_id: session_id.to_string(),
            data: Some(serde_json::json!({
                "error": error,
                "success": false
            })),
        };
        broadcast_progress_event(state, session_id, failed_event);
        
        Err(anyhow::anyhow!("AI command failed: {}", error))
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
        .route("/sessions/{session_id}", get(get_session).delete(delete_session))
        .route("/users/{user_id}/sessions", get(get_user_sessions))
        .route("/progress/{session_id}", get(progress_stream))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

#[tokio::main]
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
    let state = Arc::new(AppState {
        sessions: DashMap::new(),
        config: config.clone(),
        progress_senders: DashMap::new(),
    });

    // 创建路由
    let app = create_router(state);

    // 启动 HTTP 服务器
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", config.port))
        .await
        .unwrap();
    
    info!("Server starting on port {}", config.port);
    info!("API endpoints:");
    info!("  POST /chat - Send chat message to AI agent");
    info!("  GET  /sessions/:session_id - Get session info");
    info!("  GET  /users/:user_id/sessions - Get user's sessions");
    info!("  DELETE /sessions/:session_id - Delete session");
    info!("  GET  /progress/:session_id - SSE progress stream for AI tasks");
    info!("  GET  /health - Health check");
    
    // 启动服务器
    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("Server error: {}", e))?;

    info!("Server shutdown complete");
    
    Ok(())
}

/// 初始化遥测系统
fn init_telemetry() -> anyhow::Result<()> {
    // 混合方案：初始化 tracing subscriber，但使用 OpenTelemetry 的 trace_id 生成
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rcoder=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
        
    info!("✓ Tracing 初始化成功，支持 trace_id 生成");
    
    Ok(())
}