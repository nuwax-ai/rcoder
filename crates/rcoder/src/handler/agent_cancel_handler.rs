//! Agent任务取消处理器
//!
//! 转发取消请求到容器内的 agent_runner 服务

use axum::extract::{Query, State};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, error, info, instrument};
use utoipa::{IntoParams, ToSchema};

use crate::router::AppState;
use docker_manager::ContainerBasicInfo;
use shared_types::{AppError, HttpResult};

/// 取消任务的查询参数
#[derive(Debug, Deserialize, IntoParams)]
pub struct CancelQuery {
    /// 项目ID，用于标识特定的项目
    #[param(example = "test_project")]
    pub project_id: String,
    /// 会话ID，用于标识要取消的会话
    #[param(example = "session456")]
    pub session_id: String,
}

/// 取消任务的响应
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CancelResponse {
    /// 取消操作是否成功
    #[schema(example = true)]
    pub success: bool,
    /// 被取消的会话ID
    #[schema(example = "session456")]
    pub session_id: String,
}

/// 获取或创建容器（用于取消请求）
async fn get_or_create_container_for_cancel(
    project_id: &str,
) -> Result<ContainerBasicInfo, AppError> {
    info!(
        "🔍 [CANCEL_CONTAINER] 开始处理容器: project_id={}",
        project_id
    );

    // 使用新的容器管理服务
    let container_info =
        crate::service::container_manager::ContainerManager::get_or_create_container(project_id)
            .await?;

    info!(
        "✅ [CANCEL_CONTAINER] 容器准备就绪: project_id={}, container_id={}, service_url={}",
        project_id, container_info.container_id, container_info.service_url
    );

    Ok(container_info)
}

/// 转发取消请求到容器内的 agent_runner 服务
async fn forward_cancel_request_to_container_service(
    project_id: &str,
    session_id: &str,
    container_info: &ContainerBasicInfo,
) -> Result<HttpResult<CancelResponse>, AppError> {
    info!(
        "📤 [CANCEL_FORWARD] 转发取消请求到容器: project_id={}, session_id={}, container_id={}",
        project_id, session_id, container_info.container_id
    );

    // 转发到容器内的 agent/session/cancel 接口（使用查询参数）
    let client = Client::new();
    let cancel_url = format!(
        "{}/agent/session/cancel?project_id={}&session_id={}",
        container_info.service_url, project_id, session_id
    );

    info!("📤 [CANCEL_FORWARD] 发送取消请求到: {}", cancel_url);

    let response = client.post(&cancel_url).send().await.map_err(|e| {
        error!("❌ [CANCEL_FORWARD] 转发取消请求失败: {}", e);
        AppError::internal_server_error(&format!("转发取消请求到容器失败: {}", e))
    })?;

    let status = response.status();
    debug!("📥 [CANCEL_FORWARD] 容器响应状态: {}", status);

    if status.is_success() {
        // 解析容器返回的 HttpResult<CancelResponse> 格式
        let container_http_result: shared_types::HttpResult<CancelResponse> =
            response.json().await.map_err(|e| {
                error!("❌ [CANCEL_FORWARD] 解析容器响应失败: {}", e);
                AppError::internal_server_error(&format!("解析容器响应失败: {}", e))
            })?;

        // 提取 data 字段中的 CancelResponse
        let container_response = container_http_result.data.ok_or_else(|| {
            error!("❌ [CANCEL_FORWARD] 容器响应缺少 data 字段");
            AppError::internal_server_error("容器响应缺少 data 字段")
        })?;

        info!(
            "✅ [CANCEL_FORWARD] 容器取消响应成功: session_id={}, success={}",
            container_response.session_id, container_response.success
        );

        Ok(HttpResult::success(container_response))
    } else {
        let error_text = format!("容器返回错误状态: {}", status);
        let response_body = response.text().await.unwrap_or_default();
        error!(
            "❌ [CANCEL_FORWARD] {}, 响应内容: {}",
            error_text, response_body
        );

        Ok(HttpResult::error(
            "CANCEL001",
            &format!("容器取消请求失败: {}", status),
        ))
    }
}

/// 转发取消请求到容器内的 agent_runner 服务（组合函数）
async fn forward_cancel_request_to_container(
    project_id: &str,
    session_id: &str,
) -> Result<HttpResult<CancelResponse>, AppError> {
    info!(
        "🚀 [CANCEL_FORWARD] 开始转发取消请求: project_id={}, session_id={}",
        project_id, session_id
    );

    // 第一步：获取或创建容器
    let container_info = get_or_create_container_for_cancel(project_id).await?;

    // 第二步：转发取消请求到容器服务
    forward_cancel_request_to_container_service(project_id, session_id, &container_info).await
}

/// 处理agent任务取消请求
///
/// 转发取消请求到容器内的 agent_runner 服务
#[utoipa::path(
    post,
    path = "/agent/session/cancel",
    params(
        CancelQuery
    ),
    responses(
        (
            status = 200,
            description = "成功转发取消请求到容器",
            body = HttpResult<CancelResponse>,
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
    summary = "转发Agent任务取消请求",
    description = "将取消请求转发到容器内的 agent_runner/agent/cancel 接口"
)]
#[instrument(skip(_state))]
pub async fn agent_session_cancel(
    State(_state): State<Arc<AppState>>,
    Query(query): Query<CancelQuery>,
) -> Result<HttpResult<CancelResponse>, AppError> {
    info!(
        "🛑 [CANCEL_FORWARD] 收到取消任务请求: session_id={}, project_id={}",
        query.session_id, query.project_id
    );

    // 第一步：获取或创建容器
    let container_info = get_or_create_container_for_cancel(&query.project_id).await?;

    // 第二步：转发取消请求到容器服务
    let result = forward_cancel_request_to_container_service(
        &query.project_id,
        &query.session_id,
        &container_info,
    )
    .await;

    match &result {
        Ok(_) => {
            info!(
                "✅ [CANCEL_FORWARD] 取消请求处理成功: project_id={}",
                query.project_id
            );
        }
        Err(e) => {
            error!(
                "❌ [CANCEL_FORWARD] 取消请求处理失败: project_id={}, error={}",
                query.project_id, e
            );
        }
    }

    result
}
