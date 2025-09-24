use dashmap::DashMap;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tracing::{Span, debug, error, info, instrument, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

mod handler;
mod model;
mod progress_events;
mod proxy_agent;

use model::*;
use progress_events::{
    ProgressEvent, ProgressEventSubType, ProgressEventType, SessionMessageManager,
};

use acp_adapter::SessionManager;
use acp_adapter::types::StreamUpdate;

use crate::handler::chat_handler::{AppConfig, AppState};

mod middleware;

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

// ==================== HTTP 处理器 ====================

// 聊天处理相关函数已移动到 handler::chat_handler 模块

// 健康检查函数已移动到 handler::health_handler 模块

/// 加载配置
fn load_config() -> AppConfig {
    let mut config = AppConfig::default();

    if let Ok(port) = std::env::var("PORT") {
        config.port = port.parse().unwrap_or(3000);
    }

    if let Ok(projects_dir) = std::env::var("PROJECTS_DIR") {
        config.projects_dir = PathBuf::from(projects_dir);
    }

    info!(
        "Loaded config: port={}, projects_dir={:?}",
        config.port, config.projects_dir
    );

    config
}

// 路由创建函数已移动到 handler 模块

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

    let state = Arc::new(AppState {
        sessions: Arc::new(DashMap::new()),
        config: config.clone(),
        message_manager: Arc::new(SessionMessageManager::new(1000)), // 缓存最近1000条消息
        local_task_sender,
    });

    // proxy_manager 不需要直接访问 app_state，通过参数传递即可

    // 创建路由
    let app = handler::create_router(state.clone());

    // 启动 HTTP 服务器
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", config.port))
        .await
        .unwrap();

    info!("Server starting on port {}", config.port);
    info!("API endpoints:");
    info!("  POST /chat - Send chat message to AI agent (legacy)");
    info!("  GET  /progress/:session_id - SSE progress stream for AI tasks (unified stream)");
    info!("  GET  /health - Health check");
    info!("  NOTE: Plan data is delivered via the unified /progress/{{session_id}} SSE stream");

    axum::serve(listener, app).await?;

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
