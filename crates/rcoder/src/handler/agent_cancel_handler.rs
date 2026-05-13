//! Agent任务取消处理器
//!
//! 转发取消请求到容器内的 agent_runner 服务

#![allow(dead_code)]

use axum::extract::State;
use axum::http::HeaderMap;
use serde::Deserialize;
use std::sync::Arc;
use tracing::{debug, error, info, instrument, warn};
use utoipa::ToSchema;

use crate::router::AppState;
use docker_manager::ContainerBasicInfo;
use shared_types::{AppError, AgentCancelRequest, AgentCancelResponse, ComputerAgentCancelRequest, HttpResult};

use super::utils::{I18nJsonOrQuery, extract_grpc_addr, get_locale_from_headers};

/// Computer Agent 取消任务的查询参数（仅用于测试）
#[derive(Debug, Deserialize, ToSchema)]
pub struct ComputerCancelQuery {
    /// 用户ID，用于标识特定的用户容器（ComputerAgentRunner模式，可与 pod_id 二选一）
    #[schema(example = "user_123")]
    pub user_id: Option<String>,
    /// 项目ID，必填，用于标识要取消的特定项目的 agent
    #[schema(example = "project456")]
    pub project_id: String,
    /// 会话ID，用于标识要取消的会话（可选）
    #[serde(default)]
    #[schema(example = "session789")]
    pub session_id: Option<String>,
    /// Pod ID，用于共享容器模式下的容器定位（可选）
    #[serde(default)]
    pub pod_id: Option<String>,
    /// 租户ID（可选）
    #[serde(default, deserialize_with = "shared_types::flexible_string::flexible_string")]
    pub tenant_id: Option<String>,
    /// 空间ID（可选）
    #[serde(default, deserialize_with = "shared_types::flexible_string::flexible_string")]
    pub space_id: Option<String>,
    /// 隔离类型（可选），如 "project", "tenant", "space"
    #[serde(default)]
    pub isolation_type: Option<String>,
}

/// 取消操作的标识符
#[derive(Debug, Clone)]
enum CancelIdentifier {
    /// RCoder 模式：使用 project_id
    Project(String),
    /// ComputerAgentRunner 模式：使用 user_id
    User(String),
    /// 共享容器模式：使用 pod_id
    Pod(String),
}

/// 统一的容器查询函数 - 通过 DuckDB 查询
async fn get_container_for_cancel_duckdb(
    state: &AppState,
    identifier: &CancelIdentifier,
) -> Result<Option<ContainerBasicInfo>, AppError> {
    let identifier_display = match identifier {
        CancelIdentifier::Project(pid) => format!("project_id={}", pid),
        CancelIdentifier::User(uid) => format!("user_id={}", uid),
        CancelIdentifier::Pod(pod_id) => format!("pod_id={}", pod_id),
    };

    info!(
        "🔍 [CANCEL_CONTAINER_DUCKDB] Looking up container: {}",
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
        CancelIdentifier::Pod(pod_id) => {
            // 共享容器模式：通过 pod_id 查询容器
            // 目前暂时使用 get_container_by_user_id 作为占位，后续需要实现 get_container_by_pod_id
            state.projects.get_container_by_pod_id(pod_id)
        }
    };

    if let Some(ref info) = container_info {
        info!(
            "✅ [CANCEL_CONTAINER_DUCKDB] Container found: {}, container_id={}, service_url={}",
            identifier_display, info.container_id, info.service_url
        );
    } else {
        info!(
            "ℹ️ [CANCEL_CONTAINER_DUCKDB] Container not found: {}, no need to cancel",
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
    locale: &'static str,
    rcoder_prefix: &str,
    computer_prefix: &str,
) -> Result<HttpResult<AgentCancelResponse>, AppError> {
    let session_id_display = session_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| "None".to_string());
    info!(
        "📤 [CANCEL_FORWARD] Forwarding cancel request to container (gRPC): project_id={}, session_id={}, container_id={}",
        project_id, session_id_display, container_info.container_id
    );

    // 🎯 使用 gRPC 替代 HTTP
    // 从 service_url 提取 gRPC 地址
    let grpc_addr = extract_grpc_addr(&container_info.service_url)?;

    info!(
        "📡 [CANCEL_FORWARD] Sending gRPC cancel request to: {}, session_id={}",
        grpc_addr, session_id_display
    );

    // 构建 session_id（如果未提供则使用空字符串，由 Agent Runner 根据 project_id 查找）
    let session_id_str = session_id.unwrap_or("").to_string();
    let reason = "User requested cancellation".to_string();

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
                    "✅ [CANCEL_FORWARD] gRPC cancel succeeded: session_id={}",
                    session_id_str
                );
                Ok(HttpResult::success(AgentCancelResponse {
                    success: true,
                    session_id: session_id_str,
                }))
            } else {
                let error_msg = grpc_response
                    .message
                    .unwrap_or_else(|| "Unknown error".to_string());
                error!("[CANCEL_FORWARD] gRPC cancelfailed: {}", error_msg);
                Ok(HttpResult::error_with_locale(
                    shared_types::error_codes::ERR_CANCEL_FAILED,
                    locale,
                ))
            }
        }
        Err(e) => {
            error!("[CANCEL_FORWARD] gRPC call failed: {}", e);

            // 检查特定的 gRPC 错误码并分类处理
            if let Some(status) = crate::grpc::extract_grpc_status(&e) {
                use tonic::Code;
                match status.code() {
                    Code::NotFound => {
                        // 会话或 Agent 不存在，返回成功（幂等设计）
                        info!("[CANCEL_FORWARD] Session not found, cancel succeeded");
                        return Ok(HttpResult::success(AgentCancelResponse {
                            success: true,
                            session_id: session_id.unwrap_or("").to_string(),
                        }));
                    }
                    Code::Unavailable => {
                        // Agent Worker 不可用，需要判断是容器已销毁还是临时故障
                        // 通过 Docker API 检查容器是否真的存在
                        let container_exists = check_container_exists_by_info(container_info, rcoder_prefix, computer_prefix).await;

                        if !container_exists {
                            // 容器已销毁，取消目标已达成（幂等设计）
                            info!(
                                "[CANCEL_FORWARD] container already destroyed, cancel request already completed"
                            );
                            return Ok(HttpResult::success(AgentCancelResponse {
                                success: true,
                                session_id: session_id.unwrap_or("").to_string(),
                            }));
                        } else {
                            // 容器存在但服务不可用（可能是临时故障），返回错误
                            warn!(
                                "[CANCEL_FORWARD] Agent Worker unavailable (container exists, may be temporary failure)"
                            );
                            return Ok(HttpResult::error_with_locale(
                                shared_types::error_codes::ERR_SERVICE_UNAVAILABLE,
                                locale,
                            ));
                        }
                    }
                    other_code => {
                        // 其他 gRPC 状态码
                        error!("[CANCEL_FORWARD] gRPC error code: {:?}", other_code);
                    }
                }
            }

            // 其他 gRPC 通信失败（网络错误等）
            Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_GRPC_ERROR,
                locale,
            ))
        }
    }
}

/// 检查容器是否真实存在（通过 Docker API）
///
/// 用于区分 Unavailable 错误的原因：
/// - 容器已销毁 → 返回 false（取消目标已达成）
/// - 容器存在但服务不可用 → 返回 true（临时故障）
///
/// 使用容器名称而非 ID，因为容器重启后 ID 会变，但名称不变
async fn check_container_exists_by_info(
    container_info: &ContainerBasicInfo,
    rcoder_prefix: &str,
    computer_prefix: &str,
) -> bool {
    match docker_manager::runtime::RuntimeManager::get().await {
        Ok(runtime) => {
            // 使用配置化的前缀，而不是硬编码的 ServiceType::container_prefix()

            let query = if let Some(identifier) = container_info
                .container_name
                .strip_prefix(&format!("{}-", computer_prefix))
            {
                runtime
                    .get_container_info_by_identifier(
                        identifier,
                        &shared_types::ServiceType::ComputerAgentRunner,
                    )
                    .await
            } else if let Some(identifier) = container_info
                .container_name
                .strip_prefix(&format!("{}-", rcoder_prefix))
            {
                runtime
                    .get_container_info_by_identifier(identifier, &shared_types::ServiceType::RCoder)
                    .await
            } else {
                return true;
            };

            match query {
                Ok(Some(info)) => {
                    debug!(
                        "🔍 [CANCEL_FORWARD] Runtime container exists: name={}, id={}",
                        info.container_name, info.container_id
                    );
                    true
                }
                Ok(None) => {
                    info!(
                        "🔍 [CANCEL_FORWARD] Runtime container not found (already destroyed): {}",
                        container_info.container_name
                    );
                    false
                }
                Err(e) => {
                    warn!(
                        "⚠️ [CANCEL_FORWARD] Failed to query runtime container status: {}, conservatively assuming container exists",
                        e
                    );
                    true
                }
            }
        }
        Err(e) => {
            // 无法获取 runtime，保守地认为容器存在
            warn!(
                "[CANCEL_FORWARD] Failed to get runtime: {}, conservatively assuming container exists",
                e
            );
            true
        }
    }
}

/// 内部核心处理函数 v2：处理会话取消请求（支持多种服务类型）
///
/// 使用 DuckDB 统一查询，支持 RCoder 和 ComputerAgentRunner 两种模式
async fn handle_session_cancel_internal_v2(
    state: &AppState,
    identifier: CancelIdentifier,
    project_id: String,         // 必填：传递给 agent_runner 的项目ID
    session_id: Option<String>, // 可选：会话ID
    locale: &'static str,
) -> Result<HttpResult<AgentCancelResponse>, AppError> {
    let session_id_display = session_id
        .as_deref()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "None".to_string());

    let identifier_display = match &identifier {
        CancelIdentifier::Project(pid) => format!("project_id={}", pid),
        CancelIdentifier::User(uid) => format!("user_id={}", uid),
        CancelIdentifier::Pod(pod_id) => format!("pod_id={}", pod_id),
    };

    info!(
        "🛑 [CANCEL_FORWARD_V2] Received cancel task request: session_id={}, project_id={}, {}",
        session_id_display, project_id, identifier_display
    );

    // 获取容器（不创建）
    let container_info = get_container_for_cancel_duckdb(state, &identifier).await?;

    // 如果容器不存在，说明任务已经结束或从未启动，直接返回成功
    let Some(container_info) = container_info else {
        info!(
            "✅ [CANCEL_FORWARD_V2] Container not found, cancel target already achieved: {}",
            identifier_display
        );
        return Ok(HttpResult::success(AgentCancelResponse {
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
        locale,
        &state.container_prefix_rcoder,
        &state.container_prefix_computer,
    )
    .await;

    match &result {
        Ok(_) => {
            info!(
                "✅ [CANCEL_FORWARD_V2] Cancel request handled successfully: project_id={}, {}",
                project_id, identifier_display
            );
        }
        Err(e) => {
            error!(
                "❌ [CANCEL_FORWARD_V2] Cancel request handling failed: project_id={}, {}, error={}",
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
    request_body = AgentCancelRequest,
    responses(
        (
            status = 200,
            description = "成功转发取消请求到容器",
            body = HttpResult<AgentCancelResponse>,
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
            body = HttpResult<String>
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
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<AgentCancelRequest>,
) -> Result<HttpResult<AgentCancelResponse>, AppError> {
    let locale = get_locale_from_headers(&headers);

    // 使用 garde 进行字段校验
    let I18nJsonOrQuery(request) = I18nJsonOrQuery(request).validate_into_app_error()?;
    let project_id = request.project_id.as_ref().expect("validated: project_id is required and non-empty");

    info!(
        "🚫 [CANCEL] Agent cancel request: project_id={}, session_id={:?}",
        project_id, request.session_id
    );

    handle_session_cancel_internal_v2(
        &state,
        CancelIdentifier::Project(project_id.to_string()),
        project_id.to_string(),
        request.session_id,
        locale,
    )
    .await
}

/// 处理 Computer Agent 任务取消请求
///
/// 转发取消请求到容器内的 agent_runner 服务（使用 gRPC）
#[utoipa::path(
    post,
    path = "/computer/agent/session/cancel",
    request_body = ComputerAgentCancelRequest,
    responses(
        (
            status = 200,
            description = "成功转发取消请求到容器",
            body = HttpResult<AgentCancelResponse>,
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
                    "message": "user_id 或 pod_id is required"
                }
            })
        ),
        (
            status = 401,
            description = "API Key 鉴权失败",
            body = HttpResult<String>
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
    description = "将 Computer Agent 取消请求通过 gRPC 转发到容器内的 agent_runner 服务，支持通过 user_id 或 pod_id 定位用户容器"
)]
#[instrument(skip(state), fields(user_id = ?request.user_id.as_deref(), project_id = %request.project_id, pod_id = ?request.pod_id.as_deref()))]
pub async fn computer_agent_session_cancel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerAgentCancelRequest>,
) -> Result<HttpResult<AgentCancelResponse>, AppError> {
    let locale = get_locale_from_headers(&headers);

    // 验证 user_id 或 pod_id 至少有一个
    let has_user_id = request.user_id.as_ref().map(|s| !s.trim().is_empty()).unwrap_or(false);
    let has_pod_id = request.pod_id.as_ref().map(|s| !s.trim().is_empty()).unwrap_or(false);
    if !has_user_id && !has_pod_id {
        error!("[COMPUTER_CANCEL] user_id or pod_id is required");
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        ));
    }

    // 验证 project_id 不为空
    if request.project_id.trim().is_empty() {
        error!("[COMPUTER_CANCEL] project_id is required");
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        ));
    }

    info!(
        "🚀 [COMPUTER_CANCEL] Starting to process cancel request: user_id={:?}, pod_id={:?}, project_id={}, session_id={:?}",
        request.user_id, request.pod_id, request.project_id, request.session_id
    );

    // 使用 user_id 或 pod_id 来构建 CancelIdentifier
    let identifier = if has_user_id {
        CancelIdentifier::User(request.user_id.clone().unwrap())
    } else {
        CancelIdentifier::Pod(request.pod_id.clone().unwrap())
    };

    handle_session_cancel_internal_v2(
        &state,
        identifier,
        request.project_id, // 必填的 project_id
        request.session_id,
        locale,
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
        assert_eq!(query.user_id, Some("user_123".to_string()));
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
        assert_eq!(query.user_id, Some("user_123".to_string()));
        assert_eq!(query.project_id, "project_456");
        assert_eq!(query.session_id, None);
    }

    #[test]
    fn test_cancel_identifier_display() {
        // 测试 CancelIdentifier 的显示格式
        let project_id = CancelIdentifier::Project("test_project".to_string());
        let user_id = CancelIdentifier::User("test_user".to_string());
        let pod_id = CancelIdentifier::Pod("test_pod".to_string());

        let display = match &project_id {
            CancelIdentifier::Project(pid) => format!("project_id={}", pid),
            _ => unreachable!(),
        };
        assert_eq!(display, "project_id=test_project");

        let display = match &user_id {
            CancelIdentifier::User(uid) => format!("user_id={}", uid),
            _ => unreachable!(),
        };
        assert_eq!(display, "user_id=test_user");

        let display = match &pod_id {
            CancelIdentifier::Pod(pod_id) => format!("pod_id={}", pod_id),
            _ => unreachable!(),
        };
        assert_eq!(display, "pod_id=test_pod");
    }
}
