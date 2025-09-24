use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post},
};
use dashmap::DashMap;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::{
    config::AppConfig, handler, proxy_agent::LocalSetAgentRequest, service::SessionMessageManager,
};

/// 会话信息结构
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub user_id: String,
    pub project_id: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_activity: chrono::DateTime<chrono::Utc>,
}

/// 应用状态
#[derive(Clone, Debug)]
pub struct AppState {
    /// 活跃的会话映射, project_id -> SessionInfo
    pub sessions: Arc<DashMap<String, SessionInfo>>,
    /// 应用配置
    pub config: AppConfig,

    /// 进度事件消息管理器 - 为每个 project_id 维护循环数组缓存
    pub message_manager: Arc<SessionMessageManager>,

    /// 本地任务发送器
    pub local_task_sender: mpsc::UnboundedSender<LocalSetAgentRequest>,
}

/// 创建 Axum 路由
pub fn create_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(handler::health_check))
        .route("/chat", post(handler::handle_chat))
        .route(
            "/agent/progress/{session_id}",
            get(handler::agent_session_notification),
        )
        .with_state(state)
}
