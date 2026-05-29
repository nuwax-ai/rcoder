//! CancelSession RPC 实现

use std::sync::Arc;

use agent_client_protocol::schema::StopReason;
use shared_types::grpc::{CancelRequest, CancelResponse, CancelResultType};
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, instrument, warn};

use crate::model::AgentStatus;
use crate::router::AppState;
use crate::service::{AGENT_REGISTRY, PERMISSION_MANAGER};

use super::cleanup::{
    CancelAndWaitResult, cleanup_session_full, close_session_connection, send_cancel_and_wait,
    send_session_prompt_end,
};
use super::locale::{locale_from_grpc_request, localized};

#[instrument(skip(app_state, request))]
pub async fn cancel_session(
    app_state: &Arc<AppState>,
    request: Request<CancelRequest>,
) -> Result<Response<CancelResponse>, Status> {
    let locale = locale_from_grpc_request(&request);
    shared_types::scope_request_locale(locale, async move {
        let req = request.into_inner();
        info!(
            "🛑 [gRPC] CancelSession: session_id={}, project_id={}, reason={}",
            req.session_id, req.project_id, req.reason
        );

        let actual_session_id = if req.session_id.is_empty() {
            info!(
                "📝 [gRPC] session_id is empty, looking up by project_id={}",
                req.project_id
            );

            match AGENT_REGISTRY.get_agent_info(&req.project_id) {
                Some(info) => {
                    let sid = info.session_id.to_string();
                    info!(
                        "✅ [gRPC] got session_id={} from project_id={}",
                        req.project_id, sid
                    );
                    sid
                }
                None => {
                    info!(
                        "ℹ️ [gRPC] project_id={} has no active session, cancel target achieved",
                        req.project_id
                    );
                    return Ok(Response::new(CancelResponse {
                        success: true,
                        result: CancelResultType::CancelResultSuccess as i32,
                        message: Some(localized(
                            locale,
                            "项目当前没有活跃会话",
                            "專案目前沒有活躍工作階段",
                            "Project has no active session",
                        )),
                    }));
                }
            }
        } else {
            req.session_id.clone()
        };

        let cancelled_permissions =
            PERMISSION_MANAGER.cancel_session_permissions(&actual_session_id);
        if cancelled_permissions > 0 {
            info!(
                "[gRPC] cancelled {} pending permission request(s): session_id={}",
                cancelled_permissions, actual_session_id
            );
        }

        let project_id = match AGENT_REGISTRY.get_project_by_session(&actual_session_id) {
            Some(pid) => {
                debug!(
                    "✅ [gRPC] found project_id={} for session_id={}",
                    actual_session_id, pid
                );
                pid
            }
            None => {
                warn!(
                    "⚠️ [gRPC] found no project for session_id={}",
                    actual_session_id
                );
                return Ok(Response::new(CancelResponse {
                    success: true,
                    result: CancelResultType::CancelResultSuccess as i32,
                    message: Some(localized(
                        locale,
                        "会话不存在或已完成",
                        "工作階段不存在或已完成",
                        "Session does not exist or already completed",
                    )),
                }));
            }
        };

        let (status, cancel_tx) = {
            let agent_info = match AGENT_REGISTRY.get_agent_info(&project_id) {
                Some(info) => info,
                None => {
                    info!(
                        "ℹ️ [gRPC] project_id={} has no active session, cancel target achieved (idempotent)",
                        project_id
                    );
                    return Ok(Response::new(CancelResponse {
                        success: true,
                        result: CancelResultType::CancelResultSuccess as i32,
                        message: Some(localized(
                            locale,
                            "项目当前没有活跃会话",
                            "專案目前沒有活躍工作階段",
                            "Project has no active session",
                        )),
                    }));
                }
            };

            let status = agent_info.status;
            let cancel_tx = agent_info.cancel_tx.clone();
            drop(agent_info);
            (status, cancel_tx)
        };

        match status {
            AgentStatus::Idle => {
                info!(
                    "✅ [gRPC] Agent already in Idle status, cancel request idempotent success: project_id={}, session_id={}",
                    project_id, actual_session_id
                );
                return Ok(Response::new(CancelResponse {
                    success: true,
                    result: CancelResultType::CancelResultSuccess as i32,
                    message: Some(localized(
                        locale,
                        "Agent 已处于空闲状态",
                        "Agent 已處於閒置狀態",
                        "Agent already in idle status",
                    )),
                }));
            }
            AgentStatus::Terminating => {
                info!(
                    "✅ [gRPC] Agent already stopping, cancel request idempotent success: project_id={}, session_id={}",
                    project_id, actual_session_id
                );
                return Ok(Response::new(CancelResponse {
                    success: true,
                    result: CancelResultType::CancelResultSuccess as i32,
                    message: Some(localized(
                        locale,
                        "Agent 已在停止中",
                        "Agent 已在停止中",
                        "Agent is already stopping",
                    )),
                }));
            }
            AgentStatus::Active | AgentStatus::Pending => {
                debug!(
                    "🔄 [gRPC] Agent status is {:?}, executing cancel: project_id={}, session_id={}",
                    status, project_id, actual_session_id
                );
            }
        }

        if cancel_tx.is_closed() {
            error!(
                "❌ [gRPC] cancel_tx channel closed, LocalSet may have unexpectedly exited: project_id={}, session_id={}",
                project_id, actual_session_id
            );
            return Ok(Response::new(CancelResponse {
                success: false,
                result: CancelResultType::CancelResultFailed as i32,
                message: Some(localized(
                    locale,
                    "取消通道已关闭，Agent 可能已停止",
                    "取消通道已關閉，Agent 可能已停止",
                    "Cancel channel closed, Agent may have stopped",
                )),
            }));
        }

        let cancel_timeout_secs = app_state
            .config
            .grpc_timeouts
            .as_ref()
            .map(|t| t.cancel_session_timeout_secs)
            .unwrap_or(30);

        match send_cancel_and_wait(&cancel_tx, &actual_session_id, cancel_timeout_secs).await {
            CancelAndWaitResult::Completed(cancel_result) => {
                let is_success = cancel_result.is_success();
                debug!(
                    "✅ [gRPC] received Agent cancel response: session_id={}, success={}",
                    actual_session_id, is_success
                );

                if !is_success {
                    return Ok(Response::new(CancelResponse {
                        success: false,
                        result: CancelResultType::CancelResultFailed as i32,
                        message: Some(localized(
                            locale,
                            "Agent 取消执行失败",
                            "Agent 取消執行失敗",
                            "Agent cancel execution failed",
                        )),
                    }));
                }
            }
            CancelAndWaitResult::ChannelClosed(e) => {
                error!(
                    "❌ [gRPC] Waiting for Agent cancel response channel closed: session_id={}, error={:?}",
                    actual_session_id, e
                );

                send_session_prompt_end(
                    &project_id,
                    &actual_session_id,
                    StopReason::Cancelled,
                    Some(format!(
                        "{}: {}",
                        localized(
                            locale,
                            "Agent 响应通道关闭",
                            "Agent 回應通道關閉",
                            "Agent response channel closed",
                        ),
                        e
                    )),
                )
                .await;
                close_session_connection(&actual_session_id).await;

                return Ok(Response::new(CancelResponse {
                    success: false,
                    result: CancelResultType::CancelResultFailed as i32,
                    message: Some(format!(
                        "{}: {}",
                        localized(locale, "响应通道关闭", "回應通道關閉", "Response channel closed"),
                        e
                    )),
                }));
            }
            CancelAndWaitResult::Timeout => {
                warn!(
                    "⚠️ [gRPC] Waiting for Agent cancel response timed out: session_id={}, actively cleaning up resources",
                    actual_session_id
                );

                cleanup_session_full(
                    &project_id,
                    &actual_session_id,
                    StopReason::Cancelled,
                    Some(localized(
                        locale,
                        "取消请求超时，主动清理资源",
                        "取消請求逾時，主動清理資源",
                        "Cancel request timed out; resources were cleaned up",
                    )),
                    true,
                    &[AgentStatus::Active],
                )
                .await;

                return Ok(Response::new(CancelResponse {
                    success: false,
                    result: CancelResultType::CancelResultTimeout as i32,
                    message: Some(localized(
                        locale,
                        "取消请求超时（30秒）",
                        "取消請求逾時（30 秒）",
                        "Cancel request timed out (30 seconds)",
                    )),
                }));
            }
            CancelAndWaitResult::SendFailed(e) => {
                error!("[gRPC] failed to send cancel notification: {}", e);
                return Ok(Response::new(CancelResponse {
                    success: false,
                    result: CancelResultType::CancelResultFailed as i32,
                    message: Some(format!(
                        "{}: {}",
                        localized(
                            locale,
                            "发送取消通知失败",
                            "發送取消通知失敗",
                            "Failed to send cancel notification",
                        ),
                        e
                    )),
                }));
            }
        }

        cleanup_session_full(
            &project_id,
            &actual_session_id,
            StopReason::Cancelled,
            None,
            true,
            &[AgentStatus::Active, AgentStatus::Pending],
        )
        .await;

        Ok(Response::new(CancelResponse {
            success: true,
            result: CancelResultType::CancelResultSuccess as i32,
            message: Some(localized(
                locale,
                "取消成功",
                "取消成功",
                "Cancelled successfully",
            )),
        }))
    })
    .await
}
