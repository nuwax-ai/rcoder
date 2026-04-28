//! Agent任务停止处理器
//!
//! 转发停止请求到容器内的 agent_runner 服务

use axum::extract::State;
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, info, instrument};
use utoipa::{IntoParams, ToSchema};

use super::utils::{I18nJson, get_locale_from_headers};
use crate::{AppError, HttpResult, router::AppState};

/// 停止Agent请求参数
#[derive(Debug, Deserialize, ToSchema, IntoParams)]
pub struct StopAgentQuery {
    /// 项目ID
    #[param(example = "test_project")]
    pub project_id: String,
    /// Pod ID，用于共享容器模式下的容器定位（可选）
    #[param(example = "pod_abc123")]
    #[serde(default)]
    pub pod_id: Option<String>,
    /// 租户ID（可选）
    #[param(example = "tenant_001")]
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// 空间ID（可选）
    #[param(example = "space_001")]
    #[serde(default)]
    pub space_id: Option<String>,
    /// 隔离类型（可选），如 "project", "tenant", "space"
    #[param(example = "project")]
    #[serde(default)]
    pub isolation_type: Option<String>,
}

/// 停止Agent响应
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StopAgentResponse {
    /// 是否成功停止
    pub success: bool,
    /// 项目ID
    #[schema(example = "test_project")]
    pub project_id: String,
    /// 会话ID（如果存在）
    pub session_id: Option<String>,
    /// 消息
    pub message: String,
}

/// 直接销毁指定项目对应的容器
async fn destroy_container_for_project(
    state: &Arc<AppState>,
    project_id: &str,
    locale: &'static str,
) -> Result<HttpResult<StopAgentResponse>, AppError> {
    info!(
        "[STOP_DESTROY] startingdestroycontainer: project_id={}",
        project_id
    );

    let runtime = match docker_manager::runtime::RuntimeManager::get().await {
        Ok(rt) => rt,
        Err(e) => {
            error!("[STOP_DESTROY] Failed to get runtime: {}", e);
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_CONTAINER_ERROR,
                locale,
            ));
        }
    };

    let container_info = runtime
        .get_container_info_by_identifier(project_id, &shared_types::ServiceType::RCoder)
        .await
        .ok()
        .flatten();

    if let Some(container_info) = container_info {
        info!(
            "🎯 [STOP_DESTROY] Container found, starting destruction: project_id={}, container_id={}, container_name={}",
            project_id, container_info.container_id, container_info.container_name
        );

        // 停止容器
        let stop_result = runtime
            .stop_container_by_identifier(project_id, &shared_types::ServiceType::RCoder)
            .await;

        if let Err(e) = stop_result {
            error!("[STOP_DESTROY] stoppedcontainerfailed: {}", e);
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_STOP_FAILED,
                locale,
            ));
        }

        // 从 DuckDB 存储中移除项目（如果 project_id 不是 "unknown"）
        if container_info.project_id != "unknown" {
            state.remove_project(&container_info.project_id);
        }

        info!(
            "✅ [STOP_DESTROY] Container destroyed successfully: project_id={}, container_id={}, container_name={}",
            project_id, container_info.container_id, container_info.container_name
        );

        let response = StopAgentResponse {
            success: true,
            project_id: project_id.to_string(),
            session_id: None,
            message: shared_types::get_i18n_message("success.container_destroyed", locale),
        };

        Ok(HttpResult::success(response))
    } else {
        // 容器不存在，但返回成功
        info!(
            "📭 [STOP_DESTROY] Container does not exist, no need to destroy: project_id={}",
            project_id
        );

        let response = StopAgentResponse {
            success: true,
            project_id: project_id.to_string(),
            session_id: None,
            message: shared_types::get_i18n_message("success.container_not_exist", locale),
        };

        Ok(HttpResult::success(response))
    }
}

/// 停止指定项目的Agent服务
///
/// 直接销毁 project_id 对应的容器，不向容器内的 agent_runner 发送消息
#[utoipa::path(
    post,
    path = "/agent/stop",
    request_body = StopAgentQuery,
    responses(
        (
            status = 200,
            description = "成功销毁容器",
            body = HttpResult<StopAgentResponse>,
            example = json!({
                "success": true,
                "data": {
                    "success": true,
                    "project_id": "test_project",
                    "session_id": null,
                    "message": "容器已成功销毁"
                },
                "error": null
            })
        ),
        (
            status = 200,
            description = "容器不存在但返回成功",
            body = HttpResult<StopAgentResponse>,
            example = json!({
                "success": true,
                "data": {
                    "success": true,
                    "project_id": "test_project",
                    "session_id": null,
                    "message": "容器不存在，无需销毁"
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
                    "message": "Invalid project_id parameter"
                }
            })
        ),
        (
            status = 401,
            description = "API Key 鉴权失败",
            body = HttpResult<String>
        ),
        (
            status = 500,
            description = "销毁容器失败",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "DESTROY_FAILED",
                    "message": "Failed to destroy container"
                }
            })
        )
    ),
    tag = "agent",
    operation_id = "agent_stop",
    summary = "销毁Agent容器",
    description = "直接销毁 project_id 对应的容器，不向容器内的 agent_runner 发送消息。如果容器不存在，也返回成功。"
)]
#[instrument(skip(state))]
pub async fn agent_stop(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJson(request): I18nJson<StopAgentQuery>,
) -> Result<HttpResult<StopAgentResponse>, AppError> {
    let locale = get_locale_from_headers(&headers);
    let project_id = request.project_id.trim();

    if project_id.is_empty() {
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_INVALID_PARAMS,
            locale,
        ));
    }

    info!(
        "🛑 [STOP_DESTROY] Received container destroy request: project_id={}",
        project_id
    );

    // 直接销毁容器
    let result = destroy_container_for_project(&state, project_id, locale).await;

    match &result {
        Ok(response) => {
            if let Some(data) = response.data.as_ref() {
                if data.success {
                    info!(
                        "[STOP_DESTROY] containerdestroysucceeded: project_id={}",
                        project_id
                    );
                } else {
                    error!(
                        "[STOP_DESTROY] containerdestroyfailed: project_id={}",
                        project_id
                    );
                }
            } else {
                error!(
                    "[STOP_DESTROY] Empty response: project_id={}",
                    project_id
                );
            }
        }
        Err(e) => {
            error!(
                "❌ [STOP_DESTROY] 销毁容器过程中出错: project_id={}, error={}",
                project_id, e
            );
        }
    }

    result
}
