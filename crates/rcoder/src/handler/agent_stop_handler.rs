use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info, warn};
use utoipa::{IntoParams, ToSchema};

use crate::{
    model::{AgentStatusResponse, AppError, HttpResult},
    proxy_agent::PROJECT_AND_AGENT_INFO_MAP,
    router::AppState,
    service::clear_project_messages,
};

/// 停止Agent请求参数
#[derive(Debug, Deserialize, ToSchema, IntoParams)]
pub struct StopAgentQuery {
    /// 项目ID
    #[param(example = "test_project")]
    pub project_id: String,
}

/// 停止Agent响应
#[derive(Debug, Serialize, ToSchema)]
pub struct StopAgentResponse {
    /// 是否成功停止
    pub success: bool,
    /// 项目ID
    pub project_id: String,
    /// 会话ID（如果存在）
    pub session_id: Option<String>,
    /// 消息
    pub message: String,
}

/// 停止指定项目的Agent服务
///
/// 基于RAII原则，从PROJECT_AND_AGENT_INFO_MAP中移除对应project_id，
/// AgentLifecycleGuard会自动完成所有资源清理
#[utoipa::path(
    post,
    path = "/agent/stop",
    params(
        StopAgentQuery
    ),
    responses(
        (
            status = 200,
            description = "成功停止Agent服务",
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
        )
    ),
    tag = "agent",
    operation_id = "agent_stop",
    summary = "停止Agent服务",
    description = "停止指定项目的Agent服务，用于测试和管理。基于RAII原则自动清理所有相关资源。"
)]
pub async fn agent_stop(
    State(state): State<Arc<AppState>>,
    Query(query): Query<StopAgentQuery>,
) -> Result<HttpResult<StopAgentResponse>, AppError> {
    let project_id = query.project_id.trim();

    if project_id.is_empty() {
        return Ok(HttpResult::error(
            "INVALID_PARAMS",
            "project_id cannot be empty",
        ));
    }

    info!("🛑 收到停止Agent服务请求: project_id={}", project_id);

    // 🧹 先清空对应 project_id 的所有 SSE 消息缓存，避免历史消息积压
    let cleared_count = clear_project_messages(project_id, &state.sessions);
    if cleared_count > 0 {
        info!("📝 在停止Agent服务前清空了 {} 条项目SSE历史消息: project_id={}", cleared_count, project_id);
    }

    // 检查Agent是否存在
    let agent_exists = PROJECT_AND_AGENT_INFO_MAP.contains_key(project_id);

    if !agent_exists {
        info!("📭 Agent服务已不存在，认为停止成功: project_id={}", project_id);
        return Ok(HttpResult::success(StopAgentResponse {
            success: true,
            project_id: project_id.to_string(),
            session_id: None,
            message: "Agent服务已不存在，停止操作成功".to_string(),
        }));
    }

    // 获取session_id（用于响应）
    let session_id = PROJECT_AND_AGENT_INFO_MAP
        .get(project_id)
        .map(|info| info.session_id.0.to_string());

    // 🎯 基于RAII原则：从MAPcommented移除，AgentLifecycleGuard自动清理资源
    let removed = PROJECT_AND_AGENT_INFO_MAP.remove(project_id);
        
    // 同步清理 SESSION_REQUEST_CONTEXT 中的 request_id
    crate::proxy_agent::SESSION_REQUEST_CONTEXT.remove(project_id);
    debug!("🧼 [agent_stop] 已清理 SESSION_REQUEST_CONTEXT 中的 project_id={}", project_id);

    match removed {
        Some(_) => {
            info!(
                "✅ Agent服务已成功停止: project_id={}, session_id={:?}",
                project_id, session_id
            );
            debug!("AgentLifecycleGuard将自动清理所有相关资源");

            Ok(HttpResult::success(StopAgentResponse {
                success: true,
                project_id: project_id.to_string(),
                session_id,
                message: "Agent服务已成功停止，所有资源已清理".to_string(),
            }))
        }
        None => {
            // 理论上不应该到这里，因为前面已经检查过存在性
        warn!("⚠️ Agent服务已不存在: project_id={}", project_id);
        Ok(HttpResult::success(StopAgentResponse {
            success: true,
            project_id: project_id.to_string(),
            session_id,
            message: "Agent服务已不存在，停止操作成功".to_string(),
        }))
        }
    }
}

/// 查询Agent状态
///
/// 查询指定项目的Agent服务状态信息
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
    description = "查询指定项目的Agent服务状态信息。如果Agent存活，返回完整的状态信息（包括会话ID、活动时间、模型配置等）；如果Agent不存活，只返回project_id和is_alive=false。"
)]
pub async fn agent_status(
    Path(project_id): Path<String>,
) -> Result<HttpResult<AgentStatusResponse>, AppError> {
    let project_id = project_id.trim();

    if project_id.is_empty() {
        return Ok(HttpResult::error(
            "INVALID_PARAMS",
            "project_id cannot be empty",
        ));
    }

    info!("📊 收到查询Agent状态请求: project_id={}", project_id);

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
            "✅ 成功获取Agent状态: project_id={}, status={:?}",
            project_id, agent_info.status
        );

        Ok(HttpResult::success(response))
    } else {
        info!("📭 Agent服务不存在，返回不存活状态: project_id={}", project_id);
        
        // Agent 不存在时，返回简化的响应
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
