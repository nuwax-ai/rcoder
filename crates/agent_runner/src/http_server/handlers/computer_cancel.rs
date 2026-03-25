//! Computer Agent Cancel Handler
//!
//! 处理 POST /computer/agent/session/cancel 请求

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use sacp::schema::{CancelNotification, SessionId};
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::CancelNotificationRequestWrapper;
use crate::http_server::router::AppState;
use crate::service::AGENT_REGISTRY;
use shared_types::{ComputerAgentCancelRequest, ComputerAgentCancelResponse, HttpResult};

/// 取消 Computer Agent 会话任务
///
/// 直接操作 AGENT_REGISTRY 发送取消信号
#[utoipa::path(
    post,
    path = "/computer/agent/session/cancel",
    params(
        ComputerAgentCancelRequest
    ),
    responses(
        (status = 200, description = "Cancel request successful", body = HttpResult<ComputerAgentCancelResponse>),
        (status = 400, description = "Bad request - missing fields"),
        (status = 500, description = "Internal server error")
    ),
    tag = "Computer Agent"
)]
pub async fn handle_computer_cancel(
    State(_state): State<Arc<AppState>>,
    Query(request): Query<ComputerAgentCancelRequest>,
) -> Result<Json<HttpResult<ComputerAgentCancelResponse>>, (StatusCode, Json<HttpResult<String>>)> {
    info!(
        "🚫 [HTTP] Computer Agent 取消请求: user_id={}, project_id={}, session_id={:?}",
        request.user_id, request.project_id, request.session_id
    );

    // 1. 验证必填字段
    if request.user_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(HttpResult::error("VALIDATION_ERROR", "user_id is required")),
        ));
    }

    if request.project_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(HttpResult::error(
                "VALIDATION_ERROR",
                "project_id is required",
            )),
        ));
    }

    // 2. 查找 session_id (如果未提供,从 AGENT_REGISTRY 获取)
    let session_id = if let Some(sid) = request.session_id {
        sid
    } else {
        // 从 AGENT_REGISTRY 查找
        match AGENT_REGISTRY.get_agent_info(&request.project_id) {
            Some(info) => {
                // session_id 是 SessionId 类型,直接转换为 String
                info.session_id.to_string()
            }
            None => {
                // Agent 不存在,幂等返回成功
                info!(
                    "ℹ️  [HTTP] Agent 不存在,幂等返回成功: project_id={}",
                    request.project_id
                );
                let response = ComputerAgentCancelResponse {
                    success: true,
                    session_id: String::new(),
                };
                return Ok(Json(HttpResult::success(response)));
            }
        }
    };

    // 3. 从 AGENT_REGISTRY 获取 Agent 信息并发送取消信号
    if let Some(agent_info) = AGENT_REGISTRY.get_agent_info(&request.project_id) {
        // 获取 cancel_tx
        let cancel_tx = agent_info.cancel_tx.clone();

        // 释放读锁
        drop(agent_info);

        // 检查是否已经空闲或停止中(幂等性)
        if cancel_tx.is_closed() {
            info!(
                "ℹ️  [HTTP] Agent 已停止,cancel channel 已关闭: session_id={}",
                session_id
            );
        } else {
            // 创建取消通知
            let session_id_obj = SessionId::new(Arc::from(session_id.as_str()));
            let cancel_notification = CancelNotification::new(session_id_obj);

            // 创建 oneshot channel 接收取消结果 (HTTP 不等待结果,直接丢弃)
            let (result_tx, _result_rx) = oneshot::channel();
            let cancel_request = CancelNotificationRequestWrapper {
                cancel_notification,
                result_tx,
            };

            // 发送取消信号 (异步)
            match cancel_tx.send(cancel_request).await {
                Ok(_) => {
                    info!("[HTTP] 取消信号已发送: session_id={}", session_id);
                }
                Err(e) => {
                    warn!(
                        "⚠️  [HTTP] 发送取消信号失败: session_id={}, error={}",
                        session_id, e
                    );
                }
            }
        }
    } else {
        // Session 不存在,幂等返回成功
        info!(
            "ℹ️  [HTTP] Agent 不存在,幂等返回成功: session_id={}",
            session_id
        );
    }

    let response = ComputerAgentCancelResponse {
        success: true,
        session_id: session_id.clone(),
    };

    Ok(Json(HttpResult::success(response)))
}
