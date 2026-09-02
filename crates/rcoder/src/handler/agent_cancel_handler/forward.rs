//! 取消转发机制层（自 agent_cancel_handler 拆出；原样搬迁）。

use std::sync::Arc;
use tracing::{debug, error, info, warn};

use super::*;

/// 取消操作的标识符
#[derive(Debug, Clone)]
pub(super) enum CancelIdentifier {
    /// RCoder 模式：使用 project_id
    Project(String),
    /// ComputerAgentRunner 模式：使用 user_id
    User(String),
    /// 共享容器模式：使用 pod_id
    Pod(String),
}

/// 统一的容器查询函数
async fn get_container_for_cancel(
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
                .and_then(|info| info.container_info())
        }
        CancelIdentifier::User(user_id) => {
            // ComputerAgentRunner 模式：通过 user_id 查询容器
            state
                .projects
                .get_container_by_user_id(user_id, &shared_types::ServiceType::ComputerAgentRunner)
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
/// Cancel 请求转发参数
///
/// 封装了转发 Cancel 请求到容器服务所需的所有参数，
/// 避免函数参数过多。
struct CancelForwardParams<'a> {
    /// 项目 ID
    project_id: &'a str,
    /// 会话 ID（可选）
    session_id: Option<&'a str>,
    /// 容器信息
    container_info: &'a ContainerBasicInfo,
    /// gRPC 连接池
    grpc_pool: &'a Arc<crate::grpc::GrpcChannelPool>,
    /// 语言设置
    locale: &'static str,
    /// 容器运行时
    runtime: &'a Arc<dyn container_runtime_api::ContainerRuntime>,
    /// RCoder 容器前缀
    rcoder_prefix: &'a str,
    /// Computer 容器前缀
    computer_prefix: &'a str,
}

///
/// 🎯 使用 gRPC CancelSession RPC 替代 HTTP 转发
async fn forward_cancel_request_to_container_service(
    params: CancelForwardParams<'_>,
) -> Result<HttpResult<AgentCancelResponse>, AppError> {
    let session_id_display = params
        .session_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| "None".to_string());
    info!(
        "📤 [CANCEL_FORWARD] Forwarding cancel request to container (gRPC): project_id={}, session_id={}, container_id={}",
        params.project_id, session_id_display, params.container_info.container_id
    );

    // 🎯 使用 gRPC 替代 HTTP
    // 从 service_url 提取 gRPC 地址
    let grpc_addr = extract_grpc_addr(&params.container_info.service_url)?;

    info!(
        "📡 [CANCEL_FORWARD] Sending gRPC cancel request to: {}, session_id={}",
        grpc_addr, session_id_display
    );

    // 构建 session_id（如果未提供则使用空字符串，由 Agent Runner 根据 project_id 查找）
    let session_id_str = params.session_id.unwrap_or("").to_string();
    let reason = "User requested cancellation".to_string();

    // 调用 gRPC CancelSession
    match crate::grpc::grpc_cancel_session_with_pool(
        params.grpc_pool,
        &grpc_addr,
        session_id_str.clone(),
        reason,
        params.project_id.to_string(),
        None, // 使用默认超时 (GRPC_CANCEL_SESSION_TIMEOUT_SECS)
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
                    params.locale,
                ))
            }
        }
        Err(grpc_err) => {
            error!("[CANCEL_FORWARD] gRPC call failed: {}", grpc_err);

            // 使用 GrpcError 枚举进行错误分类处理
            match &grpc_err {
                crate::grpc::GrpcError::Status(status) => {
                    use tonic::Code;
                    match status.code() {
                        Code::NotFound => {
                            // 会话或 Agent 不存在，返回成功（幂等设计）
                            info!("[CANCEL_FORWARD] Session not found, cancel succeeded");
                            return Ok(HttpResult::success(AgentCancelResponse {
                                success: true,
                                session_id: params.session_id.unwrap_or("").to_string(),
                            }));
                        }
                        Code::Unavailable => {
                            // Agent Worker 不可用，需要判断是容器已销毁还是临时故障
                            // 通过 Docker API 检查容器是否真的存在
                            let container_exists = check_container_exists_by_info(
                                params.runtime,
                                params.container_info,
                                params.rcoder_prefix,
                                params.computer_prefix,
                            )
                            .await;

                            if !container_exists {
                                // 容器已销毁，取消目标已达成（幂等设计）
                                info!(
                                    "[CANCEL_FORWARD] container already destroyed, cancel request already completed"
                                );
                                return Ok(HttpResult::success(AgentCancelResponse {
                                    success: true,
                                    session_id: params.session_id.unwrap_or("").to_string(),
                                }));
                            } else {
                                // 容器存在但服务不可用（可能是临时故障），返回错误
                                warn!(
                                    "[CANCEL_FORWARD] Agent Worker unavailable (container exists, may be temporary failure)"
                                );
                                return Ok(HttpResult::error_with_locale(
                                    shared_types::error_codes::ERR_SERVICE_UNAVAILABLE,
                                    params.locale,
                                ));
                            }
                        }
                        other_code => {
                            // 其他 gRPC 状态码
                            error!("[CANCEL_FORWARD] gRPC error code: {:?}", other_code);
                        }
                    }
                }
                crate::grpc::GrpcError::Transport(_) => {
                    // 连接层错误，通常是网络问题
                    error!("[CANCEL_FORWARD] gRPC transport error: {}", grpc_err);
                }
            }

            // 其他 gRPC 通信失败（网络错误等）
            Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_GRPC_ERROR,
                params.locale,
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
    runtime: &Arc<dyn container_runtime_api::ContainerRuntime>,
    container_info: &ContainerBasicInfo,
    rcoder_prefix: &str,
    computer_prefix: &str,
) -> bool {
    let query = if let Some((identifier, service_type)) = container_identity_from_name(
        &container_info.container_name,
        rcoder_prefix,
        computer_prefix,
    ) {
        runtime
            .get_container_info_by_identifier(identifier, &service_type)
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

/// 内部核心处理函数 v2：处理会话取消请求（支持多种服务类型）
///
/// 使用 storage lookup，支持 RCoder 和 ComputerAgentRunner 两种模式
pub(super) async fn handle_session_cancel_internal_v2(
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
    let container_info = get_container_for_cancel(state, &identifier).await?;

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
    let cancel_params = CancelForwardParams {
        project_id: &project_id,
        session_id: session_id.as_deref(),
        container_info: &container_info,
        grpc_pool: &state.grpc_pool,
        locale,
        runtime: state.runtime(),
        rcoder_prefix: &state.container_prefix_rcoder,
        computer_prefix: &state.container_prefix_computer,
    };
    let result = forward_cancel_request_to_container_service(cancel_params).await;

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

#[cfg(test)]
mod tests {
    use super::*;

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
