//! Computer Agent Container Status Handler
//!
//! 处理 GET /computer/agent/container-status 请求
//! 返回 agent_runner 进程级别的状态信息

use axum::{Json, extract::State};
use serde::Serialize;
use std::sync::Arc;
use std::time::Instant;

use crate::http_server::router::AppState;
use crate::service::AGENT_REGISTRY;
use shared_types::{AppError, HttpResult};

/// 容器状态响应
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContainerStatusResponse {
    pub pid: u32,
    pub active_sessions: usize,
    pub total_registered: usize,
    pub uptime_seconds: u64,
    pub status: String,
}

/// 进程启动时间（lazy init）
static START_TIME: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn start_time() -> &'static Instant {
    START_TIME.get_or_init(Instant::now)
}

/// 获取 agent_runner 容器/进程状态
///
/// 返回进程 PID、活跃会话数、注册会话数、运行时间等。
#[utoipa::path(
    get,
    path = "/computer/agent/container-status",
    responses(
        (status = 200, description = "Container status", body = HttpResult<ContainerStatusResponse>),
    ),
    tag = "Computer Agent"
)]
pub async fn handle_computer_container_status(
    State(_state): State<Arc<AppState>>,
) -> Result<Json<HttpResult<ContainerStatusResponse>>, AppError> {
    let stats = AGENT_REGISTRY.stats();

    let response = ContainerStatusResponse {
        pid: std::process::id(),
        active_sessions: stats.session_count,
        total_registered: stats.agent_count,
        uptime_seconds: start_time().elapsed().as_secs(),
        status: "running".to_string(),
    };

    Ok(Json(HttpResult::success(response)))
}
