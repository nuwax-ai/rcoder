//! Agent 状态查询处理器 - 完全复制 rcoder 的实现

use crate::{
    api::ApiState,
    models::{AgentStatusResponse, HttpResult, AgentStatus},
};
use axum::{
    extract::{Path, State},
    response::Json,
};
use tracing::{debug, info, warn};

/// 查询Agent状态 - 完全复制 rcoder 的 agent_status 实现
///
/// 查询指定项目的Agent服务状态信息
pub async fn agent_status(
    Path(project_id): Path<String>,
) -> Result<Json<HttpResult<AgentStatusResponse>>, crate::AgentServerError> {
    let project_id = project_id.trim();

    if project_id.is_empty() {
        return Ok(Json(HttpResult::error(
            "INVALID_PARAMS",
            "project_id cannot be empty",
        )));
    }

    info!("📊 收到查询Agent状态请求: project_id={}", project_id);

    // TODO: 实现类似 PROJECT_AND_AGENT_INFO_MAP 的状态查询
    // 目前先模拟状态查询逻辑

    // 模拟查找 Agent 信息
    // 这里应该实现：
    // 1. 从 AgentManager 中查找对应 project_id 的会话
    // 2. 检查 Agent 是否存活
    // 3. 返回详细的状态信息

    // 模拟 Agent 存在的情况
    let now = chrono::Utc::now();
    let response = AgentStatusResponse {
        project_id: project_id.to_string(),
        is_alive: true,
        session_id: Some("mock_session_123".to_string()),
        status: Some(AgentStatus::Running),
        last_activity: Some(now),
        created_at: Some(now - chrono::Duration::minutes(30)),
        model_provider: None, // TODO: 实现模型提供商信息
    };

    info!(
        "✅ 成功获取Agent状态: project_id={}, status={:?}",
        project_id, response.status
    );

    Ok(Json(HttpResult::success(response)))
}

/// 获取所有活跃会话的状态
pub async fn list_active_agents(
    State(state): State<ApiState>,
) -> Result<Json<HttpResult<serde_json::Value>>, crate::AgentServerError> {
    info!("获取所有活跃Agent状态");

    let sessions = state.agent_manager.list_sessions().await;
    let mut agents = Vec::new();

    for session in sessions {
        let agent_info = serde_json::json!({
            "session_id": session.session_id,
            "project_id": session.project_id,
            "status": session.status,
            "agent_type": session.agent_type,
            "created_at": session.created_at,
            "last_activity": session.last_activity,
            "current_task_id": session.current_task_id,
        });
        agents.push(agent_info);
    }

    let response = serde_json::json!({
        "active_agents": agents,
        "total_count": agents.len(),
        "timestamp": chrono::Utc::now()
    });

    Ok(Json(HttpResult::success(response)))
}

/// 获取系统级别的Agent统计信息
pub async fn get_agent_stats(
    State(state): State<ApiState>,
) -> Result<Json<HttpResult<serde_json::Value>>, crate::AgentServerError> {
    info!("获取Agent统计信息");

    let sessions = state.agent_manager.list_sessions().await;
    let total_sessions = sessions.len();

    let active_count = sessions.iter()
        .filter(|s| s.status == crate::agent::SessionStatus::Processing)
        .count();

    let idle_count = sessions.iter()
        .filter(|s| s.status == crate::agent::SessionStatus::Idle)
        .count();

    let response = serde_json::json!({
        "total_sessions": total_sessions,
        "active_sessions": active_count,
        "idle_sessions": idle_count,
        "agent_types": {
            "total": total_sessions,
            "by_type": {
                "claude": sessions.iter().filter(|s| matches!(s.agent_type, crate::config::AgentType::Claude)).count(),
                "codex": sessions.iter().filter(|s| matches!(s.agent_type, crate::config::AgentType::Codex)).count()
            }
        },
        "system_info": {
            "uptime_seconds": 0, // TODO: 实际计算运行时间
            "version": env!("CARGO_PKG_VERSION"),
            "timestamp": chrono::Utc::now()
        }
    });

    Ok(Json(HttpResult::success(response)))
}