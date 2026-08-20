//! Computer Chat 转发前后阶段
//!
//! 从 `handle_computer_chat_internal` 抽出：Agent 自动安装检查、VNC 后端注册、
//! Agent 状态探活、session_id 自动解析、响应后会话映射更新。

use shared_types::{ChatResponse, ComputerChatRequest};
use std::sync::Arc;
use tracing::{debug, error, info, instrument, warn};

use crate::{AppError, HttpResult, router::AppState};
use docker_manager::ContainerBasicInfo;

use super::super::chat_forward::ChatFlowExit;

/// 自动安装检查：如果 agent_server 携带 platforms，必须同时提供 agent_id、command、version
/// 内置 agent（容器预装）跳过安装逻辑
#[instrument(skip_all, fields(project_id = %project_id))]
pub(super) async fn ensure_agent_installed_if_needed(
    state: &Arc<AppState>,
    request: &ComputerChatRequest,
    project_id: &str,
    locale: &'static str,
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
            error!(
                "[COMPUTER_CHAT] Validation failed: agent_id is required when platforms is provided"
            );
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
                error!(
                    "[COMPUTER_CHAT] Validation failed: command is required when platforms is provided"
                );
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
                error!(
                    "[COMPUTER_CHAT] Validation failed: version is required when platforms is provided"
                );
                return Err(ChatFlowExit::Response(HttpResult::error_with_message(
                    shared_types::error_codes::ERR_VALIDATION,
                    locale,
                    "version is required and cannot be empty when platforms is provided",
                )));
            }
        };
        let args = server.args.as_deref().unwrap_or(&[]);

        info!(
            "📦 [COMPUTER_CHAT] Auto-install: agent_id={}, version={}, args={:?}",
            agent_id, version, args
        );

        let install_req = crate::handler::agent_install_strategy::AgentInstallRequest {
            agent_id,
            command,
            args,
            version,
            platforms,
        };
        crate::handler::agent_install_strategy::ensure_agent_installed(
            state,
            project_id,
            &install_req,
            &shared_types::ServiceType::ComputerAgentRunner,
        )
        .await?;
    } else {
        debug!(
            "📦 [COMPUTER_CHAT] Builtin agent detected, skipping install: agent_id={}",
            agent_id
        );
    }

    Ok(())
}

/// 注册 VNC 后端到 Pingora（用于 WebSocket 代理）
pub(super) fn register_vnc_backend(
    state: &Arc<AppState>,
    user_id: &str,
    container_info: &ContainerBasicInfo,
) {
    if let Some(ref pingora_service) = state.pingora_service {
        // K8s 用 Service FQDN，Docker 用容器 IP（统一走 shared_types 分发）
        let backend_addr = shared_types::build_backend_addr(
            &container_info.container_name,
            &container_info.container_ip,
            &state.config.app_manager.namespace,
            &state.cluster_domain,
        );
        pingora_service.add_vnc_backend(user_id, &backend_addr);
        debug!(
            "🔗 [COMPUTER_CHAT] VNC backend registered: user_id={} -> {}",
            user_id, backend_addr
        );
    }
}

/// 🆕 主动查询 Agent 状态 (User Request)
/// 在转发请求前，主动查询 Agent 状态，确保状态是最新的。
/// 这有助于在容器重启后，确认 Agent 是否真正处于空闲状态。
#[instrument(skip_all, fields(project_id = %project_id))]
pub(super) async fn probe_agent_status(
    state: &Arc<AppState>,
    container_info: &ContainerBasicInfo,
    project_id: &str,
    locale: &'static str,
) {
    // 根据运行环境选择 gRPC 地址
    let grpc_addr_result = async {
        // K8s 用 Service FQDN，Docker 用容器 IP（统一走 shared_types 分发）
        let addr = shared_types::build_grpc_addr(
            &container_info.container_name,
            &container_info.container_ip,
            &state.config.app_manager.namespace,
            &state.cluster_domain,
        );

        Ok::<_, String>(addr)
    }
    .await;

    if let Ok(grpc_addr) = grpc_addr_result {
        debug!("[COMPUTER_CHAT] Checking Agent status: {}", grpc_addr);
        if let Ok(mut client) = state.grpc_pool.get_client(&grpc_addr).await {
            let status_req = shared_types::grpc::GetStatusRequest {
                project_id: project_id.to_string(),
                session_id: "".to_string(), // 我们只关心 project 级别的状态
            };

            let mut grpc_request = crate::grpc::new_request_with_locale(status_req, locale);
            grpc_request.set_timeout(std::time::Duration::from_secs(5));

            match client.get_status(grpc_request).await {
                Ok(resp) => {
                    let status = resp.into_inner().status;
                    debug!(
                        "📊 [COMPUTER_CHAT] Agent current status: project_id={}, status={}",
                        project_id, status
                    );
                    // 如果状态是 idle，我们可以更有信心地继续
                }
                Err(e) => {
                    warn!("[COMPUTER_CHAT] Failed to get Agent status: {}", e);
                    // Query failed不阻止请求继续，可能是网络波动，让后续的 Chat 请求去处理
                }
            }
        }
    }
}

/// 🆕 自动查找 session_id 逻辑
/// 如果用户没有传递 session_id，尝试从状态中查找最新的 session_id，
/// 并克隆 request 注入解析后的 session_id 用于转发。
pub(super) fn resolve_forward_request(
    state: &Arc<AppState>,
    request: &ComputerChatRequest,
    project_id: &str,
) -> ComputerChatRequest {
    let session_id_to_use = match &request.session_id {
        Some(sid) if !sid.is_empty() => {
            debug!("[COMPUTER_CHAT] Using session_id: {}", sid);
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
                                "🔄 [COMPUTER_CHAT] No session_id provided, auto using latest session: project_id={}, session_id={}",
                                project_id, sid
                            );
                            sid.to_string()
                        }
                        _ => {
                            debug!("[COMPUTER_CHAT] Project exists, creating new session");
                            String::new()
                        }
                    }
                }
                None => {
                    debug!("[COMPUTER_CHAT] No project, creating new session");
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

/// 更新会话映射（填充所有三个映射表，保持一致性）
/// 无论请求成功还是失败，只要响应中包含 session_id，都要更新映射
/// 这样用户可以通过 SSE 接口获取错误通知，而不会收到 SESSION_EXPIRED 错误
#[instrument(skip_all, fields(user_id = %user_id, project_id = %project_id))]
pub(super) async fn update_session_mappings_after_response(
    state: &Arc<AppState>,
    result: &HttpResult<ChatResponse>,
    user_id: &str,
    project_id: &str,
    container_info: &ContainerBasicInfo,
    request: &ComputerChatRequest,
) -> Result<(), AppError> {
    let Some(chat_response) = &result.data else {
        return Ok(());
    };

    let session_id = chat_response.session_id.clone();

    // 只有当 session_id 非空时才更新映射
    if session_id.is_empty() {
        return Ok(());
    }

    info!(
        "🔗 [COMPUTER_CHAT] Associated session: session_id={} -> user_id={}, project_id={}, success={}",
        session_id,
        user_id,
        project_id,
        result.is_success()
    );

    // 从 Runtime API 获取最新容器信息，避免使用过期 IP
    let container_info = match state
        .runtime()
        .get_container_info_by_identifier(user_id, &shared_types::ServiceType::ComputerAgentRunner)
        .await
    {
        Ok(Some(info)) => {
            info!(
                "🔄 [COMPUTER_CHAT] Getting latest container info from Runtime API: user_id={}, container_id={}, container_ip={}",
                user_id, info.container_id, info.container_ip
            );
            info
        }
        Ok(None) => {
            warn!(
                "⚠️ [COMPUTER_CHAT] Container not found in runtime: user_id={}, using cached container info",
                user_id
            );
            // 使用之前获取的容器信息
            container_info.clone()
        }
        Err(e) => {
            warn!(
                "⚠️ [COMPUTER_CHAT] Failed to get container info from runtime: user_id={}, error={}, using cached container info",
                user_id, e
            );
            // 使用之前获取的容器信息
            container_info.clone()
        }
    };

    // ComputerAgentRunner 模式：每个 project 独立记录
    // 使用真正的 project_id 作为 map_key，user_id 存储在数据字段中
    let map_key = project_id.to_string();

    // 检查是否已存在该 project_id 的记录
    if let Some(existing_info) = state.get_project(&map_key) {
        // 已存在：更新信息
        let mut updated_info = (*existing_info).clone();

        // 更新活动时间
        updated_info.update_activity();
        // 添加 session（多 session 模型，不清除其他 session）
        updated_info.add_session(session_id.clone());

        // 更新扩展信息
        updated_info.update_extended_from_request(
            Some(container_info.clone()),
            request.model_provider.clone(),
            request.request_id.clone(),
            Some(shared_types::ServiceType::ComputerAgentRunner),
        );

        // 单次原子写入（项目元数据 + session 映射），消除 CAS 竞态。
        // durable：PG 事务提交完成才返回——session_id 到达前端时任何副本可服务
        state
            .insert_project_with_session_durable(
                map_key.clone(),
                Arc::new(updated_info),
                &session_id,
            )
            .await
            .map_err(|e| {
                tracing::error!(
                    "[STORAGE] insert_project_with_session_durable failed: {}",
                    e
                );
                e
            })?;

        info!(
            "🔄 [COMPUTER_CHAT] Updated existing container mapping: user_id={}, project_id={}, session_id={} (last_activity refreshed)",
            user_id, project_id, session_id
        );
    } else {
        // 不存在：创建新的 ProjectAndContainerInfo
        let mut project_info = shared_types::ProjectAndContainerInfo::new(map_key.clone());

        // 设置 user_id（ComputerAgentRunner 模式）
        project_info.set_user_id(Some(user_id.to_string()));
        // 设置 pod_id（共享容器模式）
        project_info.set_pod_id(request.pod_id.clone());
        // 添加 session（多 session 模型）
        project_info.add_session(session_id.clone());

        // 更新扩展信息（容器、模型配置等）
        project_info.update_extended_from_request(
            Some(container_info.clone()),
            request.model_provider.clone(),
            request.request_id.clone(),
            Some(shared_types::ServiceType::ComputerAgentRunner),
        );
        project_info.set_scope(
            request.tenant_id.clone(),
            request.space_id.clone(),
            request.isolation_type.clone(),
        );

        // 单次原子写入（项目元数据 + session 映射），消除 CAS 竞态。
        // durable：PG 事务提交完成才返回——session_id 到达前端时任何副本可服务
        state
            .insert_project_with_session_durable(
                map_key.clone(),
                Arc::new(project_info),
                &session_id,
            )
            .await
            .map_err(|e| {
                tracing::error!(
                    "[STORAGE] insert_project_with_session_durable failed: {}",
                    e
                );
                e
            })?;

        info!(
            "🆕 [COMPUTER_CHAT] Created new container mapping: user_id={}, project_id={}, session_id={}",
            user_id, project_id, session_id
        );
    }

    if result.is_success() {
        info!(
            "✅ [COMPUTER_CHAT] Request processed: user_id={}, project_id={}, session_id={} (all mappings updated)",
            user_id, project_id, session_id
        );
    } else {
        warn!(
            "⚠️ [COMPUTER_CHAT] Request failed but session mapping saved: user_id={}, project_id={}, session_id={}, code={}, message={}",
            user_id, project_id, session_id, result.code, result.message
        );
    }

    Ok(())
}
