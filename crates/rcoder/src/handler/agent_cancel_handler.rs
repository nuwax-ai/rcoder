//! Agent任务取消处理器
//!
//! 转发取消请求到容器内的 agent_runner 服务

use axum::{extract::{Query, State}, Json};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, error, info, instrument};
use utoipa::{IntoParams, ToSchema};

use crate::{
    CancelNotificationRequest, proxy_agent::{PROJECT_AND_AGENT_INFO_MAP, docker_container_agent},
};
use crate::{model::AppError, model::HttpResult, router::AppState};

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
#[derive(Debug, Serialize, ToSchema)]
pub struct CancelResponse {
    /// 取消操作是否成功
    #[schema(example = true)]
    pub success: bool,
    /// 被取消的会话ID
    #[schema(example = "session456")]
    pub session_id: String,
}

/// 转发取消请求到容器内的 agent_runner 服务
async fn forward_cancel_request_to_container(
    project_id: &str,
    session_id: &str,
) -> Result<HttpResult<CancelResponse>, AppError> {
    info!(
        "🚀 [CANCEL_FORWARD] 开始转发取消请求: project_id={}, session_id={}",
        project_id, session_id
    );

    // 检查或创建容器
    let (server_url, _project_id_final) = ensure_container_exists_for_cancel(project_id).await?;

    // 构建容器内 agent_runner 的取消请求
    let cancel_request = serde_json::json!({
        "session_id": session_id,
        "project_id": project_id
    });

    // 转发到容器内的 agent/agent/cancel 接口
    let client = Client::new();
    let cancel_url = format!("{}/agent/agent/cancel", server_url);

    info!(
        "📤 [CANCEL_FORWARD] 转发取消请求到容器: {}",
        cancel_url
    );

    let response = client
        .post(&cancel_url)
        .json(&cancel_request)
        .send()
        .await
        .map_err(|e| {
            error!("❌ [CANCEL_FORWARD] 转发取消请求失败: {}", e);
            AppError::internal_server_error(&format!("转发取消请求到容器失败: {}", e))
        })?;

    if response.status().is_success() {
        // 直接返回容器内的响应
        let container_response: CancelResponse = response.json().await
            .map_err(|e| {
                error!("❌ [CANCEL_FORWARD] 解析容器响应失败: {}", e);
                AppError::internal_server_error(&format!("解析容器响应失败: {}", e))
            })?;

        info!(
            "✅ [CANCEL_FORWARD] 容器取消响应成功: session_id={}, success={}",
            container_response.session_id, container_response.success
        );

        Ok(HttpResult::success(container_response))
    } else {
        error!(
            "❌ [CANCEL_FORWARD] 容器取消请求失败: status={}, body={:?}",
            response.status(),
            response.text().await
        );

        Ok(HttpResult::error(
            "CANCEL001",
            &format!("容器取消请求失败: {}", response.status()),
        ))
    }
}

/// 检查或创建容器（用于取消请求）
async fn ensure_container_exists_for_cancel(project_id: &str) -> Result<(String, String), AppError> {
    // 检查容器是否已存在
    if !PROJECT_AND_AGENT_INFO_MAP.contains_key(project_id) {
        info!("🏗️ [CANCEL_FORWARD] 容器不存在，创建新容器: project_id={}", project_id);

        // 使用默认配置创建容器
        let chat_prompt = crate::model::ChatPromptBuilder::default()
            .project_id(project_id.to_string())
            .session_id(format!("cancel_session_{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)))
            .prompt("cancel_request".to_string())
            .build()
            .map_err(|e| {
                error!("❌ [CANCEL_FORWARD] 构建 ChatPrompt 失败: {}", e);
                AppError::internal_server_error(&format!("构建 ChatPrompt 失败: {}", e))
            })?;

        // 创建容器
        create_container_for_cancel(&chat_prompt, None).await?;
    }

    // 获取容器服务 URL
    if let Some(agent_info) = PROJECT_AND_AGENT_INFO_MAP.get(project_id) {
        let docker_manager = std::sync::Arc::new(
            docker_manager::DockerManager::with_default_config().await
                .map_err(|e| {
                    error!("❌ [CANCEL_FORWARD] 创建 DockerManager 失败: {}", e);
                    AppError::internal_server_error(&format!("创建 DockerManager 失败: {}", e))
                })?
        );

        // 获取容器 IP 地址
        let container_info = docker_manager.get_container_info(project_id);
        if let Some(container_info) = container_info {
            let server_url = docker_container_agent::get_container_ip(&docker_manager, &container_info.container_id, container_info.assigned_port).await
                .map_err(|e| {
                    error!("❌ [CANCEL_FORWARD] 获取容器 IP 失败: {}", e);
                    AppError::internal_server_error(&format!("获取容器 IP 失败: {}", e))
                })?;

            info!("✅ [CANCEL_FORWARD] 获取容器服务 URL: {}", server_url);
            Ok((server_url, project_id.to_string()))
        } else {
            Err(AppError::internal_server_error("未找到容器信息"))
        }
    } else {
        Err(AppError::internal_server_error("容器创建失败"))
    }
}

/// 为取消请求创建容器
async fn create_container_for_cancel(
    chat_prompt: &crate::model::ChatPrompt,
    _model_provider: Option<shared_types::ModelProviderConfig>,
) -> Result<(), AppError> {
    let project_id = &chat_prompt.project_id;
    info!("🏗️ [CANCEL_FORWARD] 开始为取消请求创建容器: project_id={}", project_id);

    // 使用 docker_container_agent 创建容器
    let docker_manager = std::sync::Arc::new(
        docker_manager::DockerManager::with_default_config().await
            .map_err(|e| {
                error!("❌ [CANCEL_FORWARD] 创建 DockerManager 失败: project_id={}, error={}", project_id, e);
                AppError::internal_server_error(&format!("创建 DockerManager 失败: {}", e))
            })?
    );

    let connection_info = docker_container_agent::start_docker_container_agent_service(
        chat_prompt.clone(),
        None, // 取消请求不需要特定的 model provider
        docker_manager,
    ).await.map_err(|e| {
        error!("❌ [CANCEL_FORWARD] 创建容器失败: project_id={}, error={}", project_id, e);
        AppError::internal_server_error(&format!("创建容器失败: {}", e))
    })?;

    info!("✅ [CANCEL_FORWARD] 容器创建成功: project_id={}, session_id={}",
          project_id, connection_info.session_id);

    // 创建生命周期守卫并存储到 MAP 中
    let project_and_agent_info = crate::model::ProjectAndAgentInfo {
        project_id: project_id.clone(),
        session_id: connection_info.session_id.clone(),
        prompt_tx: connection_info.prompt_tx.clone(),
        cancel_tx: connection_info.cancel_tx.clone(),
        model_provider: None,
        request_id: chat_prompt.request_id.clone(),
        status: crate::model::AgentStatus::Idle,
        last_activity: chrono::Utc::now(),
        created_at: chrono::Utc::now(),
        lifecycle_guard: connection_info.stop_handle
            .ok_or_else(|| AppError::internal_server_error("缺少生命周期守卫"))?
            .as_ref().clone(),
    };

    // 存储到全局 MAP
    PROJECT_AND_AGENT_INFO_MAP.insert(project_id.clone(), project_and_agent_info);

    // 建立 project_id -> session_id 映射
    let session_id_str = connection_info.session_id.to_string();
    let cleared_old = crate::service::session_cache::ensure_project_session(project_id, &session_id_str).await;
    if cleared_old > 0 {
        info!("🧹 Project session 映射更新，已清理旧消息: project_id={}, cleared_count={}",
              project_id, cleared_old);
    }

    info!("✅ [CANCEL_FORWARD] 容器创建完成并已注册: project_id={}", project_id);
    Ok(())
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
        "🛑 [CANCEL_FORWARD] 收到取消任务请求: session_id={}, project_id={:?}",
        query.session_id, query.project_id
    );

    // 直接转发到容器内的 agent_runner 服务
    let result = forward_cancel_request_to_container(&query.project_id, &query.session_id).await;

    info!("✅ [CANCEL_FORWARD] 取消请求转发完成");
    result
}