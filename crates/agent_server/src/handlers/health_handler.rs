//! 健康检查处理器 - 完全复制 rcoder 的实现

use crate::{
    api::ApiState,
    models::{HealthResponse, HealthStatus, HealthCheck},
};
use axum::{extract::State, response::Json};
use chrono::Utc;
use std::collections::HashMap;
use tracing::{debug, info};

/// 健康检查响应结构
#[derive(Debug, serde::Serialize)]
pub struct ServiceHealthResponse {
    /// 服务状态
    pub status: String,
    /// 时间戳
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// 服务名称
    pub service: String,
    /// 版本信息
    pub version: String,
}

/// 健康检查端点 - 完全复制 rcoder 的实现
///
/// 检查服务的健康状态
pub async fn health_check() -> Json<ServiceHealthResponse> {
    Json(ServiceHealthResponse {
        status: "healthy".to_string(),
        timestamp: Utc::now(),
        service: "agent-server".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

/// 详细健康检查端点 - 包含 Agent 状态检查
pub async fn detailed_health_check(
    State(state): State<ApiState>,
) -> Result<Json<HealthResponse>, crate::AgentServerError> {
    debug!("执行详细健康检查");
    info!("Health check requested");

    let start_time = std::time::Instant::now();
    let now = chrono::Utc::now();

    // 执行各项检查
    let mut checks = HashMap::new();

    // Agent 健康检查
    let agent_check_start = std::time::Instant::now();
    let agent_healthy = state.agent_manager.health_check().await.unwrap_or(false);
    let agent_check = HealthCheck {
        status: if agent_healthy { crate::models::HealthStatus::Healthy } else { crate::models::HealthStatus::Unhealthy },
        message: if agent_healthy {
            "Agent 运行正常".to_string()
        } else {
            "Agent 未运行或异常".to_string()
        },
        duration_ms: agent_check_start.elapsed().as_millis() as u64,
        timestamp: now,
    };
    checks.insert("agent".to_string(), agent_check);

    // 工作目录检查
    let workspace_check_start = std::time::Instant::now();
    let workspace_exists = state.config.work_dir.exists();
    let workspace_writable = workspace_exists && check_directory_writable(&state.config.work_dir);
    let workspace_check = HealthCheck {
        status: if workspace_writable { crate::models::HealthStatus::Healthy } else { crate::models::HealthStatus::Unhealthy },
        message: if workspace_writable {
            "工作目录可读写".to_string()
        } else {
            "工作目录不可访问".to_string()
        },
        duration_ms: workspace_check_start.elapsed().as_millis() as u64,
        timestamp: now,
    };
    checks.insert("workspace".to_string(), workspace_check);

    // 确定整体状态
    let overall_status = if checks.values().all(|check| matches!(check.status, crate::models::HealthStatus::Healthy)) {
        crate::models::HealthStatus::Healthy
    } else if checks.values().any(|check| matches!(check.status, crate::models::HealthStatus::Unhealthy)) {
        crate::models::HealthStatus::Unhealthy
    } else {
        crate::models::HealthStatus::Warning
    };

    // 获取活跃会话数
    let active_sessions = state.agent_manager.get_active_sessions_count().await;

    let response = HealthResponse {
        status: overall_status,
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: now - chrono::Duration::seconds(0), // TODO: 从实际启动时间计算
        uptime_seconds: 0, // TODO: 从实际启动时间计算
        active_sessions,
        total_requests: 0, // TODO: 实现请求计数器
        checks,
    };

    let duration = start_time.elapsed();
    debug!("健康检查完成，耗时: {:?}", duration);

    Ok(Json(response))
}

/// 检查目录是否可写
fn check_directory_writable(path: &std::path::Path) -> bool {
    let test_file = path.join(".rcoder_write_test");
    match std::fs::write(&test_file, "test") {
        Ok(_) => {
            let _ = std::fs::remove_file(&test_file);
            true
        }
        Err(_) => false,
    }
}