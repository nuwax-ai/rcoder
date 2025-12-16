//! Agent任务取消处理器
//!
//! 转发取消请求到容器内的 agent_runner 服务

use axum::extract::{Query, State};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, error, info, instrument, warn};
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
    /// 会话ID，用于标识要取消的会话（可选，如果不提供则取消该项目的所有会话）
    #[param(example = "session456")]
    #[serde(default)]
    pub session_id: Option<String>,
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

/// 获取容器（用于取消请求，不创建）
async fn get_container_for_cancel(
    project_id: &str,
) -> Result<Option<ContainerBasicInfo>, AppError> {
    info!(
        "🔍 [CANCEL_CONTAINER] 查找容器: project_id={}",
        project_id
    );

    // 只获取容器，不创建
    let container_info =
        crate::service::container_manager::ContainerManager::get_container_info(project_id)
            .await?;

    if let Some(ref info) = container_info {
        info!(
            "✅ [CANCEL_CONTAINER] 找到容器: project_id={}, container_id={}, service_url={}",
            project_id, info.container_id, info.service_url
        );
    } else {
        info!(
            "ℹ️ [CANCEL_CONTAINER] 容器不存在: project_id={}, 无需取消",
            project_id
        );
    }

    Ok(container_info)
}

/// 转发取消请求到容器内的 agent_runner 服务
///
/// 🎯 使用 gRPC CancelSession RPC 替代 HTTP 转发
async fn forward_cancel_request_to_container_service(
    project_id: &str,
    session_id: Option<&str>,
    container_info: &ContainerBasicInfo,
    grpc_pool: &Arc<crate::grpc::GrpcChannelPool>,
) -> Result<HttpResult<CancelResponse>, AppError> {
    let session_id_display = session_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| "None".to_string());
    info!(
        "📤 [CANCEL_FORWARD] 转发取消请求到容器 (gRPC): project_id={}, session_id={}, container_id={}",
        project_id, session_id_display, container_info.container_id
    );

    // 🎯 使用 gRPC 替代 HTTP
    // 从 service_url 提取 gRPC 地址
    let grpc_addr = extract_grpc_addr(&container_info.service_url)?;

    debug!(
        "📡 [CANCEL_FORWARD] 发送 gRPC 取消请求到: {}, session_id={}",
        grpc_addr, session_id_display
    );

    // 构建 session_id（如果未提供则使用空字符串，由 Agent Runner 根据 project_id 查找）
    let session_id_str = session_id.unwrap_or("").to_string();
    let reason = "用户请求取消".to_string();

    // 调用 gRPC CancelSession
    match crate::grpc::grpc_cancel_session_with_pool(
        grpc_pool,
        &grpc_addr,
        session_id_str.clone(),
        reason,
        project_id.to_string(),
    )
    .await
    {
        Ok(grpc_response) => {
            if grpc_response.success {
                info!(
                    "✅ [CANCEL_FORWARD] gRPC 取消成功: session_id={}",
                    session_id_str
                );
                Ok(HttpResult::success(CancelResponse {
                    success: true,
                    session_id: session_id_str,
                }))
            } else {
                let error_msg = grpc_response
                    .message
                    .unwrap_or_else(|| "未知错误".to_string());
                error!("❌ [CANCEL_FORWARD] gRPC 取消失败: {}", error_msg);
                Ok(HttpResult::error("CANCEL001", &error_msg))
            }
        }
        Err(e) => {
            error!("❌ [CANCEL_FORWARD] gRPC 调用失败: {}", e);
            // 尝试回退到 HTTP（可选）
            warn!("⚠️ [CANCEL_FORWARD] gRPC 失败，尝试 HTTP 回退");
            forward_cancel_request_via_http(project_id, session_id, container_info).await
        }
    }
}

/// 从 service_url 提取 gRPC 地址
fn extract_grpc_addr(service_url: &str) -> Result<String, AppError> {
    // service_url 格式: http://192.168.1.100:8086
    let host = service_url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split(':')
        .next()
        .ok_or_else(|| AppError::internal_server_error("无效的 service_url"))?;

    Ok(format!("{}:{}", host, shared_types::GRPC_DEFAULT_PORT))
}

/// HTTP 回退方案（当 gRPC 不可用时）
async fn forward_cancel_request_via_http(
    project_id: &str,
    session_id: Option<&str>,
    container_info: &ContainerBasicInfo,
) -> Result<HttpResult<CancelResponse>, AppError> {
    let client = Client::new();
    let cancel_url = if let Some(sid) = session_id {
        format!(
            "{}/agent/session/cancel?project_id={}&session_id={}",
            container_info.service_url, project_id, sid
        )
    } else {
        format!(
            "{}/agent/session/cancel?project_id={}",
            container_info.service_url, project_id
        )
    };

    debug!("📡 [HTTP_FALLBACK] 发送 HTTP 取消请求到: {}", cancel_url);

    let response = client.post(&cancel_url).send().await.map_err(|e| {
        error!("❌ [HTTP_FALLBACK] HTTP 请求失败: {}", e);
        AppError::internal_server_error(&format!("转发取消请求到容器失败: {}", e))
    })?;

    let status = response.status();
    debug!("📥 [HTTP_FALLBACK] 容器响应状态: {}", status);

    if status.is_success() {
        // 解析容器返回的 HttpResult<CancelResponse> 格式
        let container_http_result: shared_types::HttpResult<CancelResponse> =
            response.json().await.map_err(|e| {
                error!("❌ [HTTP_FALLBACK] 解析容器响应失败: {}", e);
                AppError::internal_server_error(&format!("解析容器响应失败: {}", e))
            })?;

        // 提取 data 字段中的 CancelResponse
        let container_response = container_http_result.data.ok_or_else(|| {
            error!("❌ [HTTP_FALLBACK] 容器响应缺少 data 字段");
            AppError::internal_server_error("容器响应缺少 data 字段")
        })?;

        info!(
            "✅ [HTTP_FALLBACK] 容器取消响应成功: session_id={}, success={}",
            container_response.session_id, container_response.success
        );

        Ok(HttpResult::success(container_response))
    } else {
        let error_text = format!("容器返回错误状态: {}", status);
        let response_body = response.text().await.unwrap_or_default();
        error!(
            "❌ [HTTP_FALLBACK] {}, 响应内容: {}",
            error_text, response_body
        );

        Ok(HttpResult::error(
            "CANCEL001",
            &format!("容器取消请求失败: {}", status),
        ))
    }
}


/// 内部核心处理函数：处理会话取消请求（供多个接口复用）
///
/// 该函数封装了取消会话的核心逻辑，可被不同的 API 接口调用
async fn handle_session_cancel_internal(
    project_id: &str,
    session_id: Option<String>,
    grpc_pool: &Arc<crate::grpc::GrpcChannelPool>,
) -> Result<HttpResult<CancelResponse>, AppError> {
    let session_id_display = session_id
        .as_deref()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "None".to_string());
    info!(
        "🛑 [CANCEL_FORWARD] 收到取消任务请求: session_id={}, project_id={}",
        session_id_display, project_id
    );

    // 第一步：获取容器（不创建）
    let container_info = get_container_for_cancel(project_id).await?;

    // 如果容器不存在，说明任务已经结束或从未启动，直接返回成功
    let Some(container_info) = container_info else {
        info!(
            "✅ [CANCEL_FORWARD] 容器不存在，取消目标已达成: project_id={}",
            project_id
        );
        return Ok(HttpResult::success(CancelResponse {
            success: true,
            session_id: session_id.unwrap_or_else(|| "all".to_string()),
        }));
    };

    // 第二步：转发取消请求到容器服务（使用全局连接池）
    let result = forward_cancel_request_to_container_service(
        project_id,
        session_id.as_deref(),
        &container_info,
        grpc_pool,
    )
    .await;

    match &result {
        Ok(_) => {
            info!(
                "✅ [CANCEL_FORWARD] 取消请求处理成功: project_id={}",
                project_id
            );
        }
        Err(e) => {
            error!(
                "❌ [CANCEL_FORWARD] 取消请求处理失败: project_id={}, error={}",
                project_id, e
            );
        }
    }

    result
}

/// 处理agent任务取消请求
///
/// 转发取消请求到容器内的 agent_runner 服务（使用 gRPC）
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
    summary = "转发Agent任务取消请求（gRPC）",
    description = "将取消请求通过 gRPC 转发到容器内的 agent_runner 服务"
)]
#[instrument(skip(state))]
pub async fn agent_session_cancel(
    State(state): State<Arc<AppState>>,
    Query(query): Query<CancelQuery>,
) -> Result<HttpResult<CancelResponse>, AppError> {
    handle_session_cancel_internal(&query.project_id, query.session_id, &state.grpc_pool).await
}

/// 处理 Computer Agent 任务取消请求
///
/// 转发取消请求到容器内的 agent_runner 服务（使用 gRPC）
#[utoipa::path(
    post,
    path = "/computer/agent/session/cancel",
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
    tag = "computer",
    operation_id = "computer_agent_session_cancel",
    summary = "转发 Computer Agent 任务取消请求（gRPC）",
    description = "将 Computer Agent 取消请求通过 gRPC 转发到容器内的 agent_runner 服务"
)]
#[instrument(skip(state))]
pub async fn computer_agent_session_cancel(
    State(state): State<Arc<AppState>>,
    Query(query): Query<CancelQuery>,
) -> Result<HttpResult<CancelResponse>, AppError> {
    handle_session_cancel_internal(&query.project_id, query.session_id, &state.grpc_pool).await
}
