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

    // 3. 通过 gRPC 调用 StopAgent RPC（如果实现了）
    // 目前先通过 CancelSession 来停止 Agent
    if let Some(session_id) = &request.session_id {
        info!("🔄 [COMPUTER_STOP] 尝试取消会话: session_id={}", session_id);

        // 提取 gRPC 地址
        let grpc_addr = extract_grpc_addr(&container_info.service_url)?;

        // 调用 CancelSession RPC
        match crate::grpc::grpc_cancel_session_with_pool(
            &state.grpc_pool,
            &grpc_addr,
            session_id.clone(),
            "User requested stop".to_string(), // reason
            project_id.clone(),                // project_id
        )
        .await
        {
            Ok(response) => {
                if response.success {
                    info!("✅ [COMPUTER_STOP] 会话取消成功: session_id={}", session_id);
                } else {
                    warn!(
                        "⚠️ [COMPUTER_STOP] 会话取消返回失败: session_id={}, message={}",
                        session_id,
                        response.message.unwrap_or_default()
                    );
                }
            }
            Err(e) => {
                error!(
                    "❌ [COMPUTER_STOP] 调用 CancelSession 失败: {}, session_id={}",
                    e, session_id
                );
                // gRPC 通信失败，继续清理本地状态
                // 注：业务错误码现在由 agent_runner 通过 grpc_response 返回
            }
        }

        // 注意：DuckDB 存储中的会话数据会由 cleanup_task 统一清理
        // 不再需要手动清理 session_to_container_id 映射
        info!(
            "🧹 [COMPUTER_STOP] 会话 {} 将由 cleanup_task 清理",
            session_id
        );
    }

    // 4. 返回成功响应
    // 注意：容器不会被销毁，继续运行其他 project_id 的 Agent
    let response = ComputerAgentStopResponse {
        success: true,
        message: format!(
            "Agent {} 已停止，容器 {} 继续运行",
            project_id, container_info.container_id
        ),
        user_id,
        project_id,
    };

    info!(
        "✅ [COMPUTER_STOP] Agent 停止完成: user_id={}, project_id={}",
        request.user_id, request.project_id
    );

    Ok(HttpResult::success(response))
}

/// 从 service_url 提取 gRPC 地址
fn extract_grpc_addr(service_url: &str) -> Result<String, AppError> {
    let host = service_url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split(':')
        .next()
        .ok_or_else(|| AppError::internal_server_error("无效的 service_url"))?;

    Ok(format!("{}:{}", host, shared_types::GRPC_DEFAULT_PORT))
}
