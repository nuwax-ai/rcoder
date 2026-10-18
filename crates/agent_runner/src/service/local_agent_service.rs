//! Agent Runner 本地服务实现
//!
//! 实现 `AgentHttpService` trait，直接调用本地服务（AGENT_REGISTRY, SESSION_CACHE, handle_chat_core）
//! 适用于 Agent Runner 的 HTTP Server 模式

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::v1::{CancelNotification, SessionId};
use async_trait::async_trait;
use dashmap::DashMap;
use shared_types::{
    AgentHttpService, AgentStatusResponse, ChatResponse, HttpResult, RcoderAgentCancelRequest,
    RcoderAgentCancelResponse, RcoderAgentStopRequest, RcoderAgentStopResponse, RcoderChatRequest,
    ServiceType, SessionHint,
};
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::grpc::remove_agent_and_cleanup;
use crate::service::{
    AGENT_REGISTRY, AgentSessionService, SESSION_CACHE,
    chat_handler::{ChatHandlerContext, ChatHandlerInput},
    get_or_create_session_data, handle_chat_core,
};

/// Agent Runner 本地服务实现
///
/// 直接调用本地 AGENT_REGISTRY、SESSION_CACHE、handle_chat_core()
/// 不需要 gRPC 转发
pub struct LocalAgentHttpService {
    /// Agent 会话服务
    pub agent_session_service: Arc<AgentSessionService>,
    /// 共享 API Key 管理器
    pub shared_api_key_manager: Arc<DashMap<String, shared_types::ModelProviderConfig>>,
    /// project_id -> UUID 映射
    pub project_uuid_map: Arc<DashMap<String, String>>,
    /// 项目工作目录根路径
    pub projects_dir: PathBuf,
}

impl LocalAgentHttpService {
    /// 创建新的 LocalAgentHttpService
    pub fn new(
        agent_session_service: Arc<AgentSessionService>,
        shared_api_key_manager: Arc<DashMap<String, shared_types::ModelProviderConfig>>,
        project_uuid_map: Arc<DashMap<String, String>>,
        projects_dir: PathBuf,
    ) -> Self {
        Self {
            agent_session_service,
            shared_api_key_manager,
            project_uuid_map,
            projects_dir,
        }
    }

    /// Caller holds the project's execution transition lock. Session routing is
    /// optional; a real child must exit before its physical entry is removed.
    async fn retire_execution(
        project_id: &str,
        target: &shared_types::ProjectAndAgentInfo,
    ) -> anyhow::Result<()> {
        let session_id = target.session_id.0.as_ref();
        let captured_cache = SessionHint::classify(Some(session_id))
            .as_session_id()
            .filter(|sid| AGENT_REGISTRY.get_project_by_session(sid).as_deref() == Some(project_id))
            .and_then(|sid| SESSION_CACHE.view(sid, |_, data| data.clone()));
        if !remove_agent_and_cleanup(project_id, target).await? {
            anyhow::bail!("Project execution changed before stop completed");
        }
        // Clear only the captured old cache after exit, never a replacement
        // cache and never a shared historical placeholder key.
        if let Some(data) = captured_cache {
            data.clear_message_buffer().await;
        }
        Ok(())
    }
}

#[async_trait]
impl AgentHttpService for LocalAgentHttpService {
    /// Chat 对话请求
    async fn chat(&self, request: RcoderChatRequest) -> HttpResult<ChatResponse> {
        // 0. 验证 prompt 不为空
        if request.prompt.is_empty() {
            warn!("[LocalAgent] Empty prompt received");
            return HttpResult::error(
                shared_types::error_codes::ERR_VALIDATION,
                "Prompt cannot be empty",
            );
        }

        info!(
            "📨 [LocalAgent] Chat request: prompt_len={}, project_id={:?}, session_id={:?}",
            request.prompt.len(),
            request.project_id,
            request.session_id
        );

        // 1. 生成 project_id（如果未提供）
        let project_id = request
            .project_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string().replace('-', ""));

        // 确定用于拼接工作目录的标识符
        // agent_work_dir 用于替代 project_id 参与工作目录路径拼接
        let work_dir_id = request
            .agent_work_dir
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| project_id.clone());

        // 校验 agent_work_dir（如果提供了）
        // 注意：这里使用 project_id 作为 fallback，所以 work_dir_id 不会为空
        // 但如果 agent_work_dir 有值且不合法，需要返回错误。
        // 本服务是 WebAgentRunner 形态（HTTP Server 直跑模式）：绝对路径形态
        // 不支持（work_dir 会流入容器挂载配置语义），for_service 显式拒绝
        if let Err(e) = shared_types::validate_agent_work_dir_for_service(
            &ServiceType::WebAgentRunner,
            &work_dir_id,
        ) {
            return HttpResult::error(shared_types::error_codes::ERR_VALIDATION, &e);
        }

        // 2. 自动查找现有 session_id（如果未提供）
        let session_id = request
            .session_id
            .or_else(|| AGENT_REGISTRY.get_session_by_project(&project_id));

        // 3. 创建项目工作目录（work_dir_id 已由上方校验保证为单段，
        // join 不会被绝对路径替换前缀）
        let project_dir = self.projects_dir.join(&work_dir_id);
        if let Err(e) = tokio::fs::create_dir_all(&project_dir).await {
            let error_msg = format!("Failed to create project dir: {}", e);
            warn!("[LocalAgent] {}", error_msg);
            return HttpResult::error(
                shared_types::error_codes::ERR_INTERNAL_SERVER_ERROR,
                &error_msg,
            );
        }

        // 4. 生成 request_id（如果未提供）
        let request_id = request
            .request_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        // 5. 构建 ChatHandlerInput
        let input = ChatHandlerInput {
            project_id: project_id.clone(),
            project_dir,
            session_id,
            force_new_session: false,
            prompt: request.prompt,
            request_id: request_id.clone(),
            attachments: request.attachments,
            data_source_attachments: request.data_source_attachments,
            model_config: request.model_provider,
            service_type: ServiceType::WebAgentRunner,
            user_id: None,
            agent_config_override: request.agent_config,
            system_prompt_override: request.system_prompt,
            user_prompt_template_override: request.user_prompt,
            is_devcomputer: false,
        };

        // 6. 构建 ChatHandlerContext
        let context = ChatHandlerContext {
            agent_session_service: self.agent_session_service.clone(),
            shared_api_key_manager: self.shared_api_key_manager.clone(),
            project_uuid_map: self.project_uuid_map.clone(),
        };

        // 7. 调用 handle_chat_core
        let output = handle_chat_core(input, &context).await;

        if output.success
            && SessionHint::classify(Some(&output.session_id))
                .as_session_id()
                .is_none()
        {
            return HttpResult::error(
                shared_types::error_codes::ERR_AGENT_ERROR,
                "Agent returned an invalid session identity after task acceptance; do not automatically replay the prompt",
            );
        }

        // Cache creation uses the same identity boundary as notifications and gRPC.
        if output.success
            && let Err(error) = get_or_create_session_data(&output.session_id).await
        {
            return HttpResult::error(
                shared_types::error_codes::ERR_AGENT_ERROR,
                &format!(
                    "Session cache registration failed after task acceptance: {error}; do not automatically replay the prompt"
                ),
            );
        }

        // 9. 构建响应
        if output.error.is_some() || !output.success {
            let error_code = output
                .error_code
                .as_deref()
                .unwrap_or(shared_types::error_codes::ERR_INTERNAL_SERVER_ERROR);
            let error_msg = output.error.unwrap_or_else(|| "Unknown error".to_string());
            HttpResult::error(error_code, &error_msg)
        } else {
            HttpResult::success(ChatResponse {
                project_id: output.project_id,
                session_id: output.session_id,
                error: output.error,
                request_id: Some(request_id),
                need_fallback: None,
                fallback_reason: None,
                reloaded: if output.reloaded { Some(true) } else { None },
                agent_version: output.agent_version,
            })
        }
    }

    /// 查询 Agent 状态
    async fn get_status(&self, project_id: &str) -> HttpResult<AgentStatusResponse> {
        info!("[LocalAgent] Status query: project_id={}", project_id);

        if let Some(info) = AGENT_REGISTRY.get_agent_info(project_id) {
            // Agent 存在且活跃
            let response = AgentStatusResponse {
                project_id: project_id.to_string(),
                is_alive: true,
                session_id: SessionHint::classify(Some(info.session_id.0.as_ref()))
                    .as_session_id()
                    .map(str::to_owned),
                status: Some(info.status),
                last_activity: Some(info.last_activity),
                created_at: Some(info.created_at),
                model_provider: None, // AgentRegistry 不存储 model_provider
            };

            info!(
                "[LocalAgent] Agent status: project_id={}, is_alive=true, status={:?}",
                project_id, info.status
            );

            HttpResult::success(response)
        } else {
            // Agent 不存在
            info!("[LocalAgent] Agent not found: project_id={}", project_id);

            let response = AgentStatusResponse {
                project_id: project_id.to_string(),
                is_alive: false,
                session_id: None,
                status: None,
                last_activity: None,
                created_at: None,
                model_provider: None,
            };

            HttpResult::success(response)
        }
    }

    /// Stop the project-owned physical execution, including legacy invalid IDs.
    async fn stop(&self, request: RcoderAgentStopRequest) -> HttpResult<RcoderAgentStopResponse> {
        let project_id = request.project_id;
        let _transition = agent_abstraction::session::lock_execution_transition(&project_id).await;
        let target = AGENT_REGISTRY.view_agent_info(&project_id, Clone::clone);
        let Some(target) = target else {
            return HttpResult::success(RcoderAgentStopResponse {
                success: true,
                project_id,
                session_id: None,
                message: "Agent execution is absent".into(),
            });
        };
        let session_id = target.session_id.to_string();
        if let Err(error) = Self::retire_execution(&project_id, &target).await {
            return HttpResult::error(
                shared_types::error_codes::ERR_AGENT_ERROR,
                &format!("Agent exit is unconfirmed: {error}"),
            );
        }
        HttpResult::success(RcoderAgentStopResponse {
            success: true,
            project_id,
            session_id: SessionHint::classify(Some(&session_id))
                .as_session_id()
                .map(str::to_owned),
            message: "Agent execution exited and was reaped".into(),
        })
    }

    /// 取消正在执行的任务
    async fn cancel(
        &self,
        request: RcoderAgentCancelRequest,
    ) -> HttpResult<RcoderAgentCancelResponse> {
        info!(
            "🚫 [LocalAgent] Cancel request: project_id={}, session_id={:?}",
            request.project_id, request.session_id
        );

        let _transition =
            agent_abstraction::session::lock_execution_transition(&request.project_id).await;
        let target = AGENT_REGISTRY.view_agent_info(&request.project_id, Clone::clone);
        let Some(target) = target else {
            return HttpResult::success(RcoderAgentCancelResponse {
                success: true,
                session_id: String::new(),
            });
        };
        let session_id = target.session_id.to_string();
        if request
            .session_id
            .as_deref()
            .filter(|sid| !sid.is_empty())
            .is_some_and(|sid| sid != session_id)
        {
            return HttpResult::error(
                shared_types::error_codes::ERR_VALIDATION,
                "session_id no longer matches this project execution",
            );
        }
        if SessionHint::classify(Some(&session_id))
            .as_session_id()
            .is_none()
        {
            // No ACP cancellation can be safely addressed with a placeholder.
            // End this owned execution so the original Chat can create a real
            // session on retry; never replay the already accepted prompt.
            if let Err(error) = Self::retire_execution(&request.project_id, &target).await {
                return HttpResult::error(
                    shared_types::error_codes::ERR_AGENT_ERROR,
                    &format!("Agent exit is unconfirmed: {error}"),
                );
            }
            return HttpResult::success(RcoderAgentCancelResponse {
                success: true,
                session_id: String::new(),
            });
        }
        if AGENT_REGISTRY
            .get_project_by_session(&session_id)
            .is_some_and(|owner| owner != request.project_id)
        {
            return HttpResult::error(
                shared_types::error_codes::ERR_VALIDATION,
                "session_id belongs to a different project",
            );
        }
        match target.status {
            shared_types::AgentStatus::Idle | shared_types::AgentStatus::Terminating => {
                return HttpResult::success(RcoderAgentCancelResponse {
                    success: true,
                    session_id,
                });
            }
            shared_types::AgentStatus::Active | shared_types::AgentStatus::Pending => {}
        }
        if target.cancel_tx.is_closed() {
            return HttpResult::error(
                shared_types::error_codes::ERR_AGENT_ERROR,
                "Agent cancellation channel is closed; execution exit is unconfirmed",
            );
        }
        let (result_tx, _result_rx) = oneshot::channel();
        let cancel_request = shared_types::CancelNotificationRequestWrapper {
            cancel_notification: CancelNotification::new(SessionId::new(Arc::from(
                session_id.as_str(),
            ))),
            result_tx,
        };
        if let Err(error) = target.cancel_tx.send(cancel_request).await {
            return HttpResult::error(
                shared_types::error_codes::ERR_AGENT_ERROR,
                &format!("Agent cancellation was not accepted: {error}"),
            );
        }
        // The valid-session API acknowledges acceptance; terminal SSE still
        // reports the task outcome and the agent remains available for reuse.
        HttpResult::success(RcoderAgentCancelResponse {
            success: true,
            session_id,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::service::SessionData;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::{Notify, mpsc};

    #[derive(Default)]
    pub(crate) struct ControlledExit {
        pub(crate) entered: Notify,
        pub(crate) release: Notify,
        pub(crate) fail: AtomicBool,
        pub(crate) exited: AtomicBool,
        pub(crate) waits: AtomicUsize,
        token: tokio_util::sync::CancellationToken,
    }

    impl shared_types::AgentLifecycle for ControlledExit {
        fn graceful_stop(
            &self,
        ) -> std::pin::Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
            Box::pin(async move {
                self.entered.notify_one();
                self.release.notified().await;
                anyhow::ensure!(
                    !self.fail.load(Ordering::SeqCst),
                    "controlled exit not confirmed"
                );
                self.exited.store(true, Ordering::SeqCst);
                Ok(())
            })
        }

        fn wait_for_exit(
            &self,
        ) -> std::pin::Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
            Box::pin(async move {
                self.waits.fetch_add(1, Ordering::SeqCst);
                anyhow::ensure!(self.exited.load(Ordering::SeqCst), "exit not observed");
                Ok(())
            })
        }

        fn cancel(&self) {
            self.token.cancel();
        }
        fn is_stopped(&self) -> bool {
            self.exited.load(Ordering::SeqCst)
        }
        fn cancellation_token(&self) -> &tokio_util::sync::CancellationToken {
            &self.token
        }
    }

    pub(crate) struct Execution {
        pub(crate) project: String,
        pub(crate) lifecycle: Arc<ControlledExit>,
        _prompt: mpsc::Receiver<agent_client_protocol::schema::v1::PromptRequest>,
        pub(crate) cancel: mpsc::Receiver<shared_types::CancelNotificationRequestWrapper>,
    }

    impl Execution {
        pub(crate) fn seed(session: &str) -> Self {
            let project = format!("local-control-{}", uuid::Uuid::new_v4());
            let lifecycle = Arc::new(ControlledExit::default());
            let (prompt_tx, prompt) = mpsc::channel(1);
            let (cancel_tx, cancel) = mpsc::channel(1);
            // Model an already-existing physical execution. New registrations
            // must not manufacture an invalid ACP identity.
            AGENT_REGISTRY.inner_mut().insert(
                project.clone(),
                shared_types::ProjectAndAgentInfo {
                    project_id: project.clone(),
                    session_id: SessionId::new(Arc::from(session)),
                    prompt_tx,
                    cancel_tx,
                    model_provider: None,
                    request_id: Some("captured".into()),
                    status: shared_types::AgentStatus::Active,
                    last_activity: chrono::Utc::now(),
                    created_at: chrono::Utc::now(),
                    stop_handle: Some(lifecycle.clone()),
                    agent_binary_snapshot: None,
                },
            );
            Self {
                project,
                lifecycle,
                _prompt: prompt,
                cancel,
            }
        }
    }

    impl Drop for Execution {
        fn drop(&mut self) {
            AGENT_REGISTRY.remove_by_project(&self.project);
        }
    }

    pub(crate) fn service() -> Arc<LocalAgentHttpService> {
        Arc::new(LocalAgentHttpService::new(
            Arc::new(AgentSessionService::new(
                Arc::new(agent_abstraction::launcher::DirectModelRuntimeEnvResolver),
                1,
            )),
            Arc::new(DashMap::new()),
            Arc::new(DashMap::new()),
            PathBuf::new(),
        ))
    }

    #[tokio::test]
    async fn invalid_sid_status_keeps_physical_execution_visible_without_publishing_sid() {
        let execution = Execution::seed("pending");
        let response = service().get_status(&execution.project).await;
        let data = response.data.unwrap();
        assert!(data.is_alive);
        assert!(data.session_id.is_none());
        assert_eq!(data.status, Some(shared_types::AgentStatus::Active));
        assert!(AGENT_REGISTRY.get_project_by_session("pending").is_none());
    }

    #[tokio::test]
    async fn local_stop_waits_for_invalid_sid_execution_exit_without_clearing_shared_sid_cache() {
        let execution = Execution::seed("pending");
        let cache = SessionData::new(8).await;
        SESSION_CACHE.insert("pending".into(), cache.clone());
        let svc = service();
        let project = execution.project.clone();
        let stop = tokio::spawn(async move {
            svc.stop(RcoderAgentStopRequest {
                project_id: project,
            })
            .await
        });
        tokio::time::timeout(
            Duration::from_secs(2),
            execution.lifecycle.entered.notified(),
        )
        .await
        .unwrap();
        assert!(!stop.is_finished());
        assert!(AGENT_REGISTRY.inner_mut().contains_key(&execution.project));
        execution.lifecycle.release.notify_one();
        let result = stop.await.unwrap();
        assert!(result.success);
        assert!(result.data.unwrap().session_id.is_none());
        assert_eq!(execution.lifecycle.waits.load(Ordering::SeqCst), 1);
        assert!(!AGENT_REGISTRY.inner_mut().contains_key(&execution.project));
        assert!(
            SESSION_CACHE
                .view("pending", |_, value| Arc::ptr_eq(value, &cache))
                .unwrap()
        );
        SESSION_CACHE.remove_if("pending", |_, value| Arc::ptr_eq(value, &cache));
    }

    #[tokio::test]
    async fn unconfirmed_local_stop_retains_execution_and_allows_retry_after_exit_recovers() {
        let execution = Execution::seed("pending");
        let svc = service();
        execution.lifecycle.fail.store(true, Ordering::SeqCst);
        execution.lifecycle.release.notify_one();
        let failed = svc
            .stop(RcoderAgentStopRequest {
                project_id: execution.project.clone(),
            })
            .await;
        assert!(!failed.success);
        assert!(failed.message.contains("controlled exit not confirmed"));
        assert!(AGENT_REGISTRY.inner_mut().contains_key(&execution.project));
        execution.lifecycle.fail.store(false, Ordering::SeqCst);
        execution.lifecycle.release.notify_one();
        let retried = svc
            .stop(RcoderAgentStopRequest {
                project_id: execution.project.clone(),
            })
            .await;
        assert!(retried.success);
        assert!(!AGENT_REGISTRY.inner_mut().contains_key(&execution.project));
    }

    #[tokio::test]
    async fn invalid_sid_local_cancel_reaps_owned_execution_without_sending_acp_placeholder() {
        let mut execution = Execution::seed("pending");
        execution.lifecycle.release.notify_one();
        let result = service()
            .cancel(RcoderAgentCancelRequest {
                project_id: execution.project.clone(),
                session_id: Some("pending".into()),
            })
            .await;
        assert!(result.success);
        assert!(result.data.unwrap().session_id.is_empty());
        assert!(execution.lifecycle.exited.load(Ordering::SeqCst));
        assert!(!AGENT_REGISTRY.inner_mut().contains_key(&execution.project));
        assert!(execution.cancel.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_cancel_rejects_mismatched_sid_but_preserves_valid_session_cancel_acceptance() {
        let mut execution = Execution::seed("opaque-session");
        let svc = service();
        let wrong = svc
            .cancel(RcoderAgentCancelRequest {
                project_id: execution.project.clone(),
                session_id: Some("foreign-session".into()),
            })
            .await;
        assert!(!wrong.success);
        assert_eq!(wrong.code, shared_types::error_codes::ERR_VALIDATION);
        assert!(execution.cancel.try_recv().is_err());
        let accepted = svc
            .cancel(RcoderAgentCancelRequest {
                project_id: execution.project.clone(),
                session_id: None,
            })
            .await;
        assert!(accepted.success);
        assert_eq!(accepted.data.unwrap().session_id, "opaque-session");
        assert_eq!(
            execution
                .cancel
                .recv()
                .await
                .unwrap()
                .cancel_notification
                .session_id
                .to_string(),
            "opaque-session"
        );
        assert!(!execution.lifecycle.exited.load(Ordering::SeqCst));
    }
}
