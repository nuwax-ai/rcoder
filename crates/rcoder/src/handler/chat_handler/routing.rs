//! chat 容器目标解析与项目记录维护（从 chat_handler.rs 拆出）。

use anyhow::Result;
use shared_types::{AgentChatRequest, ProjectAndContainerInfo};
use std::sync::Arc;
use tracing::{debug, error, info};

use docker_manager::ContainerBasicInfo;

use crate::handler::chat_forward::ChatFlowExit;
use crate::handler::pod_handler::resolve_resource_limits_from_config;
use crate::*;
use crate::{AppError, HttpResult, router::AppState};

/// 第二段：容器目标解析
///
/// 第一步：获取或创建容器（默认使用 ServiceType::WebAgentRunner）；
/// 第二步：获取或创建 ProjectAndContainerInfo（使用存储）；
/// 随后立即更新活动时间，防止 gRPC 请求期间被 cleanup_task 误清理。
pub(crate) async fn resolve_container_target(
    state: &Arc<AppState>,
    request: &AgentChatRequest,
    project_id: &str,
    container_work_path: &str,
    service_type: &shared_types::ServiceType,
) -> Result<ContainerBasicInfo, AppError> {
    let container_options = service::container_manager::ContainerCreateOptions {
        project_id,
        service_type,
        request_resource_limits: resolve_resource_limits_from_config(
            state,
            service_type,
            request
                .agent_config
                .as_ref()
                .and_then(|c| c.resource_limits.clone()),
        )?,
        pod_id: request.pod_id.as_deref(),
        isolation_type: request.isolation_type.as_deref(),
        tenant_id: request.tenant_id.as_deref(),
        space_id: request.space_id.as_deref(),
        container_work_path,
        runtime: state.runtime(),
    };
    let container_info =
        service::container_manager::ContainerManager::get_or_create_container(container_options)
            .await?;

    // 第二步：获取或创建 ProjectAndContainerInfo - 使用 存储
    drop(ensure_project_record(
        state,
        request,
        project_id,
        &container_info,
        service_type,
    )?);

    // 请求到达时立即更新活动时间（不等待请求执行结果）
    // 这样可以防止在 gRPC 请求期间被 cleanup_task 误清理
    state.update_activity(project_id);
    debug!("[CHAT] Updated activity time: project_id={}", project_id);

    Ok(container_info)
}

/// 获取或创建 ProjectAndContainerInfo（存在则按需更新扩展状态，不存在则新建）
pub(crate) fn ensure_project_record(
    state: &Arc<AppState>,
    request: &AgentChatRequest,
    project_id: &str,
    container_info: &ContainerBasicInfo,
    service_type: &shared_types::ServiceType,
) -> Result<Arc<ProjectAndContainerInfo>, AppError> {
    info!("[CHAT] Getting/creating project: project_id={}", project_id);

    // 检查项目是否存在
    if let Some(existing_info) = state.get_project(project_id) {
        info!(
            "[CHAT] Project exists, checking for update: project_id={}",
            project_id
        );

        // 检查是否需要更新扩展状态
        // 重要：也需要检查 service_type 和 pod_id 是否需要更新
        // 如果 service_type 或 pod_id 不匹配，需要更新以确保 container_key() 返回正确的值
        let needs_extended_update = existing_info.container_info().is_none()
            || existing_info.model_provider().is_none()
            || existing_info.request_id().is_none()
            || existing_info.service_type() != Some(service_type.clone())
            || existing_info.pod_id() != request.pod_id.as_deref();

        if needs_extended_update {
            // 创建更新后的信息
            let mut mutable_info = (*existing_info).clone();
            // 补充 pod_id（兼容旧数据或服务重启后丢失的情况）
            if mutable_info.pod_id().is_none() && request.pod_id.is_some() {
                mutable_info.set_pod_id(request.pod_id.clone());
            }
            mutable_info.update_extended_from_request(
                Some(container_info.clone()),
                request.model_provider.clone(),
                request.request_id.clone(),
                Some(service_type.clone()),
            );
            mutable_info.update_activity();

            let arc_info = Arc::new(mutable_info);
            state
                .insert_project(project_id.to_string(), arc_info.clone())
                .map_err(|e| {
                    tracing::error!("[STORAGE] insert_project failed: {}", e);
                    e
                })?;

            info!(
                "✅ [CHAT] Project info fully updated: project_id={}, container_id={}",
                project_id, container_info.container_id
            );

            Ok(arc_info)
        } else {
            // 只需要更新活动时间
            state.update_activity(project_id);
            info!("[CHAT] Activity time updated: project_id={}", project_id);
            Ok(existing_info)
        }
    } else {
        info!("[CHAT] Creating new project: project_id={}", project_id);

        // 创建新的 ProjectAndContainerInfo
        let mut new_info = ProjectAndContainerInfo::new(project_id.to_string());
        new_info.set_pod_id(request.pod_id.clone());
        new_info.update_extended_from_request(
            Some(container_info.clone()),
            request.model_provider.clone(),
            request.request_id.clone(),
            Some(service_type.clone()),
        );
        new_info.set_scope(
            request.tenant_id.clone(),
            request.space_id.clone(),
            request.isolation_type.clone(),
        );

        let arc_info = Arc::new(new_info);
        state
            .insert_project(project_id.to_string(), arc_info.clone())
            .map_err(|e| {
                tracing::error!("[STORAGE] insert_project failed: {}", e);
                e
            })?;

        info!(
            "✅ [CHAT] Project info created: project_id={}, container_id={}",
            project_id, container_info.container_id
        );

        Ok(arc_info)
    }
}

/// 自动安装检查：如果 agent_server 携带 platforms，必须同时提供 agent_id、command、version
/// 内置 agent（容器预装）跳过安装逻辑
pub(crate) async fn ensure_chat_agent_installed_if_needed(
    state: &Arc<AppState>,
    request: &AgentChatRequest,
    project_id: &str,
    locale: &'static str,
    service_type: &shared_types::ServiceType,
) -> Result<(), ChatFlowExit> {
    let Some(ref agent_config) = request.agent_config else {
        return Ok(());
    };
    let Some(ref server) = agent_config.agent_server else {
        return Ok(());
    };
    let Some(ref platforms) = server.platforms else {
        return Ok(());
    };

    // agent_id 必填且非空
    let agent_id = match server.agent_id.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(id) => id,
        None => {
            error!("[CHAT] Validation failed: agent_id is required when platforms is provided");
            return Err(ChatFlowExit::Response(HttpResult::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
                "agent_id is required and cannot be empty when platforms is provided",
            )));
        }
    };

    if !shared_types::is_builtin_agent(agent_id) {
        let command = match server.command.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(c) => c,
            None => {
                error!("[CHAT] Validation failed: command is required when platforms is provided");
                return Err(ChatFlowExit::Response(HttpResult::error_with_message(
                    shared_types::error_codes::ERR_VALIDATION,
                    locale,
                    "command is required and cannot be empty when platforms is provided",
                )));
            }
        };
        let version = match server.version.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(v) => v,
            None => {
                error!("[CHAT] Validation failed: version is required when platforms is provided");
                return Err(ChatFlowExit::Response(HttpResult::error_with_message(
                    shared_types::error_codes::ERR_VALIDATION,
                    locale,
                    "version is required and cannot be empty when platforms is provided",
                )));
            }
        };
        let args = server.args.as_deref().unwrap_or(&[]);

        info!(
            "📦 [CHAT] Auto-install: agent_id={}, version={}, args={:?}",
            agent_id, version, args
        );

        let install_req = handler::agent_install_strategy::AgentInstallRequest {
            agent_id,
            command,
            args,
            version,
            platforms,
        };
        handler::agent_install_strategy::ensure_agent_installed(
            state,
            project_id,
            &install_req,
            service_type,
        )
        .await?;
    } else {
        debug!(
            "📦 [CHAT] Builtin agent detected, skipping install: agent_id={}",
            agent_id
        );
    }

    Ok(())
}

/// 🆕 自动查找 session_id 逻辑
/// 如果用户没有传递 session_id，尝试从状态中查找最新的 session_id，
/// 并克隆 request 注入解析后的 session_id 用于转发。
pub(crate) fn resolve_chat_forward_request(
    state: &Arc<AppState>,
    request: &AgentChatRequest,
    project_id: &str,
) -> AgentChatRequest {
    let session_id_to_use = match &request.session_id {
        Some(sid) if !sid.is_empty() => {
            debug!("[CHAT] Using provided session_id: {}", sid);
            sid.clone()
        }
        _ => {
            // 用户没有传递 session_id，尝试查找最新的
            match state.get_project(project_id) {
                Some(project_info) => {
                    let existing_session_id = project_info.session_id();
                    match existing_session_id {
                        Some(sid) if !sid.is_empty() => {
                            info!(
                                "🔄 [CHAT] No session_id provided, auto using latest session: project_id={}, session_id={}",
                                project_id, sid
                            );
                            sid.to_string()
                        }
                        _ => {
                            debug!(
                                "[CHAT] No existing session_id for project, will create new session"
                            );
                            String::new()
                        }
                    }
                }
                None => {
                    debug!("[CHAT] No project exists, will create new session");
                    String::new()
                }
            }
        }
    };

    // 克隆 request 并修改 session_id
    let mut request_for_forward = request.clone();
    request_for_forward.session_id = if session_id_to_use.is_empty() {
        None
    } else {
        Some(session_id_to_use.clone())
    };
    // 🆕 自动查找 session_id 逻辑结束
    request_for_forward
}
