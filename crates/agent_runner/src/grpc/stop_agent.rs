//! StopAgent RPC 实现

use std::sync::Arc;

use agent_client_protocol::schema::StopReason;
use shared_types::grpc::{StopAgentRequest, StopAgentResponse};
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, instrument, warn};

use crate::model::AgentStatus;
use crate::router::AppState;
use crate::service::{AGENT_REGISTRY, PERMISSION_MANAGER};

use super::cleanup::{
    CancelAndWaitResult, close_session_connection, remove_agent_and_cleanup,
    remove_session_from_cache, send_cancel_and_wait, send_session_prompt_end,
};
use super::locale::{locale_from_grpc_request, localized};

fn cleanup_api_keys(app_state: &Arc<AppState>, project_id: &str) {
    if let Some((_, uuid)) = app_state.project_uuid_map.remove(project_id) {
        if let Some((key, config)) = app_state.shared_api_key_manager.remove(&uuid) {
            info!(
                "🗑️ [gRPC] Cleaned up API key config: uuid={}, provider_name={}",
                key, config.name
            );
        }
        info!(
            "🗑️ [gRPC] Cleaned up project UUID mapping: project_id={}, uuid={}",
            project_id, uuid
        );
    } else {
        debug!(
            "🔍 [gRPC] project UUID mapping not found: project_id={}",
            project_id
        );
    }
}

#[instrument(skip(app_state, request))]
pub async fn stop_agent(
    app_state: &Arc<AppState>,
    request: Request<StopAgentRequest>,
) -> Result<Response<StopAgentResponse>, Status> {
    let locale = locale_from_grpc_request(&request);
    shared_types::scope_request_locale(locale, async move {
        let req = request.into_inner();
        let project_id = req.project_id.clone();
        let force = req.force;
        let reason = req
            .reason
            .clone()
            .unwrap_or_else(|| {
                localized(
                    locale,
                    "用户请求停止",
                    "使用者請求停止",
                    "Stop requested by user",
                )
            });

        info!(
            "🛑 [gRPC] StopAgent: project_id={}, force={}, reason={}",
            project_id, force, reason
        );

        let cancelled_permissions = PERMISSION_MANAGER.cancel_project_permissions(&project_id);
        if cancelled_permissions > 0 {
            info!(
                "[gRPC] cancelled {} pending permission request(s): project_id={}",
                cancelled_permissions, project_id
            );
        }

        let (agent_status, session_id, cancel_tx) = match AGENT_REGISTRY.get_agent_info(&project_id)
        {
            Some(info) => {
                let status = info.status;
                let session_id = info.session_id.to_string();
                let cancel_tx = info.cancel_tx.clone();
                (status, session_id, cancel_tx)
            }
            None => {
                info!("📭 [gRPC] Agent not found: project_id={}", project_id);
                return Ok(Response::new(StopAgentResponse {
                    success: true,
                    result: "not_found".to_string(),
                    message: Some(format!(
                        "{} {}",
                        localized(
                            locale,
                            "项目的 Agent 不存在或已停止:",
                            "專案 Agent 不存在或已停止:",
                            "Agent not found or already stopped for project:",
                        ),
                        project_id
                    )),
                    project_id,
                }));
            }
        };

        if agent_status == AgentStatus::Terminating {
            info!("ℹ️ [gRPC] Agent is already stopping: project_id={}", project_id);

            if !session_id.is_empty() {
                send_session_prompt_end(
                    &project_id,
                    &session_id,
                    StopReason::Cancelled,
                    Some(localized(
                        locale,
                        "Agent 已在停止中",
                        "Agent 已在停止中",
                        "Agent is already stopping",
                    )),
                )
                .await;
                close_session_connection(&session_id).await;
            }

            return Ok(Response::new(StopAgentResponse {
                success: true,
                result: "already_stopped".to_string(),
                message: Some(format!(
                    "{} {}",
                    localized(
                        locale,
                        "项目的 Agent 已在停止中:",
                        "專案的 Agent 已在停止中:",
                        "Agent is already stopping for project:",
                    ),
                    project_id
                )),
                project_id,
            }));
        }

        if force || agent_status == AgentStatus::Idle || agent_status == AgentStatus::Pending {
            info!(
                "🔥 [gRPC] Force stopping/Idle/Pending status, directly cleaning up: project_id={}, status={:?}",
                project_id, agent_status
            );

            if !session_id.is_empty() {
                send_session_prompt_end(
                    &project_id,
                    &session_id,
                    StopReason::Cancelled,
                    None,
                )
                .await;
                close_session_connection(&session_id).await;
                remove_session_from_cache(&session_id);
            }

            cleanup_api_keys(app_state, &project_id);

            remove_agent_and_cleanup(&project_id);

            info!(
                "✅ [gRPC] StopAgent returned success immediately, background cleanup in progress: project_id={}",
                project_id
            );
            let response_message = format!(
                "{} {}",
                localized(
                    locale,
                    "项目的 Agent 正在停止（后台清理中）:",
                    "專案的 Agent 正在停止（後台清理中）:",
                    "Agent is stopping (background cleanup in progress) for project:",
                ),
                project_id
            );

            return Ok(Response::new(StopAgentResponse {
                success: true,
                result: "stopped".to_string(),
                message: Some(response_message),
                project_id,
            }));
        }

        if agent_status == AgentStatus::Active {
            info!(
                "📡 [gRPC] Agent is executing task, cancelling session first: project_id={}, session_id={}",
                project_id, session_id
            );

            let cancel_timeout_secs = app_state
                .config
                .grpc_timeouts
                .as_ref()
                .map(|t| t.cancel_session_timeout_secs)
                .unwrap_or(30);

            match send_cancel_and_wait(&cancel_tx, &session_id, cancel_timeout_secs).await {
                CancelAndWaitResult::Completed(cancel_result) => {
                    if cancel_result.is_success() {
                        info!(
                            "✅ [gRPC] Session cancelled successfully, continuing to stop Agent: project_id={}",
                            project_id
                        );

                        send_session_prompt_end(
                            &project_id,
                            &session_id,
                            StopReason::Cancelled,
                            None,
                        )
                        .await;
                        close_session_connection(&session_id).await;
                        remove_session_from_cache(&session_id);

                        cleanup_api_keys(app_state, &project_id);

                        remove_agent_and_cleanup(&project_id);

                        let response_message = format!(
                            "{} {}",
                            localized(
                                locale,
                                "项目的 Agent 已成功停止:",
                                "專案的 Agent 已成功停止:",
                                "Agent stopped successfully for project:",
                            ),
                            project_id
                        );

                        return Ok(Response::new(StopAgentResponse {
                            success: true,
                            result: "stopped".to_string(),
                            message: Some(response_message),
                            project_id,
                        }));
                    } else {
                        warn!(
                            "⚠️ [gRPC] Session cancellation failed, Agent stop failed: project_id={}",
                            project_id
                        );

                        return Ok(Response::new(StopAgentResponse {
                            success: false,
                            result: "error".to_string(),
                            message: Some(localized(
                                locale,
                                "取消会话失败",
                                "取消工作階段失敗",
                                "Failed to cancel session",
                            )),
                            project_id,
                        }));
                    }
                }
                CancelAndWaitResult::ChannelClosed(e) => {
                    error!(
                        "❌ [gRPC] Waiting for cancel response channel closed: project_id={}, error={:?}",
                        project_id, e
                    );

                    send_session_prompt_end(
                        &project_id,
                        &session_id,
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
                    close_session_connection(&session_id).await;

                    return Ok(Response::new(StopAgentResponse {
                        success: false,
                        result: "error".to_string(),
                        message: Some(format!(
                            "{}: {}",
                            localized(
                                locale,
                                "响应通道关闭",
                                "回應通道關閉",
                                "Response channel closed",
                            ),
                            e
                        )),
                        project_id,
                    }));
                }
                CancelAndWaitResult::Timeout => {
                    warn!("⏰ [gRPC] Timed out waiting for cancel response: project_id={}", project_id);

                    return Ok(Response::new(StopAgentResponse {
                        success: false,
                        result: "error".to_string(),
                        message: Some(localized(
                            locale,
                            "取消请求超时（30秒）",
                            "取消請求逾時（30 秒）",
                            "Cancel request timed out (30 seconds)",
                        )),
                        project_id,
                    }));
                }
                CancelAndWaitResult::SendFailed(e) => {
                    error!(
                        "❌ [gRPC] Failed to send cancel notification: project_id={}, error={}",
                        project_id, e
                    );
                    return Ok(Response::new(StopAgentResponse {
                        success: false,
                        result: "error".to_string(),
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
                        project_id,
                    }));
                }
            }
        }

        warn!(
            "⚠️ [gRPC] StopAgent reached unexpected branch: project_id={}",
            project_id
        );
        Ok(Response::new(StopAgentResponse {
            success: false,
            result: "error".to_string(),
            message: Some(localized(
                locale,
                "意外的代码分支",
                "意外的程式分支",
                "Unexpected code path",
            )),
            project_id,
        }))
    })
    .await
}
