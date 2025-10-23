//! Agent执行任务的SSE通知处理器
//!
//! 使用 Pingora 透明代理处理 SSE 消息，实现高效的 SSE 重定向

use crate::{AppError, HttpResult};
use axum::{extract::Path, response::Json};
use serde::{Deserialize, Serialize};
use shared_types::ProjectAndAgentInfo;
use tracing::{error, info};
use utoipa::{IntoParams, ToSchema};

/// 会话通知路径参数
#[derive(Debug, Deserialize, IntoParams)]
pub struct SessionNotificationParams {
    /// 会话ID，用于标识特定的会话连接
    #[param(example = "session456")]
    pub session_id: String,
}

/// 检查 session_id 对应的容器，返回容器的直接 SSE URL
///
/// 此接口返回容器内 agent_runner 的直接连接地址，让前端绕过 axum SSE 处理
/// 直接通过 Pingora 透明代理连接到容器，实现更高效的 SSE 通信
///
/// ## 🔄 代理流程
///
/// 1. 用户请求 `/agent/progress/{session_id}`
/// 2. axum 处理器检查 session_id 对应的容器是否存在
/// 3. 如果容器存在，返回容器的实际 SSE 端点 URL
/// 4. 前端直接连接到容器 URL (通过 Pingora 透明代理)
/// 5. 如果容器不存在，返回错误
///
/// ## 💡 优势
///
/// - **高性能**: 避免 axum SSE 转发的开销
/// - **低延迟**: 直接连接到目标容器
/// - **简化架构**: 减少中间层处理
/// - **实时性**: 保持原始 SSE 协议特性
#[utoipa::path(
    get,
    path = "/agent/progress/{session_id}",
    params(
        SessionNotificationParams
    ),
    responses(
        (
            status = 200,
            description = "成功获取容器 SSE 端点地址",
            body = ProxyRedirectResponse,
            example = json!({
                "success": true,
                "data": {
                    "container_url": "http://localhost:8081/agent/agent/progress?session_id=abc123",
                    "project_id": "project_xyz",
                    "session_id": "session_abc123",
                    "status": "container_ready"
                }
            })
        ),
        (
            status = 404,
            description = "未找到对应的容器",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "CONTAINER_NOT_FOUND",
                    "message": "未找到 session_id 对应的活跃容器"
                }
            })
        ),
        (
            status = 500,
            description = "获取容器信息失败",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "CONTAINER_ERROR",
                    "message": "获取容器信息时发生内部错误"
                }
            })
        )
    ),
    tag = "agent",
    operation_id = "agent_session_notification",
    summary = "获取容器 SSE 端点地址",
    description = "检查 session_id 对应的容器，返回容器的直接 agent_runner SSE 连接地址，让前端通过 Pingora 透明代理直接连接。"
)]
pub async fn agent_session_notification(
    Path(params): Path<SessionNotificationParams>,
) -> Result<Json<HttpResult<ProxyRedirectResponse>>, AppError> {
    info!(
        "🔍 [PROXY_REDIRECT] 收到SSE端点请求: session_id={:?}",
        params.session_id
    );

    // 检查容器存在性并获取代理目标
    let session_id = params.session_id.clone();
    match check_container_and_proxy_sse(&session_id).await {
        Ok((target_url, project_id)) => {
            info!(
                "✅ [PROXY_REDIRECT] 找到容器: session_id={}, project_id={}, target_url={}",
                session_id, project_id, target_url
            );

            let response = ProxyRedirectResponse {
                container_url: format!("{}?session_id={}", target_url, session_id),
                project_id,
                session_id: session_id.clone(),
                status: "container_ready".to_string(),
            };

            Ok(Json(HttpResult::success(response)))
        }
        Err(e) => {
            error!(
                "❌ [PROXY_REDIRECT] 容器检查失败: session_id={}, error={}",
                session_id, e
            );

            Ok(Json(HttpResult::error(
                "CONTAINER_NOT_FOUND",
                "未找到 session_id 对应的活跃容器",
            )))
        }
    }
}

/// 代理重定向响应结构
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ProxyRedirectResponse {
    /// 容器的 SSE 端点 URL
    #[schema(example = "http://localhost:8081/agent/agent/progress?session_id=abc123")]
    pub container_url: String,

    /// 项目 ID
    #[schema(example = "project_xyz")]
    pub project_id: String,

    /// 会话 ID
    #[schema(example = "session_abc123")]
    pub session_id: String,

    /// 容器状态
    #[schema(example = "container_ready")]
    pub status: String,
}

/// 检查 session_id 对应的容器，返回容器的直接 SSE URL
async fn check_container_and_proxy_sse(session_id: &str) -> Result<(String, String), AppError> {
    info!(
        "🔍 [PROXY_REDIRECT] 检查容器可用性: session_id={}",
        session_id
    );

    // 查找对应的容器
    if let Some((project_id, agent_info)) = find_container_by_session_id(session_id) {
        info!(
            "✅ [PROXY_REDIRECT] 找到对应容器: project_id={}, session_id={}",
            project_id, session_id
        );

        // 获取容器的服务地址
        let container_service_url = get_container_service_url(&project_id, &agent_info).await?;

        // 构建目标容器的 SSE 端点 URL
        let target_url = format!("{}/agent/agent/progress", container_service_url);

        info!("🚀 [PROXY_REDIRECT] 容器可用，目标 URL: {}", target_url);

        Ok((target_url, project_id))
    } else {
        error!(
            "❌ [PROXY_REDIRECT] 未找到对应容器，无法建立 SSE 连接: session_id={}",
            session_id
        );
        Err(AppError::internal_server_error(&format!(
            "未找到 session_id={} 对应的活跃容器",
            session_id
        )))
    }
}

/// 获取容器服务地址
async fn get_container_service_url(
    project_id: &str,
    _agent_info: &ProjectAndAgentInfo,
) -> Result<String, AppError> {
    info!("🔍 [CONTAINER] 获取容器服务地址: project_id={}", project_id);

    let docker_manager = std::sync::Arc::new(
        docker_manager::DockerManager::with_default_config()
            .await
            .map_err(|e| {
                error!("❌ [CONTAINER] 创建 DockerManager 失败: {}", e);
                AppError::internal_server_error(&format!("创建 DockerManager 失败: {}", e))
            })?,
    );

    // 获取容器信息
    let container_info = docker_manager.get_container_info(project_id);
    if let Some(container_info) = container_info {
        let server_url = crate::proxy_agent::docker_container_agent::get_container_ip(
            &docker_manager,
            &container_info.container_id,
        )
        .await
        .map_err(|e| {
            error!("❌ [CONTAINER] 获取容器 IP 失败: {}", e);
            AppError::internal_server_error(&format!("获取容器 IP 失败: {}", e))
        })?;

        info!("✅ [CONTAINER] 获取容器服务 URL: {}", server_url);
        Ok(server_url)
    } else {
        Err(AppError::internal_server_error("未找到容器信息"))
    }
}

/// 根据session_id查找对应的容器
fn find_container_by_session_id(
    session_id: &str,
) -> Option<(String, std::sync::Arc<ProjectAndAgentInfo>)> {
    use crate::proxy_agent::PROJECT_AND_AGENT_INFO_MAP;

    for entry in PROJECT_AND_AGENT_INFO_MAP.iter() {
        let agent_info = entry.value();
        if agent_info.session_id.to_string() == session_id {
            return Some((entry.key().clone(), std::sync::Arc::new(agent_info.clone())));
        }
    }
    None
}
