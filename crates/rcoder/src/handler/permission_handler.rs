//! Permission approval callback handlers.

use axum::{Json, extract::State, http::HeaderMap};
use docker_manager::ContainerBasicInfo;
use shared_types::{
    AppError, HttpResult, ResolvePermissionHttpRequest, ResolvePermissionRequestDto,
    ResolvePermissionResponseDto,
};
use std::sync::Arc;
use tracing::{error, info};

use crate::router::AppState;

use super::utils::{I18nJsonOrQuery, extract_grpc_addr, get_locale_from_headers};

fn validate_common(input: &ResolvePermissionRequestDto) -> Result<(), AppError> {
    if input.session_id.trim().is_empty() || input.tool_call_id.trim().is_empty() {
        return Err(AppError::with_message(
            shared_types::error_codes::ERR_VALIDATION,
            "session_id and tool_call_id are required",
        ));
    }
    if !input.cancelled && input.option_id.as_deref().unwrap_or("").trim().is_empty() {
        return Err(AppError::with_message(
            shared_types::error_codes::ERR_VALIDATION,
            "option_id is required unless cancelled=true",
        ));
    }
    Ok(())
}

fn rcoder_container(
    state: &AppState,
    input: &ResolvePermissionRequestDto,
) -> Result<ContainerBasicInfo, AppError> {
    let project_id = input.project_id.as_deref().unwrap_or("").trim();
    if project_id.is_empty() {
        return Err(AppError::with_message(
            shared_types::error_codes::ERR_VALIDATION,
            "project_id is required",
        ));
    }

    state
        .get_project(project_id)
        .and_then(|info| info.container().cloned())
        .ok_or_else(|| {
            AppError::with_message(
                shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
                "container not found for project_id",
            )
        })
}

fn computer_container(
    state: &AppState,
    input: &ResolvePermissionRequestDto,
) -> Result<ContainerBasicInfo, AppError> {
    if let Some(user_id) = input.user_id.as_deref().filter(|s| !s.trim().is_empty())
        && let Some(container) = state.projects.get_container_by_user_id(user_id)
    {
        return Ok(container);
    }

    if let Some(pod_id) = input.pod_id.as_deref().filter(|s| !s.trim().is_empty())
        && let Some(container) = state.projects.get_container_by_pod_id(pod_id)
    {
        return Ok(container);
    }

    Err(AppError::with_message(
        shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
        "container not found for user_id or pod_id",
    ))
}

async fn forward_permission_resolution(
    state: &AppState,
    container: ContainerBasicInfo,
    input: ResolvePermissionRequestDto,
    locale: &'static str,
) -> Result<Json<HttpResult<ResolvePermissionResponseDto>>, AppError> {
    let grpc_addr = extract_grpc_addr(&container.service_url)?;
    info!(
        "[PERMISSION] Forwarding ResolvePermission to container: grpc_addr={}, session_id={}, tool_call_id={}",
        grpc_addr, input.session_id, input.tool_call_id
    );

    let response =
        match crate::grpc::grpc_resolve_permission_with_pool(&state.grpc_pool, &grpc_addr, input)
            .await
        {
            Ok(response) => response,
            Err(err) => {
                error!("[PERMISSION] gRPC ResolvePermission failed: {}", err);
                return Ok(Json(HttpResult::error_with_locale(
                    shared_types::error_codes::ERR_GRPC_ERROR,
                    locale,
                )));
            }
        };

    let dto = ResolvePermissionResponseDto {
        success: response.success,
        session_id: response.session_id,
        tool_call_id: response.tool_call_id,
        outcome_json: response.outcome_json,
        rule_saved: response.rule_saved,
        error_code: response.error_code,
        message: response.message,
    };

    if dto.success {
        Ok(Json(HttpResult::success(dto)))
    } else {
        Ok(Json(HttpResult {
            code: dto.error_code.clone().unwrap_or_else(|| {
                shared_types::error_codes::ERR_PERMISSION_RESOLVE_FAILED.to_string()
            }),
            message: dto.message.clone().unwrap_or_else(|| {
                shared_types::error_codes::get_error_message(
                    dto.error_code
                        .as_deref()
                        .unwrap_or(shared_types::error_codes::ERR_PERMISSION_RESOLVE_FAILED),
                    locale,
                )
            }),
            data: Some(dto),
            tid: None,
            success: false,
        }))
    }
}

/// Resolve permission for an RCoder project container.
#[utoipa::path(
    post,
    path = "/agent/notify-resolved",
    request_body(
        content_type = "application/json",
        description = "权限审批结果，包含 permission_resolve_request (含 session_id、tool_call_id、request_permission_response) 以及可选的 user_id、project_id、pod_id 等容器定位参数",
    ),
    responses(
        (
            status = 200,
            description = "权限审批处理完成",
            body = HttpResult<ResolvePermissionResponseDto>,
            example = json!({
                "success": true,
                "data": {
                    "success": true,
                    "session_id": "session123",
                    "tool_call_id": "tool_call_abc",
                    "outcome_json": "{\"outcome\":\"selected\"}",
                    "rule_saved": false,
                    "error_code": null,
                    "message": null
                },
                "error": null
            })
        ),
        (
            status = 400,
            description = "请求参数错误",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "ERR_VALIDATION",
                    "message": "session_id and tool_call_id are required"
                }
            })
        ),
        (
            status = 401,
            description = "API Key 鉴权失败",
            body = HttpResult<String>
        ),
        (
            status = 404,
            description = "未找到对应的容器或权限请求",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "ERR_PERMISSION_NOT_FOUND",
                    "message": "Permission request not found or already resolved"
                }
            })
        ),
        (
            status = 500,
            description = "转发权限审批失败",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "ERR_PERMISSION_RESOLVE_FAILED",
                    "message": "Permission resolve operation failed"
                }
            })
        )
    ),
    tag = "agent",
    operation_id = "agent_notify_resolved",
    summary = "处理 Agent 权限审批结果",
    description = "将用户权限审批结果通过 gRPC 转发到项目容器内的 agent_runner 服务"
)]
pub async fn agent_notify_resolved(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(input): I18nJsonOrQuery<ResolvePermissionHttpRequest>,
) -> Result<Json<HttpResult<ResolvePermissionResponseDto>>, AppError> {
    let locale = get_locale_from_headers(&headers);
    let dto = input.to_dto();
    validate_common(&dto)?;
    let container = rcoder_container(&state, &dto)?;
    forward_permission_resolution(&state, container, dto, locale).await
}

/// Resolve permission for a ComputerAgentRunner container.
#[utoipa::path(
    post,
    path = "/computer/notify-resolved",
    request_body(
        content_type = "application/json",
        description = "权限审批结果，包含 permission_resolve_request (含 session_id、tool_call_id、request_permission_response) 以及可选的 user_id、project_id、pod_id 等容器定位参数",
    ),
    responses(
        (
            status = 200,
            description = "权限审批处理完成",
            body = HttpResult<ResolvePermissionResponseDto>,
            example = json!({
                "success": true,
                "data": {
                    "success": true,
                    "session_id": "session123",
                    "tool_call_id": "tool_call_abc",
                    "outcome_json": "{\"outcome\":\"selected\"}",
                    "rule_saved": false,
                    "error_code": null,
                    "message": null
                },
                "error": null
            })
        ),
        (
            status = 400,
            description = "请求参数错误",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "ERR_VALIDATION",
                    "message": "session_id and tool_call_id are required"
                }
            })
        ),
        (
            status = 401,
            description = "API Key 鉴权失败",
            body = HttpResult<String>
        ),
        (
            status = 404,
            description = "未找到对应的用户容器或权限请求",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "ERR_PERMISSION_NOT_FOUND",
                    "message": "Permission request not found or already resolved"
                }
            })
        ),
        (
            status = 500,
            description = "转发权限审批失败",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "ERR_PERMISSION_RESOLVE_FAILED",
                    "message": "Permission resolve operation failed"
                }
            })
        )
    ),
    tag = "computer",
    operation_id = "computer_notify_resolved",
    summary = "处理 Computer Agent 权限审批结果",
    description = "将用户权限审批结果转发到 Computer Agent 容器内的 agent_runner 服务"
)]
pub async fn computer_notify_resolved(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(input): I18nJsonOrQuery<ResolvePermissionHttpRequest>,
) -> Result<Json<HttpResult<ResolvePermissionResponseDto>>, AppError> {
    let locale = get_locale_from_headers(&headers);
    let dto = input.to_dto();
    validate_common(&dto)?;
    if input.project_id.as_deref().unwrap_or("").trim().is_empty() {
        return Err(AppError::with_message(
            shared_types::error_codes::ERR_VALIDATION,
            "project_id is required",
        ));
    }
    if input.user_id.as_deref().unwrap_or("").trim().is_empty()
        && input.pod_id.as_deref().unwrap_or("").trim().is_empty()
    {
        return Err(AppError::with_message(
            shared_types::error_codes::ERR_VALIDATION,
            "user_id or pod_id is required",
        ));
    }
    let container = computer_container(&state, &dto)?;
    forward_permission_resolution(&state, container, dto, locale).await
}
