use axum::extract::Path;
use shared_types::{AgentStatusResponse, AppError, HttpResult};
use tracing::info;

use crate::service::AGENT_REGISTRY;


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

    // 从统一 Registry 获取 Agent 信息
    if let Some(agent_info) = AGENT_REGISTRY.get_agent_info(project_id) {
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
