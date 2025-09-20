use anyhow::Result;
use axum::{
    extract::{Path, State},
    response::Json,
    routing::{get, post},
    Router,
};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::Arc,
};
use tower_http::cors::CorsLayer;
use tracing::{info, warn, error};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

mod http_result;
use http_result::HttpResult;

// ==================== 数据结构定义 ====================

/// 用户请求结构
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
    match execute_ai_command(&agent_type, &request, &state.config).await {
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
) -> Result<String> {
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
    
    // 执行命令并获取输出
    let output = cmd.output().await?;
    
    if output.status.success() {
        let response = String::from_utf8_lossy(&output.stdout).to_string();
        info!("AI command completed successfully");
        Ok(response)
    } else {
        let error = String::from_utf8_lossy(&output.stderr).to_string();
        error!("AI command failed: {}", error);
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
        .route("/sessions/{session_id}", get(get_session).delete(delete_session))
        .route("/users/{user_id}/sessions", get(get_user_sessions))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

#[tokio::main]
async fn main() -> Result<()> {
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
    info!("  GET  /health - Health check");
    
    // 启动服务器
    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("Server error: {}", e))?;

    info!("Server shutdown complete");
    
    Ok(())
}

/// 初始化遥测系统
fn init_telemetry() -> Result<()> {
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