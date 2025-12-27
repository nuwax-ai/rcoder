//! Computer Agent 停止处理器
//!
//! 处理停止特定 project_id 的 Agent 请求（不销毁容器）。
//! 与 RCoder 的 agent_stop 不同，这里只停止单个 project_id 的 Agent，
//! 容器会继续运行其他 project_id 的 Agent。

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, info, instrument, warn};
use utoipa::ToSchema;

use crate::{AppError, HttpResult, router::AppState};

use super::utils::extract_grpc_addr;

/// Computer Agent 停止请求
#[derive(Debug, Deserialize, Serialize, Clone, ToSchema)]
pub struct ComputerAgentStopRequest {
    /// 用户 ID (必填)
    #[schema(example = "user_123")]
    pub user_id: String,

    /// 项目 ID (必填) - 只停止特定项目的 Agent
    #[schema(example = "proj_456")]
    pub project_id: String,

    /// 可选的会话 ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "session789")]
    pub session_id: Option<String>,
}

/// Computer Agent 停止响应
#[derive(Debug, Serialize, ToSchema)]
pub struct ComputerAgentStopResponse {
    /// 操作是否成功
    pub success: bool,
    /// 响应消息
    pub message: String,
    /// 用户 ID
    pub user_id: String,
    /// 项目 ID
    pub project_id: String,
}

/// 停止 Computer Agent
///
/// 停止特定 user_id 下的特定 project_id 的 Agent。
/// 注意：这不会销毁容器，容器会继续运行其他 project_id 的 Agent。
///
/// 只有当 user_id 下所有 project_id 都闲置时，容器才会被清理任务销毁。
#[utoipa::path(
    post,
    path = "/computer/agent/stop",
    request_body(
        content = ComputerAgentStopRequest,
        description = "停止特定 project_id 的 Agent 请求",
        content_type = "application/json"
    ),
    responses(
        (
            status = 200,
            description = "成功停止 Agent",
            body = HttpResult<ComputerAgentStopResponse>,
            example = json!({
                "success": true,
                "data": {
                    "success": true,
                    "message": "Agent 已停止",
                    "user_id": "user_123",
                    "project_id": "proj_456"
                },
                "error": null
            })
        ),
        (
            status = 400,
            description = "请求参数错误",
            body = HttpResult<String>
        ),
        (
            status = 404,
            description = "找不到指定的容器或 Agent",
            body = HttpResult<String>
        ),
        (
            status = 500,
            description = "服务器内部错误",
            body = HttpResult<String>
        )
    ),
    tag = "computer",
    operation_id = "computer_agent_stop",
    summary = "停止 Computer Agent",
    description = "停止特定 project_id 的 Agent，不销毁容器"
)]
#[axum::debug_handler]
#[instrument(skip(state), fields(user_id = %request.user_id, project_id = %request.project_id))]
pub async fn computer_agent_stop(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ComputerAgentStopRequest>,
) -> Result<HttpResult<ComputerAgentStopResponse>, AppError> {
    // 1. 验证参数
    if request.user_id.trim().is_empty() {
        error!("❌ [COMPUTER_STOP] user_id 不能为空");
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "user_id 不能为空",
        ));
    }

    if request.project_id.trim().is_empty() {
        error!("❌ [COMPUTER_STOP] project_id 不能为空");
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "project_id 不能为空",
        ));
    }

    let user_id = request.user_id.clone();
    let project_id = request.project_id.clone();

    info!(
        "🛑 [COMPUTER_STOP] 开始停止 Agent: user_id={}, project_id={}, session_id={:?}",
        user_id, project_id, request.session_id
    );

    // 2. 查找用户容器
    let container_info =
        crate::service::ComputerContainerManager::get_container_info(&user_id).await?;

    let container_info = match container_info {
        Some(info) => info,
        None => {
            warn!("⚠️ [COMPUTER_STOP] 找不到用户容器: user_id={}", user_id);
            return Ok(HttpResult::error(
                "NOT_FOUND",
                &format!("找不到用户 {} 的容器", user_id),
            ));
        }
    };

    info!(
        "📦 [COMPUTER_STOP] 找到容器: container_id={}, ip={}",
        container_info.container_id, container_info.container_ip
    );

    // 3. 通过 gRPC 调用 StopAgent RPC
    info!(
        "🔄 [COMPUTER_STOP] 准备调用 StopAgent RPC: project_id={}",
        project_id
    );

    // 提取 gRPC 地址
    let grpc_addr = extract_grpc_addr(&container_info.service_url)?;
    info!("🌐 [COMPUTER_STOP] gRPC 地址: {}", grpc_addr);

    // 调用 StopAgent RPC
    match crate::grpc::grpc_stop_agent_with_pool(
        &state.grpc_pool,
        &grpc_addr,
        project_id.clone(),
        request
            .session_id
            .clone()
            .or_else(|| Some("用户请求停止".to_string())),
        false, // force=false，优雅停止
    )
    .await
    {
        Ok(response) => {
            info!(
                "📥 [COMPUTER_STOP] 收到 StopAgent 响应: result={}, success={}",
                response.result, response.success
            );

            if response.success {
                let message = format!(
                    "Agent {} 已成功停止，容器 {} 继续运行",
                    project_id, container_info.container_id
                );

                let stop_response = ComputerAgentStopResponse {
                    success: true,
                    message,
                    user_id: user_id.clone(),
                    project_id: project_id.clone(),
                };

                info!(
                    "✅ [COMPUTER_STOP] Agent 停止完成: user_id={}, project_id={}",
                    user_id, project_id
                );
                return Ok(HttpResult::success(stop_response));
            } else {
                // Agent 停止失败或已经停止
                match response.result.as_str() {
                    "not_found" => {
                        warn!("⚠️ [COMPUTER_STOP] Agent 未找到: project_id={}", project_id);
                        return Ok(HttpResult::error(
                            "NOT_FOUND",
                            &format!("找不到项目 {} 的 Agent", project_id),
                        ));
                    }
                    "already_stopped" => {
                        info!(
                            "ℹ️ [COMPUTER_STOP] Agent 已经处于停止状态: project_id={}",
                            project_id
                        );
                        let message = format!(
                            "Agent {} 已经处于停止状态，容器 {} 继续运行",
                            project_id, container_info.container_id
                        );
                        let stop_response = ComputerAgentStopResponse {
                            success: true,
                            message,
                            user_id: user_id.clone(),
                            project_id: project_id.clone(),
                        };
                        return Ok(HttpResult::success(stop_response));
                    }
                    "error" => {
                        let err_msg = response.message.unwrap_or_else(|| "未知错误".to_string());
                        error!("❌ [COMPUTER_STOP] Agent 停止失败: {}", err_msg);
                        return Ok(HttpResult::error("STOP_FAILED", &err_msg));
                    }
                    _ => {
                        warn!("⚠️ [COMPUTER_STOP] 未知的响应结果: {}", response.result);
                        return Ok(HttpResult::error(
                            "UNKNOWN_RESULT",
                            &format!("未知的响应结果: {}", response.result),
                        ));
                    }
                }
            }
        }
        Err(e) => {
            error!(
                "❌ [COMPUTER_STOP] 调用 StopAgent RPC 失败: {}, project_id={}",
                e, project_id
            );
            return Ok(HttpResult::error(
                "GRPC_ERROR",
                &format!("调用 StopAgent RPC 失败: {}", e),
            ));
        }
    }
}
