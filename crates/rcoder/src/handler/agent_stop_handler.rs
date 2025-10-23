//! Agent任务停止处理器
//!
//! 转发停止请求到容器内的 agent_runner 服务

use axum::extract::{Path, Query, State};
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

/// 直接销毁指定项目对应的容器
async fn destroy_container_for_project(
    project_id: &str,
) -> Result<HttpResult<StopAgentResponse>, AppError> {
    info!("🔥 [STOP_DESTROY] 开始销毁容器: project_id={}", project_id);

    // 创建 DockerManager
    let docker_manager = std::sync::Arc::new(
        docker_manager::DockerManager::with_default_config()
            .await
            .map_err(|e| {
                error!("❌ [STOP_DESTROY] 创建 DockerManager 失败: {}", e);
                AppError::internal_server_error(&format!("创建 DockerManager 失败: {}", e))
            })?,
    );

    // 检查容器是否存在
    let container_info = docker_manager.get_container_info(project_id);

    if let Some(container_info) = container_info {
        info!(
            "🎯 [STOP_DESTROY] 找到容器，开始销毁: project_id={}, container_id={}",
            project_id, container_info.container_id
        );

        // 停止容器（DockerManager 的 stop_container 方法会自动从映射中移除）
        let stop_result = docker_manager.stop_container(project_id).await;

        if let Err(e) = stop_result {
            error!("❌ [STOP_DESTROY] 停止容器失败: {}", e);
            return Ok(HttpResult::error(
                "STOP001",
                &format!("停止容器失败: {}", e),
            ));
        }

        // 注意：DockerManager 的 stop_container 方法应该已经处理了容器清理
        // 我们不需要手动访问 Docker 字段，因为它是私有的

        // 从全局 Agent 映射中移除
        PROJECT_AND_AGENT_INFO_MAP.remove(project_id);

        info!(
            "✅ [STOP_DESTROY] 容器销毁成功: project_id={}, container_id={}",
            project_id, container_info.container_id
        );

        let response = StopAgentResponse {
            success: true,
            project_id: project_id.to_string(),
            session_id: Some(container_info.session_id.clone()),
            message: "容器已成功销毁".to_string(),
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
            message: "容器不存在，无需销毁".to_string(),
        };

        Ok(HttpResult::success(response))
    }
}

/// 检查或创建容器（用于停止请求）
async fn ensure_container_exists_for_stop(project_id: &str) -> Result<(String, String), AppError> {
    // 检查容器是否已存在
    if !PROJECT_AND_AGENT_INFO_MAP.contains_key(project_id) {
        info!(
            "🏗️ [STOP_FORWARD] 容器不存在，创建新容器: project_id={}",
            project_id
        );

        // 使用默认配置创建容器
        let chat_prompt = shared_types::ChatPromptBuilder::default()
            .project_id(project_id.to_string())
            .session_id(format!(
                "stop_session_{}",
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
            ))
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
            docker_manager::DockerManager::with_default_config()
                .await
                .map_err(|e| {
                    error!("❌ [STOP_FORWARD] 创建 DockerManager 失败: {}", e);
                    AppError::internal_server_error(&format!("创建 DockerManager 失败: {}", e))
                })?,
        );

        // 获取容器 IP 地址
        let container_info = docker_manager.get_container_info(project_id);
        if let Some(container_info) = container_info {
            let server_url = docker_container_agent::get_container_ip(
                &docker_manager,
                &container_info.container_id,
            )
            .await
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
    info!(
        "🏗️ [STOP_FORWARD] 开始为停止请求创建容器: project_id={}",
        project_id
    );

    // 使用 docker_container_agent 创建容器
    let docker_manager = std::sync::Arc::new(
        docker_manager::DockerManager::with_default_config()
            .await
            .map_err(|e| {
                error!(
                    "❌ [STOP_FORWARD] 创建 DockerManager 失败: project_id={}, error={}",
                    project_id, e
                );
                AppError::internal_server_error(&format!("创建 DockerManager 失败: {}", e))
            })?,
    );

    // 创建项目工作目录
    let project_workspace =
        crate::service::container_manager::get_project_workspace(project_id).await?;
    crate::service::container_manager::create_project_workspace(project_id)
        .await
        .map_err(|e| {
            error!(
                "❌ [STOP_FORWARD] 创建项目工作目录失败: project_id={}, error={}",
                project_id, e
            );
            AppError::internal_server_error(&format!("创建项目工作目录失败: {}", e))
        })?;

    let (_container_info, _server_url) =
        docker_container_agent::start_docker_container_agent_service(
            project_id.to_string(),
            project_workspace.to_string_lossy().to_string(),
            docker_manager,
        )
        .await
        .map_err(|e| {
            error!(
                "❌ [STOP_FORWARD] 创建容器失败: project_id={}, error={}",
                project_id, e
            );
            AppError::internal_server_error(&format!("创建容器失败: {}", e))
        })?;

    info!("✅ [STOP_FORWARD] 容器创建成功: project_id={}", project_id);

    Ok(())
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

    info!(
        "🛑 [STOP_DESTROY] 收到销毁容器请求: project_id={}",
        project_id
    );

    // 直接销毁容器
    let result = destroy_container_for_project(project_id).await;

    match &result {
        Ok(response) => {
            if response.data.as_ref().unwrap().success {
                info!("✅ [STOP_DESTROY] 容器销毁成功: project_id={}", project_id);
            } else {
                error!("❌ [STOP_DESTROY] 容器销毁失败: project_id={}", project_id);
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

    info!(
        "📊 [AGENT_STATUS] 收到查询Agent状态请求: project_id={}",
        project_id
    );

    // 从MAP中获取Agent信息
    if let Some(agent_info) = PROJECT_AND_AGENT_INFO_MAP.get(project_id) {
        let response = AgentStatusResponse {
            project_id: agent_info.project_id.clone(),
            is_alive: true,
            session_id: Some(agent_info.session_id.0.to_string()),
            status: Some(agent_info.status),
            last_activity: Some(agent_info.last_activity),
            created_at: Some(agent_info.created_at),
            model_provider: agent_info
                .model_provider
                .as_ref()
                .map(|mp| mp.to_safe_info()),
        };

        info!(
            "✅ [AGENT_STATUS] 成功获取Agent状态: project_id={}, status={:?}",
            project_id, agent_info.status
        );

        Ok(HttpResult::success(response))
    } else {
        info!(
            "📭 [AGENT_STATUS] Agent服务不存在: project_id={}",
            project_id
        );

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
