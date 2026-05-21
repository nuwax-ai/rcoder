//! Computer Agent 停止处理器
//!
//! 处理停止特定 project_id 的 Agent 请求（不销毁容器）。
//! 与 RCoder 的 agent_stop 不同，这里只停止单个 project_id 的 Agent，
//! 容器会继续运行其他 project_id 的 Agent。

use axum::extract::State;
use axum::http::HeaderMap;
use std::sync::Arc;
use tracing::{error, info, instrument, warn};

use crate::{AppError, HttpResult, router::AppState};
use shared_types::{ComputerAgentStopRequest, ComputerAgentStopResponse};

use super::utils::{I18nJsonOrQuery, extract_grpc_addr, get_locale_from_headers};

/// 停止 Computer Agent
///
/// 停止特定 user_id 下的特定 project_id 的 Agent。
/// 注意：这不会销毁容器，容器会继续运行其他 project_id 的 Agent。
///
/// 只有当 user_id 下所有 project_id 都闲置时，容器才会被清理任务销毁。
#[utoipa::path(
    post,
    path = "/computer/agent/stop",
    request_body(
        content = ComputerAgentStopRequest,
        description = "停止特定 project_id 的 Agent 请求",
        content_type = "application/json"
    ),
    responses(
        (
            status = 200,
            description = "成功停止 Agent",
            body = HttpResult<ComputerAgentStopResponse>,
            example = json!({
                "success": true,
                "data": {
                    "success": true,
                    "message": "Agent 已停止",
                    "user_id": "user_123",
                    "project_id": "proj_456"
                },
                "error": null
            })
        ),
        (
            status = 400,
            description = "请求参数错误",
            body = HttpResult<String>
        ),
        (
            status = 401,
            description = "API Key 鉴权失败",
            body = HttpResult<String>
        ),
        (
            status = 404,
            description = "找不到指定的容器或 Agent",
            body = HttpResult<String>
        ),
        (
            status = 500,
            description = "服务器内部错误",
            body = HttpResult<String>
        )
    ),
    tag = "computer",
    operation_id = "computer_agent_stop",
    summary = "停止 Computer Agent",
    description = "停止特定 project_id 的 Agent，不销毁容器"
)]
#[instrument(skip(state))]
pub async fn computer_agent_stop(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerAgentStopRequest>,
) -> Result<HttpResult<ComputerAgentStopResponse>, AppError> {
    // 获取语言设置
    let locale = get_locale_from_headers(&headers);

    // 使用 garde 进行字段校验
    let I18nJsonOrQuery(request) = I18nJsonOrQuery(request).validate_into_app_error()?;
    let project_id = request
        .project_id
        .as_ref()
        .expect("validated: project_id is required and non-empty");

    // 1. 验证参数：user_id 或 pod_id 至少有一个
    let has_user_id = request
        .user_id
        .as_ref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let has_pod_id = request
        .pod_id
        .as_ref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if !has_user_id && !has_pod_id {
        error!("[COMPUTER_STOP] user_id or pod_id is required");
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        ));
    }

    let user_id = request.user_id.clone();
    let pod_id = request.pod_id.clone();

    info!(
        "🛑 [COMPUTER_STOP] Starting to stop Agent: user_id={:?}, pod_id={:?}, project_id={}, session_id={:?}",
        user_id, pod_id, project_id, request.session_id
    );

    // 2. 查找容器（根据 user_id 或 pod_id）
    let container_info = if has_user_id {
        crate::service::ComputerContainerManager::get_container_info(user_id.as_ref().unwrap())
            .await?
    } else {
        // TODO: 实现通过 pod_id 查找容器的逻辑
        warn!("[COMPUTER_STOP] pod_id lookup not fully implemented yet");
        None
    };

    let container_info = match container_info {
        Some(info) => info,
        None => {
            warn!(
                "[COMPUTER_STOP] Container not found: user_id={:?}, pod_id={:?}",
                user_id, pod_id
            );
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
                locale,
            ));
        }
    };

    info!(
        "📦 [COMPUTER_STOP] Container found: container_id={}, ip={}",
        container_info.container_id, container_info.container_ip
    );

    // 3. 通过 gRPC 调用 StopAgent RPC
    info!(
        "🔄 [COMPUTER_STOP] Preparing to call StopAgent RPC: project_id={}",
        project_id
    );

    // 提取 gRPC 地址
    let grpc_addr = extract_grpc_addr(&container_info.service_url)?;
    info!("[COMPUTER_STOP] gRPC addr: {}", grpc_addr);

    // 调用 StopAgent RPC
    match crate::grpc::grpc_stop_agent_with_pool(
        &state.grpc_pool,
        &grpc_addr,
        project_id.to_string(),
        request
            .session_id
            .clone()
            .or_else(|| Some("User requested stop".to_string())),
        false, // force=false，优雅停止
    )
    .await
    {
        Ok(response) => {
            info!(
                "📥 [COMPUTER_STOP] Received StopAgent response: result={}, success={}",
                response.result, response.success
            );

            if response.success {
                // 🆕 清除 rcoder 端的 session_id（即使成功停止，也清理会话状态）
                state.clear_session(project_id);

                let message = format!(
                    "Agent {} stopped successfully, container {} continues running",
                    project_id, container_info.container_id
                );

                let stop_response = ComputerAgentStopResponse {
                    success: true,
                    message,
                    user_id: user_id.clone(),
                    pod_id: pod_id.clone(),
                    project_id: project_id.to_string(),
                };

                info!(
                    "✅ [COMPUTER_STOP] Agent stop completed: user_id={:?}, pod_id={:?}, project_id={}",
                    user_id, pod_id, project_id
                );
                return Ok(HttpResult::success(stop_response));
            } else {
                // Agent 停止失败或已经停止
                match response.result.as_str() {
                    "not_found" => {
                        warn!("[COMPUTER_STOP] Agent not found: project_id={}", project_id);
                        return Ok(HttpResult::error_with_locale(
                            shared_types::error_codes::ERR_AGENT_NOT_FOUND,
                            locale,
                        ));
                    }
                    "already_stopped" => {
                        info!(
                            "ℹ️ [COMPUTER_STOP] Agent already in stopped state: project_id={}",
                            project_id
                        );
                        // 🆕 清除 rcoder 端的 session_id（即使 Agent 已停止，也清理会话状态）
                        state.clear_session(project_id);

                        let message =
                            shared_types::get_i18n_message("success.agent_already_stopped", locale);
                        let stop_response = ComputerAgentStopResponse {
                            success: true,
                            message,
                            user_id: user_id.clone(),
                            pod_id: pod_id.clone(),
                            project_id: project_id.to_string(),
                        };
                        return Ok(HttpResult::success(stop_response));
                    }
                    "error" => {
                        let err_msg = response
                            .message
                            .unwrap_or_else(|| "Unknown error".to_string());
                        error!("[COMPUTER_STOP] Agent stoppedfailed: {}", err_msg);
                        return Ok(HttpResult::error_with_locale(
                            shared_types::error_codes::ERR_STOP_FAILED,
                            locale,
                        ));
                    }
                    _ => {
                        warn!("[COMPUTER_STOP] not response: {}", response.result);
                        return Ok(HttpResult::error_with_locale(
                            shared_types::error_codes::ERR_UNKNOWN,
                            locale,
                        ));
                    }
                }
            }
        }
        Err(e) => {
            error!(
                "❌ [COMPUTER_STOP] StopAgent RPC call failed: {}, project_id={}",
                e, project_id
            );
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_GRPC_ERROR,
                locale,
            ));
        }
    }
}
