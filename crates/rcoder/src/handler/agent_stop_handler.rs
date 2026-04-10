//! Agent任务停止处理器
//!
//! 转发停止请求到容器内的 agent_runner 服务

use axum::extract::State;
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, info, instrument};
use utoipa::{IntoParams, ToSchema};

use super::utils::{I18nQuery, get_locale_from_headers};
use crate::{AppError, HttpResult, router::AppState};

/// 停止Agent请求参数
#[derive(Debug, Deserialize, ToSchema, IntoParams)]
pub struct StopAgentQuery {
    /// 项目ID
    #[param(example = "test_project")]
    pub project_id: String,
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

    // 使用全局 DockerManager
    let docker_manager = match docker_manager::global::get_global_docker_manager().await {
        Ok(manager) => manager,
        Err(e) => {
            error!("[STOP_DESTROY] Failed to get global DockerManager: {}", e);
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_CONTAINER_ERROR,
                locale,
            ));
        }
    };

    // 尝试通过多种方式查找容器
    // 1. 先通过 project_id 查找
    let container_info = docker_manager.get_container_info(project_id).await;

    // 2. 如果没找到，尝试通过容器名称实时查找并直接停止
    if container_info.is_none() {
        let expected_container_name = format!("rcoder-agent-{}", project_id);
        info!(
            "🔍 [STOP_DESTROY] 通过 project_id 未找到缓存，尝试实时查找容器: {}",
            expected_container_name
        );

        // 使用 find_container_realtime 获取最新的容器信息
        if let Ok(Some(result)) = docker_manager
            .find_container_realtime(&expected_container_name)
            .await
        {
            info!(
                "🎯 [STOP_DESTROY] 实时查找到容器: container_id={}, name={}, status={:?}, running={}",
                result.container_id, result.container_name, result.status, result.is_running
            );

            // 直接使用 container_id 停止容器（无需再查缓存）
            let stop_result = docker_manager
                .stop_container_by_id(&result.container_id)
                .await;

            if let Err(e) = stop_result {
                error!("[STOP_DESTROY] stoppedcontainerfailed: {}", e);
                return Ok(HttpResult::error_with_locale(
                    shared_types::error_codes::ERR_STOP_FAILED,
                    locale,
                ));
            }

            // 从 DuckDB 存储中移除项目
            state.remove_project(project_id);

            info!(
                "✅ [STOP_DESTROY] 容器销毁成功（实时查找）: project_id={}, container_id={}",
                project_id, result.container_id
            );

            let response = StopAgentResponse {
                success: true,
                project_id: project_id.to_string(),
                session_id: None,
                message: shared_types::get_i18n_message("success.container_destroyed", locale),
            };

            return Ok(HttpResult::success(response));
        }
    }

    if let Some(container_info) = container_info {
        info!(
            "🎯 [STOP_DESTROY] 找到容器，开始销毁: project_id={}, container_id={}, container_name={}",
            project_id, container_info.container_id, container_info.container_name
        );

        // 停止容器
        let stop_result = docker_manager
            .stop_container_by_id(&container_info.container_id)
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
            "✅ [STOP_DESTROY] 容器销毁成功: project_id={}, container_id={}, container_name={}",
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
            "📭 [STOP_DESTROY] 容器不存在，无需销毁: project_id={}",
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
    params(
        StopAgentQuery
    ),
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
    I18nQuery(query): I18nQuery<StopAgentQuery>,
) -> Result<HttpResult<StopAgentResponse>, AppError> {
    let locale = get_locale_from_headers(&headers);
    let project_id = query.project_id.trim();

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
