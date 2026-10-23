//! Direct Agent session service.
//!
//! This replaces the historical central worker queue. ACP connections are
//! Send-safe now, so request handlers can call this service directly while each
//! long-lived session still owns its own background connection task.

use std::sync::Arc;

use agent_abstraction::launcher::ModelRuntimeEnvResolver;
use agent_abstraction::session::{
    AcpAgentWorker, AcpSessionManager, AgentWorker, WorkerRequest, WorkerResponse,
};
use anyhow::Result;
use dashmap::DashMap;
use shared_types::{AgentMode, ModelProviderConfig, ToolApprovalRule};
use tracing::{debug, error, info, warn};

use crate::model::{AgentStatus, ChatPromptResponse};
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
    /// 存活代理进程的 project_id → service_uuid 绑定。
    pub project_uuid_map: Option<Arc<DashMap<String, String>>>,
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
            project_uuid_map: None,
        }
    }

    pub fn with_service_uuid(mut self, service_uuid: Option<String>) -> Self {
        self.service_uuid = service_uuid;
        self
    }

    pub fn with_project_uuid_map(mut self, map: Arc<DashMap<String, String>>) -> Self {
        self.project_uuid_map = Some(map);
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
            && AGENT_REGISTRY.get_project_by_session(sid).as_deref() == Some(project_id.as_str())
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
                        service_type: request.prompt_message.service_type,
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
            project_uuid_map: request.project_uuid_map.clone(),
        };

        let worker_response = self.worker.process_request(worker_request).await?;

        let session_handles = worker_response.session_handles.clone();
        let is_new_session = worker_response.is_new_session;
        let response_session_id = worker_response.session_id.clone();

        if is_new_session {
            // SessionManager already committed the physical execution. A late
            // worker reply can supplement that exact entry, never register it
            // again after Stop or replace a newer process loading the same SID.
            let current = record_created_completion(
                &AGENT_REGISTRY,
                &SESSION_REQUEST_CONTEXT,
                &request,
                &worker_response,
            );
            if !current {
                debug!(
                    project_id,
                    session_id = response_session_id,
                    request_id,
                    "Worker reply no longer owns current execution metadata"
                );
            }
            if let Some(ref handles) = session_handles {
                // watcher 随进程退出被内核回收（agent_runner 收到 SIGTERM 直接 process::exit，
                // 见 shutdown.rs；PID 1 SIGNAL_UNKILLABLE 故无进程级优雅 token 可接入）。
                spawn_lifecycle_watcher(
                    project_id.clone(),
                    response_session_id.clone(),
                    handles.lifecycle_handle.clone(),
                    handles.cancel_tx.clone(),
                );
            }
        } else {
            // Existing-session responses retain their original contract. Their
            // WorkerResponse does not include the exact execution channels.
            let (mode, rules) = extract_mode_and_rules(&request.prompt_message);
            PERMISSION_MANAGER.upsert_session_state(&response_session_id, mode, rules);
            SESSION_REQUEST_CONTEXT.insert(project_id.clone(), request_id.clone());
            debug!("Reusing session, no new slot needed (Agent already holds slot)");
        }

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

/// Publish only metadata for the exact already-committed creation. Holding the
/// current entry through these synchronous writes also fences a later context
/// or permission update; no registry entry, process, or SID mapping is created.
fn record_created_completion(
    registry: &AgentSessionRegistry,
    contexts: &DashMap<String, String>,
    request: &AgentRequest,
    response: &WorkerResponse,
) -> bool {
    let Some(handles) = response.session_handles.as_ref() else {
        return false;
    };
    if shared_types::SessionHint::classify(Some(&response.session_id))
        .as_session_id()
        .is_none()
    {
        return false;
    }
    let project_id = &request.prompt_message.project_id;
    let request_id = &request.prompt_message.request_id;
    let (mode, rules) = extract_mode_and_rules(&request.prompt_message);
    registry.try_update_agent_info(project_id, |info| {
        if info.session_id.0.as_ref() != response.session_id.as_str()
            || !info.cancel_tx.same_channel(&handles.cancel_tx)
            || info.status == AgentStatus::Terminating
            || info
                .request_id
                .as_deref()
                .is_some_and(|current| current != request_id.as_str())
        {
            return false;
        }
        // PromptStart/End may already have advanced this round. Preserve its
        // actual status, creation time and lifecycle, including an Idle result.
        if info.request_id.is_none() {
            info.request_id = Some(request_id.clone());
        }
        PERMISSION_MANAGER.upsert_session_state(&response.session_id, mode, rules.clone());
        contexts.insert(project_id.clone(), request_id.clone());
        true
    })
}

fn spawn_lifecycle_watcher(
    project_id: String,
    session_id: String,
    lifecycle_handle: Option<Arc<dyn shared_types::AgentLifecycle>>,
    cancel_tx: tokio::sync::mpsc::Sender<shared_types::CancelNotificationRequestWrapper>,
) {
    tokio::spawn(async move {
        // 等待 Agent 生命周期结束（进程级 shutdown 由 SIGTERM→process::exit 直接终止进程，
        // watcher 随进程回收，故不再保留不可达的 shutdown 分支）。
        if let Some(lifecycle) = lifecycle_handle {
            info!(
                "🔄 [SACP] 新会话：等待 Agent 生命周期 - project_id={}, session_id={}",
                project_id, session_id
            );
            if let Err(error) = lifecycle.wait_for_exit().await {
                error!(project_id, session_id, %error, "agent exit is unconfirmed; retaining registry identity");
                return;
            }
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

        let _transition = agent_abstraction::session::lock_execution_transition(&project_id).await;
        // Agent 子进程生命周期结束时，清理该 session 残留的 pending permission。
        // 防止 Agent 在 await permission responder 时异常退出，导致 pending 永远无法被 consume 而泄漏。
        if AGENT_REGISTRY
            .view_agent_info(&project_id, |info| {
                info.session_id.to_string() == session_id
                    && !info.cancel_tx.same_channel(&cancel_tx)
            })
            .unwrap_or(false)
        {
            return;
        }
        if AGENT_REGISTRY
            .get_project_by_session(&session_id)
            .as_deref()
            == Some(project_id.as_str())
        {
            let cancelled_permissions = PERMISSION_MANAGER.cancel_session_permissions(&session_id);
            if cancelled_permissions > 0 {
                warn!(
                    "🧹 [SACP] lifecycle ended, cleared {} leftover pending permission(s): project_id={}, session_id={}",
                    cancelled_permissions, project_id, session_id
                );
            }
            // 同时清理该 session 的动态权限状态（防止 session_state 泄漏）。
            PERMISSION_MANAGER.clear_session_state(&session_id);
        }

        AGENT_REGISTRY.remove_by_project_if_session_and_channel_matches(
            &project_id,
            &session_id,
            &cancel_tx,
        );
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

#[cfg(test)]
mod completion_tests {
    use super::*;
    use agent_abstraction::session::SessionHandles;
    use agent_client_protocol::schema::v1::{PromptRequest, SessionId};
    use chrono::Utc;
    use shared_types::{ChatPromptBuilder, ProjectAndAgentInfo, ServiceType};
    use tokio::sync::mpsc;

    struct Execution {
        info: ProjectAndAgentInfo,
        _prompt: mpsc::Receiver<PromptRequest>,
        _cancel: mpsc::Receiver<shared_types::CancelNotificationRequestWrapper>,
    }

    impl Execution {
        fn new(project: &str, sid: &str, request_id: Option<&str>, status: AgentStatus) -> Self {
            let (prompt_tx, prompt) = mpsc::channel(1);
            let (cancel_tx, cancel) = mpsc::channel(1);
            Self {
                info: ProjectAndAgentInfo {
                    project_id: project.into(),
                    session_id: SessionId::new(Arc::from(sid)),
                    prompt_tx,
                    cancel_tx,
                    model_provider: None,
                    request_id: request_id.map(str::to_owned),
                    status,
                    last_activity: Utc::now(),
                    created_at: Utc::now() - chrono::Duration::hours(1),
                    stop_handle: None,
                    agent_binary_snapshot: None,
                },
                _prompt: prompt,
                _cancel: cancel,
            }
        }

        fn reply(&self, request_id: &str) -> WorkerResponse {
            WorkerResponse::new_session_success(
                self.info.project_id.clone(),
                self.info.session_id.to_string(),
                Some(request_id.to_owned()),
                ServiceType::ComputerAgentRunner,
                SessionHandles {
                    prompt_tx: self.info.prompt_tx.clone(),
                    cancel_tx: self.info.cancel_tx.clone(),
                    lifecycle_handle: self.info.stop_handle.clone(),
                },
            )
        }
    }

    fn identities() -> (String, String) {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        (format!("completion-{suffix}"), format!("opaque-{suffix}"))
    }

    fn request(project: &str, request_id: &str) -> AgentRequest {
        let prompt = ChatPromptBuilder::default()
            .project_id(project.to_owned())
            .project_path(std::path::PathBuf::from("completion-fixture"))
            .session_id(None)
            .prompt("one explicit request".to_owned())
            .request_id(request_id.to_owned())
            .service_type(ServiceType::ComputerAgentRunner)
            .build()
            .unwrap();
        AgentRequest::new(prompt.into(), None)
    }

    #[test]
    fn late_new_worker_completion_cannot_replace_same_sid_with_another_channel() {
        let registry = AgentSessionRegistry::new();
        let contexts = DashMap::new();
        let (project, sid) = identities();
        let old = Execution::new(&project, &sid, Some("old-round"), AgentStatus::Active);
        registry.register(&project, &sid, old.info.clone());
        let reply = old.reply("old-round");
        let replacement = Execution::new(&project, &sid, Some("new-round"), AgentStatus::Idle);
        registry.register(&project, &sid, replacement.info.clone());
        contexts.insert(project.clone(), "new-round".to_owned());

        assert!(!record_created_completion(
            &registry,
            &contexts,
            &request(&project, "old-round"),
            &reply,
        ));
        let current = registry.get_agent_info(&project).unwrap();
        assert!(current.cancel_tx.same_channel(&replacement.info.cancel_tx));
        assert_eq!(current.request_id.as_deref(), Some("new-round"));
        assert_eq!(current.status, AgentStatus::Idle);
        assert_eq!(current.created_at, replacement.info.created_at);
        assert_eq!(contexts.get(&project).unwrap().as_str(), "new-round");
        assert_eq!(
            registry.get_project_by_session(&sid).as_deref(),
            Some(project.as_str())
        );
    }

    #[test]
    fn late_new_worker_completion_cannot_resurrect_a_stopped_project() {
        let registry = AgentSessionRegistry::new();
        let contexts = DashMap::new();
        let (project, sid) = identities();
        let old = Execution::new(&project, &sid, Some("old-round"), AgentStatus::Active);
        registry.register(&project, &sid, old.info.clone());
        let reply = old.reply("old-round");
        registry.remove_by_project(&project);

        assert!(!record_created_completion(
            &registry,
            &contexts,
            &request(&project, "old-round"),
            &reply,
        ));
        assert!(registry.get_agent_info(&project).is_none());
        assert!(registry.get_project_by_session(&sid).is_none());
        assert!(!contexts.contains_key(&project));
    }

    #[test]
    fn current_completed_round_keeps_idle_status_and_original_creation_time() {
        let registry = AgentSessionRegistry::new();
        let contexts = DashMap::new();
        let (project, sid) = identities();
        let execution = Execution::new(&project, &sid, Some("round"), AgentStatus::Idle);
        registry.register(&project, &sid, execution.info.clone());

        assert!(record_created_completion(
            &registry,
            &contexts,
            &request(&project, "round"),
            &execution.reply("round"),
        ));
        let current = registry.get_agent_info(&project).unwrap();
        assert_eq!(current.status, AgentStatus::Idle);
        assert_eq!(current.created_at, execution.info.created_at);
        assert_eq!(current.last_activity, execution.info.last_activity);
        assert!(current.cancel_tx.same_channel(&execution.info.cancel_tx));
        assert_eq!(contexts.get(&project).unwrap().as_str(), "round");
    }

    #[test]
    fn late_new_worker_completion_cannot_overwrite_a_newer_round_on_the_same_channel() {
        let registry = AgentSessionRegistry::new();
        let contexts = DashMap::new();
        let (project, sid) = identities();
        let old = Execution::new(&project, &sid, Some("old-round"), AgentStatus::Active);
        let reply = old.reply("old-round");
        let mut newer = old.info.clone();
        newer.request_id = Some("new-round".into());
        registry.register(&project, &sid, newer.clone());
        contexts.insert(project.clone(), "new-round".to_owned());

        assert!(!record_created_completion(
            &registry,
            &contexts,
            &request(&project, "old-round"),
            &reply,
        ));
        let current = registry.get_agent_info(&project).unwrap();
        assert_eq!(current.request_id.as_deref(), Some("new-round"));
        assert_eq!(current.created_at, newer.created_at);
        assert_eq!(contexts.get(&project).unwrap().as_str(), "new-round");
    }
}
