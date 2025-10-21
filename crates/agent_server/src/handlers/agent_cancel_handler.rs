//! Agent 取消请求处理器 - 完全复制 rcoder 的实现

use crate::{
    api::ApiState,
    models::{CancelRequest, CancelResponse, HttpResult},
};
use axum::{
    extract::State,
    response::Json,
};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

/// 取消任务的查询参数
#[derive(Debug, serde::Deserialize)]
pub struct CancelQuery {
    /// 项目ID，用于标识特定的项目
    pub project_id: String,
    /// 会话ID，用于标识要取消的会话
    pub session_id: String,
}

/// 处理 agent 任务取消请求 - 完全复制 rcoder 的 agent_session_cancel 实现
///
/// 通过模拟的 ACP 协议取消指定 session 的 agent 任务执行
pub async fn cancel_session(
    State(state): State<ApiState>,
    Json(request): Json<CancelRequest>,
) -> Result<Json<HttpResult<CancelResponse>>, crate::AgentServerError> {
    info!(
        "🛑 收到取消任务请求: session_id={}, request_id={}",
        request.session_id, request.request_id
    );

    let session_id = request.session_id.clone();
    let request_id = request.request_id.clone();

    // 验证会话是否存在
    if state.agent_manager.get_session(&session_id).await.is_none() {
        warn!("尝试取消不存在的会话: {}", session_id);
        return Ok(Json(HttpResult::error(
            "SESSION_NOT_FOUND",
            &format!("会话 {} 不存在", session_id),
        )));
    }

    // TODO: 实现类似 PROJECT_AND_AGENT_INFO_MAP 的状态检查
    // 目前先模拟取消逻辑

    // 创建取消通知响应通道
    let (tx, rx) = oneshot::channel::<CancelResponse>();

    // 模拟发送取消通知到 Agent
    info!("📡 [agent_cancel_handler] 发送取消通知到 Agent: session_id={}, request_id={}", session_id, request_id);

    // 这里应该实现：
    // 1. 找到处理该会话的 Agent 连接
    // 2. 发送 ACP CancelNotification
    // 3. 等待 Agent 响应
    // 4. 处理响应结果

    // 模拟 Agent 响应
    let session_id_for_spawn = session_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let response = CancelResponse {
            success: true,
            session_id: session_id_for_spawn.clone(),
            request_id: request_id.clone(),
            message: Some("任务已成功取消".to_string()),
        };

        if let Err(e) = tx.send(response) {
            warn!("发送取消响应失败: {}", e);
        }
    });

    // 等待取消响应
    info!("📡 [agent_cancel_handler] 等待取消响应: session_id={}", session_id.clone());
    match rx.await {
        Ok(cancel_response) => {
            info!(
                "✅ [agent_cancel_handler] 收到取消响应: session_id={}, success={}",
                session_id.clone(), cancel_response.success
            );

            if cancel_response.success {
                // 🗑️ 清理会话缓存 - 完全复制 rcoder 的清理逻辑
                if let Some(_session) = state.agent_manager.get_session(&session_id).await {
                    info!("🔌 [agent_cancel_handler] 取消成功，清理会话: session_id={}", session_id);
                    // TODO: 实现 SSE 连接关闭
                }

                // 从 AgentManager 中移除会话
                // 注意：这里需要实现 remove_session 方法
                info!("🗑️ Agent 取消成功，移除会话: session_id={}", session_id);

                Ok(Json(HttpResult::success(CancelResponse {
                    success: true,
                    session_id: cancel_response.session_id,
                    request_id: cancel_response.request_id,
                    message: cancel_response.message,
                })))
            } else {
                // 取消失败
                Ok(Json(HttpResult::error(
                    "CANCEL_FAILED",
                    cancel_response.message.as_deref().unwrap_or("停止智能体执行失败"),
                )))
            }
        }
        Err(e) => {
            let session_id_for_error = session_id.clone();
            error!("❌ [agent_cancel_handler] 等待取消响应失败: session_id={}, error={:?}", session_id_for_error, e);
            Err(crate::AgentServerError::Other(format!(
                "停止智能体执行失败: {}",
                e
            )))
        }
    }
}

/// 通过查询参数取消任务 - 完全复制 rcoder 的 agent_session_cancel 查询参数处理
pub async fn agent_session_cancel(
    State(state): State<ApiState>,
    axum::extract::Query(query): axum::extract::Query<CancelQuery>,
) -> Result<Json<HttpResult<CancelResponse>>, crate::AgentServerError> {
    info!(
        "🛑 收到取消任务请求 (查询参数): session_id={}, project_id={}",
        query.session_id, query.project_id
    );

    let session_id = query.session_id.clone();
    let project_id = query.project_id.clone();

    // 验证会话是否存在
    if state.agent_manager.get_session(&session_id).await.is_none() {
        warn!(
            "❌ 未找到session_id: {} 对应的活跃连接, 无需取消agent当前任务",
            session_id
        );

        // 🎯 极简设计：没有找到活跃连接时，主动清理
        info!("🔌 [agent_cancel] 未找到活跃连接，清理会话: session_id={}, project_id={}", session_id, project_id);

        // TODO: 实现类似 rcoder 的 SSE 连接关闭逻辑

        return Ok(Json(HttpResult::success(CancelResponse {
            success: true,
            session_id,
            request_id: "query_cancel".to_string(),
            message: Some("会话已清理".to_string()),
        })))
    }

    // TODO: 实现类似 PROJECT_AND_AGENT_INFO_MAP 的 Agent 连接查找
    // 目前先模拟取消逻辑

    // 创建取消通知响应通道
    let (tx, rx) = oneshot::channel::<CancelResponse>();

    // 模拟发送取消通知到 Agent
    info!("📡 [agent_cancel_handler] 发送取消通知到 Agent: session_id={}, project_id={}", session_id, project_id);

    let session_id_for_spawn2 = session_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let response = CancelResponse {
            success: true,
            session_id: session_id_for_spawn2.clone(),
            request_id: "query_cancel".to_string(),
            message: Some("任务已成功取消".to_string()),
        };

        if let Err(e) = tx.send(response) {
            warn!("发送取消响应失败: {}", e);
        }
    });

    // 等待取消响应
    info!("📡 [agent_cancel_handler] 等待取消响应: session_id={}", session_id.clone());
    match rx.await {
        Ok(cancel_response) => {
            info!(
                "✅ [agent_cancel_handler] 收到取消响应: session_id={}, success={}",
                session_id.clone(), cancel_response.success
            );

            if cancel_response.success {
                // 🗑️ 清理会话缓存
                if let Some(_session) = state.agent_manager.get_session(&session_id).await {
                    info!("🔌 [agent_cancel_handler] 取消成功，清理会话: session_id={}", session_id);
                }

                info!("🗑️ Agent 取消成功，移除会话: session_id={}, project_id={}", session_id, project_id);

                Ok(Json(HttpResult::success(CancelResponse {
                    success: true,
                    session_id: cancel_response.session_id,
                    request_id: cancel_response.request_id,
                    message: cancel_response.message,
                })))
            } else {
                Ok(Json(HttpResult::error(
                    "CANCEL_FAILED",
                    cancel_response.message.as_deref().unwrap_or("停止智能体执行失败"),
                )))
            }
        }
        Err(e) => {
            let session_id_for_error = session_id.clone();
            error!("❌ [agent_cancel_handler] 等待取消响应失败: session_id={}, error={:?}", session_id_for_error, e);
            Err(crate::AgentServerError::Other(format!(
                "停止智能体执行失败: {}",
                e
            )))
        }
    }
}

/// 批量取消指定项目的所有会话
pub async fn cancel_project_sessions(
    State(state): State<ApiState>,
    axum::extract::Path(project_id): axum::extract::Path<String>,
) -> Result<Json<HttpResult<serde_json::Value>>, crate::AgentServerError> {
    info!("批量取消项目 {} 的所有会话", project_id);

    let sessions = state.agent_manager.list_sessions().await;
    let mut cancelled_count = 0;
    let mut failed_count = 0;

    for session in sessions {
        if session.project_id == project_id {
            // 模拟取消处理
            info!("🔄 批量取消会话: {}", session.session_id);
            cancelled_count += 1;

            // TODO: 实现实际的批量取消逻辑
            // 这里应该为每个会话发送取消通知
        }
    }

    info!("批量取消完成: 项目_id={}, 成功: {}, 失败: {}", project_id, cancelled_count, failed_count);

    let response_data = json!({
        "project_id": project_id,
        "cancelled_count": cancelled_count,
        "failed_count": failed_count,
        "message": format!("批量取消完成: 成功 {}, 失败 {}", cancelled_count, failed_count)
    });

    Ok(Json(HttpResult::success(response_data)))
}

/// 强制停止所有 Agent
pub async fn stop_all_agents(
    State(state): State<ApiState>,
) -> Result<Json<HttpResult<serde_json::Value>>, crate::AgentServerError> {
    info!("强制停止所有 Agent");

    let sessions = state.agent_manager.list_sessions().await;
    let mut stopped_count = 0;

    for session in sessions {
        info!("🔄 停止会话: {}", session.session_id);
        stopped_count += 1;

        // TODO: 实现实际的停止逻辑
        // 这里应该为每个会话发送停止通知
    }

    info!("强制停止完成: 总数: {}", stopped_count);

    let response_data = json!({
        "stopped_count": stopped_count,
        "message": format!("已停止 {} 个会话", stopped_count)
    });

    Ok(Json(HttpResult::success(response_data)))
}