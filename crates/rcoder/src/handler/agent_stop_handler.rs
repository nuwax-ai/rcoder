//! Agent任务停止处理器
//!
//! 转发停止请求到容器内的 agent_runner 服务

use axum::extract::{Query, State, Path};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, error, info, instrument};
use utoipa::{IntoParams, ToSchema};

use crate::{
    AgentStatusResponse, AppError, HttpResult,
    proxy_agent::{PROJECT_AND_AGENT_INFO_MAP, docker_container_agent},
    router::AppState,
};

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

/// 转发停止请求到容器内的 agent_runner 服务
async fn forward_stop_request_to_container(
    project_id: &str,
) -> Result<HttpResult<StopAgentResponse>, AppError> {
    info!(
        "🚀 [STOP_FORWARD] 开始转发停止请求: project_id={}",
        project_id
    );

    // 检查或创建容器
    let (server_url, _project_id_final) = ensure_container_exists_for_stop(project_id).await?;

    // 构建容器内 agent_runner 的停止请求
    let stop_request = serde_json::json!({
        "project_id": project_id
    });

    // 转发到容器内的 agent/agent/stop 接口
    let client = Client::new();
    let stop_url = format!("{}/agent/agent/stop", server_url);

    info!(
        "📤 [STOP_FORWARD] 转发停止请求到容器: {}",
        stop_url
    );

    let response = client
        .post(&stop_url)
        .json(&stop_request)
        .send()
        .await
        .map_err(|e| {
            error!("❌ [STOP_FORWARD] 转发停止请求失败: {}", e);
            AppError::internal_server_error(&format!("转发停止请求到容器失败: {}", e))
        })?;

    if response.status().is_success() {
        // 直接返回容器内的响应
        let container_response: StopAgentResponse = response.json().await
            .map_err(|e| {
                error!("❌ [STOP_FORWARD] 解析容器响应失败: {}", e);
                AppError::internal_server_error(&format!("解析容器响应失败: {}", e))
            })?;

        info!(
            "✅ [STOP_FORWARD] 容器停止响应成功: project_id={}, success={}",
            container_response.project_id, container_response.success
        );

        Ok(HttpResult::success(container_response))
    } else {
        let status = response.status();
        let body = response.text().await;
        error!(
            "❌ [STOP_FORWARD] 容器停止请求失败: status={}, body={:?}",
            status,
            body
        );

        Ok(HttpResult::error(
            "STOP001",
            &format!("容器停止请求失败: {}", status),
        ))
    }
}

/// 检查或创建容器（用于停止请求）
async fn ensure_container_exists_for_stop(project_id: &str) -> Result<(String, String), AppError> {
    // 检查容器是否已存在
    if !PROJECT_AND_AGENT_INFO_MAP.contains_key(project_id) {
        info!("🏗️ [STOP_FORWARD] 容器不存在，创建新容器: project_id={}", project_id);

        // 使用默认配置创建容器
        let chat_prompt = shared_types::ChatPromptBuilder::default()
            .project_id(project_id.to_string())
            .session_id(format!("stop_session_{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)))
            .prompt("stop_request".to_string())
            .build()
            .map_err(|e| {
                error!("❌ [STOP_FORWARD] 构建 ChatPrompt 失败: {}", e);
                AppError::internal_server_error(&format!("构建 ChatPrompt 失败: {}", e))
            })?;

        // 创建容器
        create_container_for_stop(&chat_prompt, None).await?;
    }

    // 获取容器服务 URL
    if let Some(agent_info) = PROJECT_AND_AGENT_INFO_MAP.get(project_id) {
        let docker_manager = std::sync::Arc::new(
            docker_manager::DockerManager::with_default_config().await
                .map_err(|e| {
                    error!("❌ [STOP_FORWARD] 创建 DockerManager 失败: {}", e);
                    AppError::internal_server_error(&format!("创建 DockerManager 失败: {}", e))
                })?
        );

        // 获取容器 IP 地址
        let container_info = docker_manager.get_container_info(project_id);
        if let Some(container_info) = container_info {
            let server_url = docker_container_agent::get_container_ip(&docker_manager, &container_info.container_id, container_info.assigned_port).await
                .map_err(|e| {
                    error!("❌ [STOP_FORWARD] 获取容器 IP 失败: {}", e);
                    AppError::internal_server_error(&format!("获取容器 IP 失败: {}", e))
                })?;

            info!("✅ [STOP_FORWARD] 获取容器服务 URL: {}", server_url);
            Ok((server_url, project_id.to_string()))
        } else {
            Err(AppError::internal_server_error("未找到容器信息"))
        }
    } else {
        Err(AppError::internal_server_error("容器创建失败"))
    }
}

/// 为停止请求创建容器
async fn create_container_for_stop(
    chat_prompt: &shared_types::ChatPrompt,
    _model_provider: Option<shared_types::ModelProviderConfig>,
) -> Result<(), AppError> {
    let project_id = &chat_prompt.project_id;
    info!("🏗️ [STOP_FORWARD] 开始为停止请求创建容器: project_id={}", project_id);

    // 使用 docker_container_agent 创建容器
    let docker_manager = std::sync::Arc::new(
        docker_manager::DockerManager::with_default_config().await
            .map_err(|e| {
                error!("❌ [STOP_FORWARD] 创建 DockerManager 失败: project_id={}, error={}", project_id, e);
                AppError::internal_server_error(&format!("创建 DockerManager 失败: {}", e))
            })?
    );

    let connection_info = docker_container_agent::start_docker_container_agent_service(
        chat_prompt.clone(),
        None, // 停止请求不需要特定的 model provider
        docker_manager,
    ).await.map_err(|e| {
        error!("❌ [STOP_FORWARD] 创建容器失败: project_id={}, error={}", project_id, e);
        AppError::internal_server_error(&format!("创建容器失败: {}", e))
    })?;

    info!("✅ [STOP_FORWARD] 容器创建成功: project_id={}, session_id={}",
          project_id, connection_info.session_id);

    // 创建生命周期守卫并存储到 MAP 中
    let project_and_agent_info = shared_types::ProjectAndAgentInfo {
        project_id: project_id.clone(),
        session_id: connection_info.session_id.clone(),
        prompt_tx: connection_info.prompt_tx.clone(),
        cancel_tx: connection_info.cancel_tx.clone(),
        model_provider: None,
        request_id: chat_prompt.request_id.clone(),
        status: shared_types::AgentStatus::Idle,
        last_activity: chrono::Utc::now(),
        created_at: chrono::Utc::now(),
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

    info!("✅ [STOP_FORWARD] 容器创建完成并已注册: project_id={}", project_id);
    Ok(())
}

/// 停止指定项目的Agent服务
///
/// 转发停止请求到容器内的 agent_runner 服务
#[utoipa::path(
    post,
    path = "/agent/stop",
    params(
        StopAgentQuery
    ),
    responses(
        (
            status = 200,
            description = "成功转发停止请求到容器",
            body = HttpResult<StopAgentResponse>,
            example = json!({
                "success": true,
                "data": {
                    "success": true,
                    "project_id": "test_project",
                    "session_id": "session123",
                    "message": "Agent服务已成功停止"
                },
                "error": null
            })
        ),
        (
            status = 404,
            description = "未找到对应的Agent服务",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "AGENT_NOT_FOUND",
                    "message": "No agent service found for the specified project_id"
                }
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
            status = 500,
            description = "转发停止请求失败",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "STOP_FAILED",
                    "message": "Failed to forward stop request to container"
                }
            })
        )
    ),
    tag = "agent",
    operation_id = "agent_stop",
    summary = "转发Agent停止请求",
    description = "将停止请求转发到容器内的 agent_runner/agent/stop 接口"
)]
#[instrument(skip(_state))]
pub async fn agent_stop(
    State(_state): State<Arc<AppState>>,
    Query(query): Query<StopAgentQuery>,
) -> Result<HttpResult<StopAgentResponse>, AppError> {
    let project_id = query.project_id.trim();

    if project_id.is_empty() {
        return Ok(HttpResult::error(
            "INVALID_PARAMS",
            "project_id cannot be empty",
        ));
    }

    info!("🛑 [STOP_FORWARD] 收到停止Agent服务请求: project_id={}", project_id);

    // 直接转发到容器内的 agent_runner 服务
    let result = forward_stop_request_to_container(project_id).await;

    info!("✅ [STOP_FORWARD] 停止请求转发完成");
    result
}

/// 查询Agent状态
///
/// 查询指定项目的Agent服务状态信息（保持原有接口兼容性）
#[utoipa::path(
    get,
    path = "/agent/status/{project_id}",
    params(
        ("project_id" = String, Path, description = "项目ID", example = "test_project")
    ),
    responses(
        (
            status = 200,
            description = "成功获取Agent状态",
            body = HttpResult<AgentStatusResponse>,
            examples(
                ("Agent存活" = (value = json!({
                    "success": true,
                    "data": {
                        "project_id": "test_project",
                        "is_alive": true,
                        "session_id": "session123",
                        "status": "Active",
                        "last_activity": "2024-01-01T12:00:00Z",
                        "created_at": "2024-01-01T10:00:00Z",
                        "model_provider": {
                            "id": "custom",
                            "name": "custom",
                            "api_protocol": "OpenAI",
                            "default_model": "gpt-4"
                        }
                    },
                    "error": null
                }))),
                ("Agent不存活" = (value = json!({
                    "success": true,
                    "data": {
                        "project_id": "test_project",
                        "is_alive": false
                    },
                    "error": null
                })))
            )
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
                    "message": "project_id cannot be empty"
                }
            })
        )
    ),
    tag = "agent",
    operation_id = "agent_status",
    summary = "查询Agent状态",
    description = "查询指定项目的Agent服务状态信息。如果Agent在容器中存在且运行正常，返回完整的状态信息（包括会话ID、活动时间、模型配置等）；如果Agent不存在，只返回project_id和is_alive=false。"
)]
#[instrument(skip(_state))]
pub async fn agent_status(
    State(_state): State<Arc<AppState>>,
    Path(project_id): Path<String>,
) -> Result<HttpResult<AgentStatusResponse>, AppError> {
    let project_id = project_id.trim();

    if project_id.is_empty() {
        return Ok(HttpResult::error(
            "INVALID_PARAMS",
            "project_id cannot be empty",
        ));
    }

    info!("📊 [AGENT_STATUS] 收到查询Agent状态请求: project_id={}", project_id);

    // 从MAP中获取Agent信息
    if let Some(agent_info) = PROJECT_AND_AGENT_INFO_MAP.get(project_id) {
        let response = AgentStatusResponse {
            project_id: agent_info.project_id.clone(),
            is_alive: true,
            session_id: Some(agent_info.session_id.0.to_string()),
            status: Some(agent_info.status),
            last_activity: Some(agent_info.last_activity),
            created_at: Some(agent_info.created_at),
            model_provider: agent_info.model_provider.as_ref().map(|mp| mp.to_safe_info()),
        };

        info!(
            "✅ [AGENT_STATUS] 成功获取Agent状态: project_id={}, status={:?}",
            project_id, agent_info.status
        );

        Ok(HttpResult::success(response))
    } else {
        info!("📭 [AGENT_STATUS] Agent服务不存在: project_id={}", project_id);

        let response = AgentStatusResponse {
            project_id: project_id.to_string(),
            is_alive: false,
            session_id: None,
            status: None,
            last_activity: None,
            created_at: None,
            model_provider: None,
        };

        Ok(HttpResult::success(response))
    }
}