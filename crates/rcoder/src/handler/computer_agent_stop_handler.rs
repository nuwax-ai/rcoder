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
    description = "停止特定 project_id 的 Agent，不销毁容器。支持 userApp 分派：service_type=userapp + project_id（兼任 app_id，对齐 /computer/chat 契约）定位 UserappBuilder 开发容器，agent 会话仅 dev 阶段。"
)]
#[instrument(skip(state))]
pub async fn computer_agent_stop(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerAgentStopRequest>,
) -> Result<HttpResult<ComputerAgentStopResponse>, AppError> {
    // 获取语言设置
    let locale = get_locale_from_headers(&headers);

    // 0. userApp 分派（service_type=userapp + project_id 兼任 app_id；agent 会话
    //    仅存在于 dev 的 UserappBuilder 开发容器）
    match super::pod_handler::parse_agent_userapp_dispatch(
        request.service_type.as_deref(),
        request.project_id.as_deref(),
        request.app_stage.as_deref(),
    ) {
        Ok(Some(app_id)) => {
            info!(
                "🛑 [COMPUTER_STOP] userApp dev dispatch: app_id={}, session_id={:?}",
                app_id, request.session_id
            );
            return stop_userapp_dev(&state, locale, &app_id, &request).await;
        }
        Ok(None) => {}
        Err(e) => return Ok(super::pod_handler::invalid_app_target_response(locale, &e)),
    }

    // 使用 garde 进行字段校验
    let I18nJsonOrQuery(request) = I18nJsonOrQuery(request).validate_into_app_error()?;
    let project_id = request
        .project_id
        .as_ref()
        .ok_or_else(|| AppError::validation_error("project_id is required and non-empty"))?;

    // 1. 验证参数：user_id 或 pod_id 至少有一个
    let has_user_id = request
        .user_id
        .as_ref()
        .is_some_and(|s| !s.trim().is_empty());
    let has_pod_id = request
        .pod_id
        .as_ref()
        .is_some_and(|s| !s.trim().is_empty());
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

    // 2. 查找容器：project 映射优先（computer 与 userApp 开发对话都注册映射，
    //    UserappBuilder 开发容器仅存在于映射——user_id/pod_id 的 computer 查找
    //    覆盖不了它）；映射 miss 再按 user_id/pod_id 走 computer 容器查找
    let container_info = if let Some(mapped) = state
        .get_project(project_id)
        .and_then(|p| p.container_info())
    {
        info!(
            "📦 [COMPUTER_STOP] Container resolved from project mapping: project_id={}, container_id={}",
            project_id, mapped.container_id
        );
        Some(mapped)
    } else if has_user_id {
        let uid = user_id
            .as_ref()
            .ok_or_else(|| AppError::validation_error("user_id is required"))?;
        crate::service::ComputerContainerManager::get_container_info(uid, state.runtime()).await?
    } else {
        // pod_id 作为容器标识符查找
        let pid = pod_id
            .as_ref()
            .ok_or_else(|| AppError::validation_error("pod_id is required"))?;
        crate::service::ComputerContainerManager::get_container_info(pid, state.runtime()).await?
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
        None,  // 使用默认超时 (GRPC_STOP_AGENT_TIMEOUT_SECS)
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
                state.clear_session_durable(project_id).await;

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
                        state.clear_session_durable(project_id).await;

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
                        error!("[COMPUTER_STOP] Agent stop failed: {}", err_msg);
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

/// userApp dev 分派：停止 UserappBuilder 开发容器内 app 会话的 agent。
///
/// 定位 = project 映射优先（builder 容器注册于 `state.projects[app_id]`）；
/// 映射 miss 时按 UserappBuilder 只读实时查（操作型接口不 ensure 不自愈）。
/// 成功/already_stopped 与 computer 路径同构清 `clear_session_durable`。
async fn stop_userapp_dev(
    state: &AppState,
    locale: &'static str,
    app_id: &str,
    request: &ComputerAgentStopRequest,
) -> Result<HttpResult<ComputerAgentStopResponse>, AppError> {
    let container_info = state.get_project(app_id).and_then(|p| p.container_info());
    let container_info = match container_info {
        Some(info) => info,
        None => {
            match state
                .runtime()
                .get_container_info_by_identifier(
                    app_id,
                    &shared_types::ServiceType::UserappBuilder,
                )
                .await
            {
                Ok(Some(info)) => info,
                Ok(None) => {
                    warn!(
                        "[COMPUTER_STOP][USERAPP] dev builder container not found: app_id={app_id}"
                    );
                    return Ok(HttpResult::error_with_locale(
                        shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
                        locale,
                    ));
                }
                Err(e) => {
                    error!(
                        "[COMPUTER_STOP][USERAPP] failed to query dev builder container: app_id={app_id}, error={e}"
                    );
                    return Err(AppError::internal_server_error(&format!(
                        "Failed to query container info: {e}"
                    )));
                }
            }
        }
    };

    info!(
        "📦 [COMPUTER_STOP][USERAPP] dev builder container found: container_id={}, ip={}",
        container_info.container_id, container_info.container_ip
    );

    let grpc_addr = extract_grpc_addr(&container_info.service_url)?;
    match crate::grpc::grpc_stop_agent_with_pool(
        &state.grpc_pool,
        &grpc_addr,
        app_id.to_string(),
        request
            .session_id
            .clone()
            .or_else(|| Some("User requested stop".to_string())),
        false, // force=false，优雅停止
        None,
    )
    .await
    {
        Ok(response) => {
            info!(
                "📥 [COMPUTER_STOP][USERAPP] Received StopAgent response: result={}, success={}",
                response.result, response.success
            );
            if response.success {
                state.clear_session_durable(app_id).await;
                let message = format!(
                    "Agent {app_id} stopped successfully, container {} continues running",
                    container_info.container_id
                );
                return Ok(HttpResult::success(ComputerAgentStopResponse {
                    success: true,
                    message,
                    user_id: request.user_id.clone(),
                    pod_id: None,
                    project_id: app_id.to_string(),
                }));
            }
            match response.result.as_str() {
                "not_found" => {
                    warn!("[COMPUTER_STOP][USERAPP] agent not found: app_id={app_id}");
                    Ok(HttpResult::error_with_locale(
                        shared_types::error_codes::ERR_AGENT_NOT_FOUND,
                        locale,
                    ))
                }
                "already_stopped" => {
                    info!("[COMPUTER_STOP][USERAPP] agent already stopped: app_id={app_id}");
                    state.clear_session_durable(app_id).await;
                    let message =
                        shared_types::get_i18n_message("success.agent_already_stopped", locale);
                    Ok(HttpResult::success(ComputerAgentStopResponse {
                        success: true,
                        message,
                        user_id: request.user_id.clone(),
                        pod_id: None,
                        project_id: app_id.to_string(),
                    }))
                }
                "error" => {
                    let err_msg = response
                        .message
                        .unwrap_or_else(|| "Unknown error".to_string());
                    error!("[COMPUTER_STOP][USERAPP] agent stop failed: {err_msg}");
                    Ok(HttpResult::error_with_locale(
                        shared_types::error_codes::ERR_STOP_FAILED,
                        locale,
                    ))
                }
                _ => {
                    warn!(
                        "[COMPUTER_STOP][USERAPP] unexpected result: {}",
                        response.result
                    );
                    Ok(HttpResult::error_with_locale(
                        shared_types::error_codes::ERR_UNKNOWN,
                        locale,
                    ))
                }
            }
        }
        Err(e) => {
            error!(
                "❌ [COMPUTER_STOP][USERAPP] StopAgent RPC call failed: app_id={app_id}, error={e}"
            );
            Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_GRPC_ERROR,
                locale,
            ))
        }
    }
}
