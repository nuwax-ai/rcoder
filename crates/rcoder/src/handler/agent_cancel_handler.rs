//! Agent任务取消处理器
//!
//! 转发取消请求到容器内的 agent_runner 服务

use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, info, instrument};
use utoipa::{IntoParams, ToSchema};

use crate::router::AppState;
use docker_manager::ContainerBasicInfo;
use shared_types::{AppError, HttpResult};

use super::utils::extract_grpc_addr;

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

/// Computer Agent 取消任务的查询参数
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct ComputerCancelQuery {
    /// 用户ID，用于标识特定的用户容器（ComputerAgentRunner模式）
    #[param(example = "user_123")]
    #[schema(example = "user_123")]
    pub user_id: String,
    /// 项目ID，必填，用于标识要取消的特定项目的 agent
    #[param(example = "project456")]
    #[schema(example = "project456")]
    pub project_id: String,
    /// 会话ID，用于标识要取消的会话（可选）
    #[param(example = "session789")]
    #[serde(default)]
    #[schema(example = "session789")]
    pub session_id: Option<String>,
}

/// 取消操作的标识符
#[derive(Debug, Clone)]
enum CancelIdentifier {
    /// RCoder 模式：使用 project_id
    Project(String),
    /// ComputerAgentRunner 模式：使用 user_id
    User(String),
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

/// 获取容器（用于取消请求，不创建）- 兼容旧版本
async fn get_container_for_cancel(
    project_id: &str,
) -> Result<Option<ContainerBasicInfo>, AppError> {
    info!("🔍 [CANCEL_CONTAINER] 查找容器: project_id={}", project_id);

    // 只获取容器，不创建
    let container_info =
        crate::service::container_manager::ContainerManager::get_container_info(project_id).await?;

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

/// 统一的容器查询函数 - 通过 DuckDB 查询
async fn get_container_for_cancel_duckdb(
    state: &AppState,
    identifier: &CancelIdentifier,
) -> Result<Option<ContainerBasicInfo>, AppError> {
    let identifier_display = match identifier {
        CancelIdentifier::Project(pid) => format!("project_id={}", pid),
        CancelIdentifier::User(uid) => format!("user_id={}", uid),
    };

    info!(
        "🔍 [CANCEL_CONTAINER_DUCKDB] 查找容器: {}",
        identifier_display
    );

    let container_info = match identifier {
        CancelIdentifier::Project(project_id) => {
            // RCoder 模式：直接通过 project_id 查询
            // ProjectAdapter.get() 内部会调用 get_container_for_project
            state
                .get_project(project_id)
                .and_then(|info| info.container().cloned())
        }
        CancelIdentifier::User(user_id) => {
            // ComputerAgentRunner 模式：通过 user_id 查询容器
            // 使用新添加的 get_container_by_user_id 方法
            state.projects.get_container_by_user_id(user_id)
        }
    };

    if let Some(ref info) = container_info {
        info!(
            "✅ [CANCEL_CONTAINER_DUCKDB] 找到容器: {}, container_id={}, service_url={}",
            identifier_display, info.container_id, info.service_url
        );
    } else {
        info!(
            "ℹ️ [CANCEL_CONTAINER_DUCKDB] 容器不存在: {}, 无需取消",
            identifier_display
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

    info!(
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
                Ok(HttpResult::error(
                    shared_types::error_codes::ERR_CANCEL_FAILED,
                    &error_msg,
                ))
            }
        }
        Err(e) => {
            error!("❌ [CANCEL_FORWARD] gRPC 调用失败: {}", e);

            // 只保留 NotFound 的特殊处理（幂等设计）
            // 注：业务错误码现在由 agent_runner 通过 grpc_response 返回
            if let Some(status) = crate::grpc::extract_grpc_status(&e) {
                use tonic::Code;
                if status.code() == Code::NotFound {
                    // 会话或 Agent 不存在，返回成功（幂等设计）
                    return Ok(HttpResult::success(CancelResponse {
                        success: true,
                        session_id: session_id.unwrap_or("").to_string(),
                    }));
                }
            }

            // gRPC 通信失败
            Ok(HttpResult::error(
                shared_types::error_codes::ERR_GRPC_ERROR,
                &format!("gRPC 调用失败: {}", e),
            ))
        }
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

/// 内部核心处理函数 v2：处理会话取消请求（支持多种服务类型）
///
/// 使用 DuckDB 统一查询，支持 RCoder 和 ComputerAgentRunner 两种模式
async fn handle_session_cancel_internal_v2(
    state: &AppState,
    identifier: CancelIdentifier,
    project_id: String,         // 必填：传递给 agent_runner 的项目ID
    session_id: Option<String>, // 可选：会话ID
) -> Result<HttpResult<CancelResponse>, AppError> {
    let session_id_display = session_id
        .as_deref()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "None".to_string());

    let identifier_display = match &identifier {
        CancelIdentifier::Project(pid) => format!("project_id={}", pid),
        CancelIdentifier::User(uid) => format!("user_id={}", uid),
    };

    info!(
        "🛑 [CANCEL_FORWARD_V2] 收到取消任务请求: session_id={}, project_id={}, {}",
        session_id_display, project_id, identifier_display
    );

    // 获取容器（不创建）
    let container_info = get_container_for_cancel_duckdb(state, &identifier).await?;

    // 如果容器不存在，说明任务已经结束或从未启动，直接返回成功
    let Some(container_info) = container_info else {
        info!(
            "✅ [CANCEL_FORWARD_V2] 容器不存在，取消目标已达成: {}",
            identifier_display
        );
        return Ok(HttpResult::success(CancelResponse {
            success: true,
            session_id: session_id.unwrap_or_else(|| "all".to_string()),
        }));
    };

    // 转发取消请求到容器服务
    let result = forward_cancel_request_to_container_service(
        &project_id, // 使用传入的 project_id
        session_id.as_deref(),
        &container_info,
        &state.grpc_pool,
    )
    .await;

    match &result {
        Ok(_) => {
            info!(
                "✅ [CANCEL_FORWARD_V2] 取消请求处理成功: project_id={}, {}",
                project_id, identifier_display
            );
        }
        Err(e) => {
            error!(
                "❌ [CANCEL_FORWARD_V2] 取消请求处理失败: project_id={}, {}, error={}",
                project_id, identifier_display, e
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
            status = 401,
            description = "API Key 鉴权失败",
            body = String
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
#[instrument(skip(state), fields(project_id = %query.project_id))]
pub async fn agent_session_cancel(
    State(state): State<Arc<AppState>>,
    Query(query): Query<CancelQuery>,
) -> Result<HttpResult<CancelResponse>, AppError> {
    // 使用新的 v2 版本，保持向后兼容
    let project_id = query.project_id.clone();
    handle_session_cancel_internal_v2(
        &state,
        CancelIdentifier::Project(project_id.clone()),
        project_id,
        query.session_id,
    )
    .await
}

/// 处理 Computer Agent 任务取消请求
///
/// 转发取消请求到容器内的 agent_runner 服务（使用 gRPC）
#[utoipa::path(
    post,
    path = "/computer/agent/session/cancel",
    params(
        ComputerCancelQuery
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
                    "code": "ERR_VALIDATION",
                    "message": "user_id 或 project_id 不能为空"
                }
            })
        ),
        (
            status = 401,
            description = "API Key 鉴权失败",
            body = String
        ),
        (
            status = 404,
            description = "未找到对应的用户容器或会话",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "CONTAINER_NOT_FOUND",
                    "message": "User container not found"
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
    summary = "转发 Computer Agent 任务取消请求（支持 user_id）",
    description = "将 Computer Agent 取消请求通过 gRPC 转发到容器内的 agent_runner 服务，支持通过 user_id 定位用户容器"
)]
#[instrument(skip(state), fields(user_id = %query.user_id, project_id = %query.project_id))]
pub async fn computer_agent_session_cancel(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ComputerCancelQuery>,
) -> Result<HttpResult<CancelResponse>, AppError> {
    // 验证 user_id 不为空
    if query.user_id.trim().is_empty() {
        error!("❌ [COMPUTER_CANCEL] user_id 不能为空");
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "user_id 不能为空",
        ));
    }

    // 验证 project_id 不为空
    if query.project_id.trim().is_empty() {
        error!("❌ [COMPUTER_CANCEL] project_id 不能为空");
        return Ok(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "project_id 不能为空",
        ));
    }

    info!(
        "🚀 [COMPUTER_CANCEL] 开始处理取消请求: user_id={}, project_id={}, session_id={:?}",
        query.user_id, query.project_id, query.session_id
    );

    handle_session_cancel_internal_v2(
        &state,
        CancelIdentifier::User(query.user_id),
        query.project_id, // 必填的 project_id
        query.session_id,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_computer_cancel_query_deserialization() {
        // 测试 ComputerCancelQuery 反序列化
        let query_json = json!({
            "user_id": "user_123",
            "project_id": "project_456",
            "session_id": "session_789"
        });

        let query: ComputerCancelQuery = serde_json::from_value(query_json).unwrap();
        assert_eq!(query.user_id, "user_123");
        assert_eq!(query.project_id, "project_456");
        assert_eq!(query.session_id, Some("session_789".to_string()));
    }

    #[test]
    fn test_computer_cancel_query_optional_session() {
        // 测试不带 session_id 的情况
        let query_json = json!({
            "user_id": "user_123",
            "project_id": "project_456"
        });

        let query: ComputerCancelQuery = serde_json::from_value(query_json).unwrap();
        assert_eq!(query.user_id, "user_123");
        assert_eq!(query.project_id, "project_456");
        assert_eq!(query.session_id, None);
    }

    #[test]
    fn test_cancel_identifier_display() {
        // 测试 CancelIdentifier 的显示格式
        let project_id = CancelIdentifier::Project("test_project".to_string());
        let user_id = CancelIdentifier::User("test_user".to_string());

        let display = match project_id {
            CancelIdentifier::Project(pid) => format!("project_id={}", pid),
            CancelIdentifier::User(_) => unreachable!(),
        };
        assert_eq!(display, "project_id=test_project");

        let display = match user_id {
            CancelIdentifier::User(uid) => format!("user_id={}", uid),
            CancelIdentifier::Project(_) => unreachable!(),
        };
        assert_eq!(display, "user_id=test_user");
    }
}
