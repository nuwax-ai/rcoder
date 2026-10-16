//! Computer Chat 转发前后阶段
//!
//! 从 `handle_computer_chat_internal` 抽出：Agent 自动安装检查、VNC 后端注册、
//! Agent 状态探活、session_id 自动解析、响应后会话映射更新。

use shared_types::{
    ChatResponse, ComputerChatRequest, ProjectAndContainerInfo,
    persistence::PersistenceWriteOutcome,
};
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
            return Err(ChatFlowExit::response(HttpResult::error_with_message(
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
                return Err(ChatFlowExit::response(HttpResult::error_with_message(
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
                return Err(ChatFlowExit::response(HttpResult::error_with_message(
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
        )
        .map_err(|error| error.to_string())?;

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
    request: &ComputerChatRequest,
    captured: &ProjectAndContainerInfo,
) -> ComputerChatRequest {
    let mut forwarded = request.clone();
    if forwarded
        .session_id
        .as_ref()
        .is_none_or(|sid| sid.is_empty())
    {
        forwarded.session_id = captured
            .session_id()
            .map(str::to_owned)
            .filter(|sid| !sid.is_empty());
    }
    forwarded
}

/// Capture both the registration generation and a detached physical snapshot.
/// Cloning the project alone would retain its mutable shared ContainerEntry.
pub(super) fn capture_dispatch_project(
    registered: &ProjectAndContainerInfo,
) -> Result<ProjectAndContainerInfo, AppError> {
    let basic = registered.container_info().ok_or_else(|| {
        AppError::internal_server_error("Registered Computer container is missing before dispatch")
    })?;
    let mut captured = registered.clone();
    captured.set_container(Some(basic));
    Ok(captured)
}

/// Append only this accepted session against the captured execution identity.
/// Completing an older RPC must not overwrite a later request's model/metadata.
#[instrument(skip_all, fields(project_id = %captured.project_id()))]
pub(super) async fn update_session_mappings_after_response(
    state: &Arc<AppState>,
    result: &mut HttpResult<ChatResponse>,
    captured: &ProjectAndContainerInfo,
    locale: &'static str,
) {
    let Some(sid) = result
        .data
        .as_ref()
        .map(|response| response.session_id.clone())
        .filter(|sid| !sid.is_empty())
    else {
        return;
    };
    let outcome = state
        .append_session_for_container_durable(captured, &sid)
        .await;
    complete_session_registration(
        result,
        captured,
        &sid,
        outcome,
        state.is_session_registration_committed(captured, &sid),
        locale,
    )
    .await;
}

async fn complete_session_registration(
    result: &mut HttpResult<ChatResponse>,
    captured: &ProjectAndContainerInfo,
    session_id: &str,
    outcome: anyhow::Result<PersistenceWriteOutcome>,
    readback: impl Future<Output = anyhow::Result<bool>>,
    locale: &'static str,
) {
    let (code, diagnostic) = match outcome {
        Ok(PersistenceWriteOutcome::Committed) => {
            info!(project_id = %captured.project_id(), session_id,
                "[COMPUTER_CHAT] Session registration committed");
            return;
        }
        Ok(PersistenceWriteOutcome::Deferred { reason }) => {
            // The store verifies the original registration against its committed
            // source (PG primary), never an optimistic local pending mirror.
            // No prompt replay, sleeps or new timeout budget.
            match readback.await {
                Ok(true) => {
                    info!(project_id = %captured.project_id(), session_id,
                        "[COMPUTER_CHAT] Session registration confirmed by readback");
                    return;
                }
                Ok(false) => (
                    shared_types::ERR_SERVICE_UNAVAILABLE,
                    format!("Session registration remains deferred: {reason}"),
                ),
                Err(error) => (
                    shared_types::ERR_SERVICE_UNAVAILABLE,
                    format!("Session registration deferred: {reason}; readback failed: {error:#}"),
                ),
            }
        }
        Ok(PersistenceWriteOutcome::Superseded) => (
            shared_types::ERR_CONFLICT,
            "The captured execution identity was replaced before session registration".to_owned(),
        ),
        Err(error) => (
            shared_types::ERR_SERVICE_UNAVAILABLE,
            format!("Session registration failed after task acceptance: {error:#}"),
        ),
    };
    error!(project_id = %captured.project_id(), session_id, %code,
        detail = %shared_types::sanitize_error_text(&diagnostic),
        "[COMPUTER_CHAT] Accepted task session registration unresolved; prompt must not be replayed");
    let hint = if locale.starts_with("zh") {
        "任务已受理，请仅使用现有 session_id 重连进度，不要重复发送原消息。"
    } else {
        "The task was accepted. Reconnect progress with its existing session_id; do not resend this prompt."
    };
    // Preserve a genuine runner business failure. Registration diagnostics may
    // supplement it, but cannot turn it into a successful chat response.
    if result.is_success() {
        result.code = code.to_owned();
        result.success = false;
        result.message = shared_types::sanitize_error_text(&diagnostic);
        result.error_detail = Some(
            shared_types::ErrorDetail::new(code, "chat.session_registration", diagnostic)
                .with_task_id(session_id)
                .with_retryable(false)
                .with_hint(hint)
                .localized(locale),
        );
    } else if result.error_detail.is_none() {
        result.error_detail = Some(
            shared_types::ErrorDetail::new(code, "chat.session_registration", diagnostic)
                .with_task_id(session_id)
                .with_retryable(false)
                .with_hint(hint)
                .localized(locale),
        );
    }
}

#[cfg(test)]
mod tests;
