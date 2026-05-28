//! Direct Agent session service.
//!
//! This replaces the historical central worker queue. ACP connections are
//! Send-safe now, so request handlers can call this service directly while each
//! long-lived session still owns its own background connection task.

use std::sync::Arc;

use agent_abstraction::launcher::ModelRuntimeEnvResolver;
use agent_abstraction::session::{AcpAgentWorker, AcpSessionManager, AgentWorker, WorkerRequest};
use agent_client_protocol::schema::SessionId;
use anyhow::Result;
use chrono::Utc;
use dashmap::DashMap;
use shared_types::ModelProviderConfig;
use tracing::{debug, error, info, warn};

use crate::model::{AgentStatus, ChatPromptResponse, ProjectAndAgentInfo};
use crate::proxy_agent::SESSION_REQUEST_CONTEXT;
use crate::service::{
    AGENT_REGISTRY, AgentSessionRegistry, PERMISSION_MANAGER, StateAwareNotifier,
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
    pub fn new(model_env_resolver: Arc<dyn ModelRuntimeEnvResolver>) -> Self {
        let session_manager = Arc::new(
            AcpSessionManager::<StateAwareNotifier<AgentSessionRegistry>, AgentSessionRegistry>::with_dependencies(
                Arc::new(StateAwareNotifier::new(AGENT_REGISTRY.clone())),
                AGENT_REGISTRY.clone(),
                model_env_resolver,
                PERMISSION_MANAGER.clone(),
            ),
        );

        Self {
            worker: AcpAgentWorker::new(session_manager),
        }
    }

    pub async fn process_request(&self, request: AgentRequest) -> Result<ChatPromptResponse> {
        let project_id = request.prompt_message.project_id.clone();
        let request_id = request.prompt_message.request_id.clone();

        info!(
            "🔵 [SACP] 开始处理请求 project_id={}, request_id={}",
            project_id, request_id
        );

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

        if is_new_session {
            if let Some(ref handles) = session_handles {
                debug!("🆕 New session, registering in AGENT_REGISTRY");

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

                spawn_lifecycle_watcher(
                    project_id.clone(),
                    response_session_id.clone(),
                    handles.lifecycle_handle.clone(),
                );
            }
        } else {
            debug!("♻️ Reusing session, no new slot needed (Agent already holds slot)");
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
        if let Some(lifecycle) = lifecycle_handle {
            info!(
                "🔄 [SACP] 新会话：等待 Agent 生命周期 - project_id={}, session_id={}",
                project_id, session_id
            );
            lifecycle.cancellation_token().cancelled().await;
        } else {
            warn!(
                "⚠️ [SACP] 新会话缺少 lifecycle_handle - project_id={}",
                project_id
            );
        }

        AGENT_REGISTRY.remove_by_project_if_session_matches(&project_id, &session_id);
        info!(
            "🛑 [SACP] Agent 生命周期结束，已清理 Registry - project_id={}, session_id={}",
            project_id, session_id
        );
    });
}
