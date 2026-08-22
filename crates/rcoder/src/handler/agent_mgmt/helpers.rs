//! agent-mgmt 共享 helper（从 agent_mgmt_handler.rs 拆出）。
//!
//! 转发参数校验、容器目标解析（gRPC 地址）、转发 ctx 构建——query/install 两域共用。

use std::str::FromStr;

use shared_types::{AppError, IsolationType, RoutingParams, ServiceType, error_codes as ec};
use std::sync::Arc;
use tracing::warn;

use super::super::utils::AgentMgmtForwardCtx;
use crate::router::AppState;

pub(super) fn default_install_type() -> String {
    "BINARY".to_string()
}

// === 内部工具:提取 project + 构造转发 ctx ===

/// 验证多租户路由参数（复用 computer_chat_handler 的验证模式）
///
/// 规则:pod_id 有值时,isolation_type / tenant_id / space_id 必须非空且有效。
pub(super) fn validate_routing_params(routing: &RoutingParams) -> Result<(), AppError> {
    if let Some(ref pod_id) = routing.pod_id {
        if pod_id.trim().is_empty() {
            return Err(AppError::with_message(
                ec::ERR_VALIDATION,
                "pod_id cannot be empty".to_string(),
            ));
        }
        // pod_id 有值时,isolation_type 必填
        match routing.isolation_type.as_deref() {
            None | Some("") => {
                return Err(AppError::with_message(
                    ec::ERR_VALIDATION,
                    "isolation_type is required when pod_id is provided".to_string(),
                ));
            }
            Some(it) => {
                if IsolationType::from_str(it).is_err() {
                    return Err(AppError::with_message(
                        ec::ERR_VALIDATION,
                        format!(
                            "invalid isolation_type '{}', expected: tenant, space, project",
                            it
                        ),
                    ));
                }
            }
        }
        // pod_id 有值时,tenant_id 必填
        if routing
            .tenant_id
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
        {
            return Err(AppError::with_message(
                ec::ERR_VALIDATION,
                "tenant_id is required when pod_id is provided".to_string(),
            ));
        }
        // pod_id 有值时,space_id 必填
        if routing
            .space_id
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
        {
            return Err(AppError::with_message(
                ec::ERR_VALIDATION,
                "space_id is required when pod_id is provided".to_string(),
            ));
        }
    }
    Ok(())
}

/// 解析容器目标（支持 project_id 和 user_id/pod_id 两条查找路径）
///
/// - Path A: `project_id` 有值 → storage lookup（向后兼容）
/// - Path B: `user_id` 或 `pod_id` 有值 → 运行时容器查找（多租户模式）
/// - Path C: 都没有 → ERR_VALIDATION
pub(super) async fn resolve_container_target(
    state: &Arc<AppState>,
    project_id: Option<&str>,
    routing: &RoutingParams,
) -> Result<Arc<shared_types::ProjectAndContainerInfo>, AppError> {
    // Path A: project_id 优先（向后兼容）
    if let Some(pid) = project_id.filter(|s| !s.is_empty()) {
        return state.get_project(pid).ok_or_else(|| {
            AppError::with_i18n_key(ec::ERR_PROJECT_NOT_FOUND, "error.project_not_found")
        });
    }

    // Path B: user_id 或 pod_id 路由
    let container_identifier = routing
        .pod_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .or(routing.user_id.as_deref().filter(|s| !s.is_empty()));

    if let Some(identifier) = container_identifier {
        let container_info = state
            .runtime()
            .get_container_info_by_identifier(identifier, &ServiceType::ComputerAgentRunner)
            .await
            .map_err(|e| {
                warn!(
                    "[agent_mgmt] container lookup failed: identifier={}, error={}",
                    identifier, e
                );
                AppError::with_message(
                    ec::ERR_CONTAINER_NOT_FOUND,
                    format!("container lookup failed: {}", e),
                )
            })?
            .ok_or_else(|| {
                AppError::with_message(
                    ec::ERR_CONTAINER_NOT_FOUND,
                    format!("no running container found for identifier: {}", identifier),
                )
            })?;

        let mut info = shared_types::ProjectAndContainerInfo::new(String::new());
        info.set_user_id(routing.user_id.clone());
        info.set_pod_id(routing.pod_id.clone());
        info.set_container(Some(container_info));
        info.set_service_type(Some(ServiceType::ComputerAgentRunner));
        return Ok(Arc::new(info));
    }

    // Path C: 没有任何标识符
    Err(AppError::with_message(
        ec::ERR_VALIDATION,
        "project_id, user_id, or pod_id is required".to_string(),
    ))
}

pub(super) fn build_ctx(state: &Arc<AppState>) -> AgentMgmtForwardCtx {
    AgentMgmtForwardCtx::from_state(
        state.grpc_pool.clone(),
        state.config.app_manager.namespace.clone(),
        state.cluster_domain.clone(),
        shared_types::current_request_locale(),
    )
}

// === Handlers ===
