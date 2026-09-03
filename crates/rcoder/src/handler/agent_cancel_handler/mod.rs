//! Agent 会话取消 handler（目录化拆分；函数体原样搬迁）。
//! - forward: 取消转发机制层（标识符定位/gRPC 转发/幂等分类/存在性检查）
//!
//! 死代码清理：ComputerCancelQuery（注释自称"仅用于测试"，生产零引用）
//! 随本次拆分删除。

//! Agent任务取消处理器
//!
//! 转发取消请求到容器内的 agent_runner 服务

#![allow(dead_code)]

use axum::extract::State;
use axum::http::HeaderMap;
use std::sync::Arc;
use tracing::{error, info, instrument};

use crate::router::AppState;
use docker_manager::ContainerBasicInfo;
// 存储契约 trait：state.projects（ProjectStoreBackend 枚举）上的方法经此解析
use shared_types::ProjectStore as _;
use shared_types::{
    AgentCancelRequest, AgentCancelResponse, AppError, ComputerAgentCancelRequest, HttpResult,
};

use super::utils::{
    I18nJsonOrQuery, container_identity_from_name, extract_grpc_addr, get_locale_from_headers,
};

/// 处理agent任务取消请求
///
/// 转发取消请求到容器内的 agent_runner 服务（使用 gRPC）
#[utoipa::path(
    post,
    path = "/agent/session/cancel",
    request_body = AgentCancelRequest,
    responses(
        (
            status = 200,
            description = "成功转发取消请求到容器",
            body = HttpResult<AgentCancelResponse>,
            example = json!({
                "success": true,
                "data": {
                    "success": true,
                    "session_id": "session456"
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
                    "code": "INVALID_PARAMS",
                    "message": "Invalid project_id or session_id"
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
            description = "未找到对应的项目或会话",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "PROJECT_NOT_FOUND",
                    "message": "Project or session not found"
                }
            })
        ),
        (
            status = 500,
            description = "转发取消请求失败",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "CANCEL_FAILED",
                    "message": "Failed to forward cancel request to container"
                }
            })
        )
    ),
    tag = "agent",
    operation_id = "agent_session_cancel",
    summary = "转发Agent任务取消请求（gRPC）",
    description = "将取消请求通过 gRPC 转发到容器内的 agent_runner 服务"
)]
#[instrument(skip(state))]
pub async fn agent_session_cancel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<AgentCancelRequest>,
) -> Result<HttpResult<AgentCancelResponse>, AppError> {
    let locale = get_locale_from_headers(&headers);

    // 使用 garde 进行字段校验
    let I18nJsonOrQuery(request) = I18nJsonOrQuery(request).validate_into_app_error()?;
    let project_id = request
        .project_id
        .as_ref()
        .ok_or_else(|| AppError::validation_error("project_id is required"))?;

    info!(
        "🚫 [CANCEL] Agent cancel request: project_id={}, session_id={:?}",
        project_id, request.session_id
    );

    handle_session_cancel_internal_v2(
        &state,
        CancelIdentifier::Project(project_id.to_string()),
        project_id.to_string(),
        request.session_id,
        locale,
    )
    .await
}

/// 处理 Computer Agent 任务取消请求
///
/// 转发取消请求到容器内的 agent_runner 服务（使用 gRPC）
#[utoipa::path(
    post,
    path = "/computer/agent/session/cancel",
    request_body = ComputerAgentCancelRequest,
    responses(
        (
            status = 200,
            description = "成功转发取消请求到容器",
            body = HttpResult<AgentCancelResponse>,
            example = json!({
                "success": true,
                "data": {
                    "success": true,
                    "session_id": "session456"
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
                    "message": "user_id 或 pod_id is required"
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
            description = "未找到对应的用户容器或会话",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "CONTAINER_NOT_FOUND",
                    "message": "User container not found"
                }
            })
        ),
        (
            status = 500,
            description = "转发取消请求失败",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "CANCEL_FAILED",
                    "message": "Failed to forward cancel request to container"
                }
            })
        )
    ),
    tag = "computer",
    operation_id = "computer_agent_session_cancel",
    summary = "取消 Computer Agent 任务",
    description = "将 Computer Agent 取消请求通过 gRPC 转发到容器内的 agent_runner 服务，支持通过 user_id 或 pod_id 定位用户容器。支持 userApp 分派：service_type=userapp + app_id 定位 UserappBuilder 开发容器，agent 会话仅 dev 阶段。"
)]
#[instrument(skip(state), fields(user_id = ?request.user_id.as_deref(), project_id = ?request.project_id.as_deref(), pod_id = ?request.pod_id.as_deref()))]
pub async fn computer_agent_session_cancel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerAgentCancelRequest>,
) -> Result<HttpResult<AgentCancelResponse>, AppError> {
    let locale = get_locale_from_headers(&headers);

    // 0. userApp 分派（service_type=userapp + app_id；agent 会话仅存在于
    //    dev 的 UserappBuilder）。CancelIdentifier::Project 的定位 = project
    //    映射优先——正是 builder 容器的注册位置（app_id 即映射键），容器
    //    miss 走幂等成功
    match super::pod_handler::parse_app_target(
        request.app_id.as_deref(),
        request.app_stage.as_deref(),
        request.service_type.as_deref(),
    ) {
        Ok(super::pod_handler::AppTarget::NotApp) => {}
        Ok(super::pod_handler::AppTarget::Dev(app_id)) => {
            info!(
                "🛑 [COMPUTER_CANCEL] userApp dev dispatch: app_id={app_id}, session_id={:?}",
                request.session_id
            );
            return handle_session_cancel_internal_v2(
                &state,
                CancelIdentifier::Project(app_id.clone()),
                app_id,
                request.session_id,
                locale,
            )
            .await;
        }
        Ok(super::pod_handler::AppTarget::Prod(_)) => {
            return Ok(super::pod_handler::invalid_app_target_response(
                locale,
                "app_stage 'prod' is not supported: agent 会话仅存在于 dev 阶段 (UserappBuilder 开发容器)",
            ));
        }
        Err(e) => return Ok(super::pod_handler::invalid_app_target_response(locale, &e)),
    }

    let identifier = match (&request.user_id, &request.pod_id) {
        (Some(user_id), _) if !user_id.trim().is_empty() => CancelIdentifier::User(user_id.clone()),
        (_, Some(pod_id)) if !pod_id.trim().is_empty() => CancelIdentifier::Pod(pod_id.clone()),
        _ => {
            error!("[COMPUTER_CANCEL] user_id or pod_id is required");
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
            ));
        }
    };

    // 验证 project_id 不为空（computer 路径必填）
    let Some(project_id) = request.project_id.filter(|s| !s.trim().is_empty()) else {
        error!("[COMPUTER_CANCEL] project_id is required");
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        ));
    };

    info!(
        "🚀 [COMPUTER_CANCEL] Starting to process cancel request: user_id={:?}, pod_id={:?}, project_id={}, session_id={:?}",
        request.user_id, request.pod_id, project_id, request.session_id
    );

    handle_session_cancel_internal_v2(
        &state,
        identifier,
        project_id, // computer 路径必填的 project_id
        request.session_id,
        locale,
    )
    .await
}

mod forward;

use forward::{CancelIdentifier, handle_session_cancel_internal_v2};
