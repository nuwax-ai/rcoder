//! Direct Agent session service.
//!
//! This replaces the historical central worker queue. ACP connections are
//! Send-safe now, so request handlers can call this service directly while each
//! long-lived session still owns its own background connection task.

use std::sync::Arc;

use agent_abstraction::launcher::ModelRuntimeEnvResolver;
use agent_abstraction::session::{AcpAgentWorker, AcpSessionManager, AgentWorker, WorkerRequest};
use agent_client_protocol::schema::v1::SessionId;
use anyhow::Result;
use chrono::Utc;
use dashmap::DashMap;
use shared_types::{AgentMode, ModelProviderConfig, ToolApprovalRule};
use tracing::{debug, error, info, warn};

use crate::model::{AgentStatus, ChatPromptResponse, ProjectAndAgentInfo};
use crate::proxy_agent::SESSION_REQUEST_CONTEXT;
use crate::service::{
    AGENT_REGISTRY, AgentSessionRegistry, LoggingDiagnosticsListener, PERMISSION_MANAGER,
    StateAwareNotifier,
};
use crate::utils::ContentBuilder;

#[derive(Debug)]
pub struct AgentRequest {
    pub prompt_message: agent_abstraction::PromptMessage,
    pub model_provider: Option<ModelProviderConfig>,
    pub service_uuid: Option<String>,
    pub shared_api_key_manager: Option<Arc<DashMap<String, ModelProviderConfig>>>,
}

impl AgentRequest {
    pub fn new(
        prompt_message: agent_abstraction::PromptMessage,
        model_provider: Option<ModelProviderConfig>,
    ) -> Self {
        Self {
            prompt_message,
            model_provider,
            service_uuid: None,
            shared_api_key_manager: None,
        }
    }

    pub fn with_service_uuid(mut self, service_uuid: Option<String>) -> Self {
        self.service_uuid = service_uuid;
        self
    }

    pub fn with_key_manager(
        mut self,
        key_manager: Option<Arc<DashMap<String, ModelProviderConfig>>>,
    ) -> Self {
        self.shared_api_key_manager = key_manager;
        self
    }
}

#[derive(Clone)]
pub struct AgentSessionService {
    worker: AcpAgentWorker<StateAwareNotifier<AgentSessionRegistry>, AgentSessionRegistry>,
}

impl AgentSessionService {
    pub fn new(
        model_env_resolver: Arc<dyn ModelRuntimeEnvResolver>,
        acp_session_create_timeout_secs: u64,
    ) -> Self {
        let session_manager = Arc::new(AcpSessionManager::<
            StateAwareNotifier<AgentSessionRegistry>,
            AgentSessionRegistry,
        >::with_dependencies(
            Arc::new(StateAwareNotifier::new(AGENT_REGISTRY.clone())),
            AGENT_REGISTRY.clone(),
            model_env_resolver,
            PERMISSION_MANAGER.clone(),
            Some(Arc::new(LoggingDiagnosticsListener)),
        ));

        Self {
            worker: AcpAgentWorker::new(session_manager, acp_session_create_timeout_secs),
        }
    }

    pub async fn process_request(&self, request: AgentRequest) -> Result<ChatPromptResponse> {
        let project_id = request.prompt_message.project_id.clone();
        let request_id = request.prompt_message.request_id.clone();

        info!(
            "🔵 [SACP] 开始处理请求 project_id={}, request_id={}",
            project_id, request_id
        );

        // 新 chat 请求进来：清理该 session 上轮残留的 pending permission + 最近审批记录。
        // 旧轮若卡在 permission 等待（如 ext+bash 未审批完），新轮开始时整体 cancel，
        // 避免残留阻塞或 recent 误跟随到新轮。这也替代了 recent 的 TTL 清理（不设 TTL，
        // 改由"新请求清理"保证不泄漏）。
        if let Some(sid) = request.prompt_message.session_id.as_deref()
            && !sid.trim().is_empty()
        {
            let cancelled = PERMISSION_MANAGER.cancel_session_permissions(sid);
            if cancelled > 0 {
                info!(
                    "🧹 [SACP] 新请求清理上轮 pending permission: session_id={}, count={}",
                    sid, cancelled
                );
            }
        }

        let attachment_blocks = if !request.prompt_message.attachments.is_empty() {
            match ContentBuilder::attachments_to_content_blocks(
                &request.prompt_message.attachments,
                &request.prompt_message.project_path,
            )
            .await
            {
                Ok(blocks) => Some(blocks),
                Err(e) => {
                    error!("Attachment processing failed: {:?}", e);
                    return Ok(ChatPromptResponse {
                        project_id,
                        session_id: String::new(),
                        code: shared_types::error_codes::ERR_AGENT_ERROR.to_string(),
                        error: Some(format!(
                            "{}: {:?}",
                            shared_types::error_codes::get_i18n_message_default(
                                "error.attachment_processing_failed"
                            ),
                            e
                        )),
                        request_id: Some(request_id),
                        service_type: request.prompt_message.service_type.clone(),
                    });
                }
            }
        } else {
            None
        };

        let worker_request = WorkerRequest {
            prompt_message: request.prompt_message.clone(),
            model_provider: request.model_provider.clone(),
            attachment_blocks,
            service_uuid: request.service_uuid.clone(),
            shared_api_key_manager: request.shared_api_key_manager.clone(),
        };

        let worker_response = self.worker.process_request(worker_request).await?;

        let session_handles = worker_response.session_handles.clone();
        let is_new_session = worker_response.is_new_session;
        let response_session_id = worker_response.session_id.clone();

        // 注册/更新 per-session 动态权限状态：复用 session 时 PermissionRequestContext 是旧快照，
        // 通过此状态表让本次请求的 agent_mode/tool_approval_rules 在 handler 决策时生效。
        {
            let (mode, rules) = extract_mode_and_rules(&request.prompt_message);
            PERMISSION_MANAGER.upsert_session_state(&response_session_id, mode, rules);
            debug!(
                "Registered session permission state: session_id={}, is_new={}, mode={:?}",
                response_session_id, is_new_session, mode
            );
        }

        if is_new_session {
            if let Some(ref handles) = session_handles {
                debug!("New session, registering in AGENT_REGISTRY");

                let project_and_agent_info = ProjectAndAgentInfo {
                    project_id: project_id.clone(),
                    session_id: SessionId::new(Arc::from(response_session_id.as_str())),
                    prompt_tx: handles.prompt_tx.clone(),
                    cancel_tx: handles.cancel_tx.clone(),
                    model_provider: request.model_provider.clone(),
                    request_id: Some(request_id.clone()),
                    status: AgentStatus::Active,
                    last_activity: Utc::now(),
                    created_at: Utc::now(),
                    stop_handle: handles.lifecycle_handle.clone(),
                    agent_binary_snapshot: None,
                };

                AGENT_REGISTRY.register(&project_id, &response_session_id, project_and_agent_info);

                info!(
                    "🔗 Agent 已注册到 AGENT_REGISTRY: project_id={}, session_id={}",
                    project_id, response_session_id
                );

                // watcher 随进程退出被内核回收（agent_runner 收到 SIGTERM 直接 process::exit，
                // 见 shutdown.rs；PID 1 SIGNAL_UNKILLABLE 故无进程级优雅 token 可接入）。
                spawn_lifecycle_watcher(
                    project_id.clone(),
                    response_session_id.clone(),
                    handles.lifecycle_handle.clone(),
                );
            }
        } else {
            debug!("Reusing session, no new slot needed (Agent already holds slot)");
        }

        SESSION_REQUEST_CONTEXT.insert(project_id.clone(), request_id.clone());

        Ok(ChatPromptResponse {
            project_id: worker_response.project_id,
            session_id: worker_response.session_id,
            code: if worker_response.error.is_none() {
                shared_types::error_codes::SUCCESS.to_string()
            } else {
                shared_types::error_codes::ERR_AGENT_ERROR.to_string()
            },
            error: worker_response.error,
            request_id: worker_response.request_id,
            service_type: worker_response.service_type,
        })
    }
}

fn spawn_lifecycle_watcher(
    project_id: String,
    session_id: String,
    lifecycle_handle: Option<Arc<dyn shared_types::AgentLifecycle>>,
) {
    tokio::spawn(async move {
        // 等待 Agent 生命周期结束（进程级 shutdown 由 SIGTERM→process::exit 直接终止进程，
        // watcher 随进程回收，故不再保留不可达的 shutdown 分支）。
        if let Some(lifecycle) = lifecycle_handle {
            info!(
                "🔄 [SACP] 新会话：等待 Agent 生命周期 - project_id={}, session_id={}",
                project_id, session_id
            );
            lifecycle.cancellation_token().cancelled().await;
            info!(
                "[SACP] Agent lifecycle ended naturally: project_id={}, session_id={}",
                project_id, session_id
            );
        } else {
            warn!(
                "⚠️ [SACP] 新会话缺少 lifecycle_handle - project_id={}",
                project_id
            );
        }

        // Agent 子进程生命周期结束时，清理该 session 残留的 pending permission。
        // 防止 Agent 在 await permission responder 时异常退出，导致 pending 永远无法被 consume 而泄漏。
        let cancelled_permissions = PERMISSION_MANAGER.cancel_session_permissions(&session_id);
        if cancelled_permissions > 0 {
            warn!(
                "🧹 [SACP] lifecycle ended, cleared {} leftover pending permission(s): project_id={}, session_id={}",
                cancelled_permissions, project_id, session_id
            );
        }
        // 同时清理该 session 的动态权限状态（防止 session_state 泄漏）。
        PERMISSION_MANAGER.clear_session_state(&session_id);

        AGENT_REGISTRY.remove_by_project_if_session_matches(&project_id, &session_id);
        info!(
            "🛑 [SACP] Agent 生命周期结束，已清理 Registry - project_id={}, session_id={}",
            project_id, session_id
        );
    });
}

/// 从请求的 prompt_message 提取 effective agent_mode / tool_approval_rules。
/// 逻辑与 acp_worker.rs 构造 start_config 时一致（agent_config_override.agent_server）。
/// 缺失时 fallback (Yolo, None)，与 PermissionRequestContext 默认行为对齐。
fn extract_mode_and_rules(
    prompt_message: &agent_abstraction::PromptMessage,
) -> (AgentMode, Option<Vec<ToolApprovalRule>>) {
    if let Some(ref override_cfg) = prompt_message.agent_config_override
        && let Some(ref agent_server) = override_cfg.agent_server
    {
        let mode = agent_server.agent_mode().unwrap_or(AgentMode::Yolo);
        let rules = agent_server.tool_approval_rules.clone();
        (mode, rules)
    } else {
        (AgentMode::Yolo, None)
    }
}
