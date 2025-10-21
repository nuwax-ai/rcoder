//! Agent 停止处理器

use crate::{
    api::ApiState,
    models::{StopAgentRequest, StopAgentResponse},
    shutdown::ShutdownSignal,
};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde_json::{json, Value};
use tracing::{debug, error, info, warn};

/// 停止 Agent 处理器
pub async fn stop_agent(
    State(state): State<ApiState>,
    Json(request): Json<StopAgentRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<Value>)> {
    info!("收到停止 Agent 请求，项目ID: {}, 强制: {}",
          request.project_id,
          request.force);

    // 验证项目ID
    if state.config.project_id != request.project_id {
        warn!("项目ID不匹配，期望: {}, 实际: {}",
              state.config.project_id,
              request.project_id);
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "项目ID不匹配",
                "message": format!("此 Agent 服务管理项目 {}，无法停止项目 {}",
                                state.config.project_id,
                                request.project_id),
                "code": "PROJECT_ID_MISMATCH"
            })),
        ));
    }

    // 检查是否有活跃的会话
    let active_sessions = state.agent_manager.list_sessions().await;
    let processing_sessions = active_sessions.iter().filter(|session| {
        session.status == crate::agent::SessionStatus::Processing
    }).count();

    if processing_sessions > 0 && !request.force {
        warn!("有 {} 个会话正在处理中，需要强制停止", processing_sessions);

        return Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "存在活跃会话",
                "message": format!("有 {} 个会话正在处理中，请使用强制停止或等待完成", processing_sessions),
                "code": "ACTIVE_SESSIONS_EXIST",
                "active_sessions": processing_sessions
            })),
        ));
    }

    // 发送关闭信号
    let shutdown_signal = if request.force {
        ShutdownSignal::ForceStop
    } else {
        ShutdownSignal::Stop
    };

    if let Err(e) = state.shutdown_manager.send_shutdown(shutdown_signal).await {
        error!("发送关闭信号失败: {}", e);

        let response = StopAgentResponse {
            success: false,
            project_id: request.project_id,
            message: Some(format!("发送关闭信号失败: {}", e)),
        };

        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": "停止信号发送失败",
                "message": e.to_string(),
                "code": "SHUTDOWN_SIGNAL_FAILED"
            })),
        ));
    }

    info!("关闭信号已发送，项目ID: {}", request.project_id);

    // 等待一段时间让 Agent 优雅停止
    if !request.force {
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }

    // 提前克隆 request.project_id 避免移动问题
    let project_id_for_response = request.project_id.clone();
    let project_id_clone = request.project_id.clone();
    let project_id_for_log = request.project_id.clone();

    let response = StopAgentResponse {
        success: true,
        project_id: project_id_for_response,
        message: Some(if request.force {
            "Agent 正在强制停止".to_string()
        } else {
            "Agent 正在优雅停止".to_string()
        }),
    };

    // 异步执行实际停止操作
    let shutdown_manager_clone = state.shutdown_manager.clone();
    tokio::spawn(async move {
        info!("开始执行 Agent 停止操作，项目ID: {}", project_id_for_log);
        // 给客户端一些时间接收响应
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        // 实际的停止逻辑将在 shutdown_manager 处理信号时执行
    });

    Ok(Json(response))
}

/// 立即强制停止 Agent
pub async fn force_stop_agent(
    State(state): State<ApiState>,
) -> Result<impl IntoResponse, (StatusCode, Json<Value>)> {
    info!("收到立即强制停止 Agent 请求");

    let request = StopAgentRequest {
        project_id: state.config.project_id.clone(),
        force: true,
        reason: Some("立即强制停止".to_string()),
    };

    // 调用停止处理逻辑
    stop_agent(State(state), Json(request)).await
}

/// 获取停止状态
pub async fn get_stop_status(
    State(state): State<ApiState>,
) -> Result<impl IntoResponse, (StatusCode, Json<Value>)> {
    debug!("查询 Agent 停止状态");

    // 获取 Agent 状态
    let agent_status = state.agent_manager.get_agent_status().await;

    match agent_status {
        Ok(status) => {
            let is_shutting_down = matches!(
                status,
                crate::agent::AgentStatus::Stopping | crate::agent::AgentStatus::Stopped
            );

            let response = json!({
                "project_id": state.config.project_id,
                "agent_status": status,
                "is_shutting_down": is_shutting_down,
                "active_sessions": state.agent_manager.get_active_sessions_count().await,
                "timestamp": chrono::Utc::now()
            });

            Ok(Json(response))
        }
        Err(e) => {
            error!("获取 Agent 状态失败: {}", e);

            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "获取状态失败",
                    "message": e.to_string(),
                    "code": "STATUS_CHECK_FAILED"
                })),
            ))
        }
    }
}

/// 等待 Agent 完全停止
pub async fn wait_for_agent_stop(
    State(state): State<ApiState>,
) -> Result<impl IntoResponse, (StatusCode, Json<Value>)> {
    info!("等待 Agent 完全停止");

    // 检查当前状态
    let current_status = match state.agent_manager.get_agent_status().await {
        Ok(status) => status,
        Err(e) => {
            error!("获取 Agent 状态失败: {}", e);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "获取状态失败",
                    "message": e.to_string()
                }))
            ));
        }
    };

    if matches!(current_status, crate::agent::AgentStatus::Stopped) {
        return Ok(Json(json!({
            "success": true,
            "message": "Agent 已停止",
            "status": "stopped"
        })));
    }

    // 等待停止完成
    let mut check_count = 0;
    let max_checks = 30; // 最多等待30秒

    while check_count < max_checks {
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        check_count += 1;

        match state.agent_manager.get_agent_status().await {
            Ok(crate::agent::AgentStatus::Stopped) => {
                info!("Agent 已停止，等待时间: {} 秒", check_count);
                return Ok(Json(json!({
                    "success": true,
                    "message": "Agent 已停止",
                    "status": "stopped",
                    "wait_time_seconds": check_count
                })));
            }
            Ok(_) => {
                debug!("等待 Agent 停止，已等待 {} 秒", check_count);
            }
            Err(e) => {
                error!("检查 Agent 状态失败: {}", e);
                break;
            }
        }
    }

    // 超时
    warn!("等待 Agent 停止超时");

    Err((
        StatusCode::REQUEST_TIMEOUT,
        Json(json!({
            "error": "等待停止超时",
            "message": format!("等待 {} 秒后 Agent 仍未停止", max_checks),
            "code": "STOP_TIMEOUT",
            "wait_time_seconds": check_count
        })),
    ))
}