//! Channel utility module
//!
//! Provides channel handling utility functions required for agent communication

use crate::acp::{CancelNotificationRequestWrapper, CancelResult};
use crate::traits::{SessionNotifier, SessionRegistry};
use agent_client_protocol::{Agent, ClientSideConnection, McpServer, PromptRequest, SessionId};
use shared_types::{AgentLifecycle, ModelProviderConfig, ProjectAndAgentInfo};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Prompt handler configuration
///
/// Uses generic `R: SessionRegistry` instead of directly depending on DashMap, supporting dependency injection
pub struct PromptHandlerConfig<N: SessionNotifier, R: SessionRegistry> {
    /// Whether this is a resume session
    pub is_resume_session: bool,
    /// Project path (for creating new session during degradation)
    pub project_path: PathBuf,
    /// MCP server configuration (for creating new session during degradation)
    pub mcp_servers: Vec<McpServer>,
    /// Session registry (for updating during degradation)
    pub registry: Arc<R>,
    /// Cancel channel (for creating new session entry during degradation)
    pub cancel_tx: mpsc::UnboundedSender<CancelNotificationRequestWrapper>,
    /// Lifecycle handle (for creating new session entry during degradation)
    pub lifecycle_handle: Option<Arc<dyn AgentLifecycle>>,
    /// Model configuration (for creating new session entry during degradation)
    pub model_provider: Option<ModelProviderConfig>,
    /// Notifier
    pub notifier: Arc<N>,
}

/// Spawn cancel handler for Agent
///
/// Handles cancel requests and returns results to caller via oneshot channel
pub fn spawn_cancel_handler_for_agent(
    client_conn: Arc<ClientSideConnection>,
    mut cancel_rx: mpsc::UnboundedReceiver<CancelNotificationRequestWrapper>,
    project_id: &str,
    cancel_timeout_secs: Option<u64>,
) {
    let project_id = project_id.to_string();
    let timeout_secs = cancel_timeout_secs.unwrap_or(10); // Default 10 seconds
    tokio::task::spawn_local(async move {
        while let Some(cancel_request_wrapper) = cancel_rx.recv().await {
            info!("Project[{}] received cancel request", project_id);

            // Extract CancelNotification and result channel directly from wrapper
            let cancel_notification = cancel_request_wrapper.cancel_notification;
            let result_tx = cancel_request_wrapper.result_tx;

            // Add timeout protection to prevent Agent cancel call from blocking
            let cancel_result = tokio::time::timeout(
                tokio::time::Duration::from_secs(timeout_secs),
                client_conn.cancel(cancel_notification),
            )
            .await;

            // Send response based on result
            let result = match cancel_result {
                Ok(Ok(_)) => {
                    info!("Project[{}] Agent cancel succeeded", project_id);
                    CancelResult::Success
                }
                Ok(Err(e)) => {
                    let error_msg = format!("{:?}", e);
                    error!("Project[{}] send Cancel failed: {}", project_id, error_msg);
                    CancelResult::Failed(error_msg)
                }
                Err(_timeout_err) => {
                    warn!("Project[{}] Agent cancel timeout", project_id);
                    CancelResult::Timeout
                }
            };

            // Return result via oneshot channel
            if let Err(e) = result_tx.send(result) {
                error!(
                    "Project[{}] failed to send cancel result (receiver closed): {:?}",
                    project_id, e
                );
            }
        }

        info!("Project[{}] cancel handler task ended", project_id);
    });
}

/// Spawn prompt handler for Agent
///
/// # Arguments
/// - `client_conn`: ACP client connection
/// - `prompt_rx`: Prompt message receive channel
/// - `session_id`: Current session ID
/// - `project_id`: Project ID
/// - `config`: Prompt handler configuration (contains all information needed for degradation)
///
/// # Degradation Mechanism
/// When the first Prompt of a resume session fails, degradation is completed internally:
/// 1. Create new session (without resume)
/// 2. Update session info in registry
/// 3. Retry Prompt
/// 4. Continue processing subsequent Prompts
pub fn spawn_prompt_handler_for_agent<N: SessionNotifier + 'static, R: SessionRegistry + 'static>(
    client_conn: Arc<ClientSideConnection>,
    mut prompt_rx: mpsc::UnboundedReceiver<PromptRequest>,
    session_id: SessionId,
    project_id: &str,
    config: PromptHandlerConfig<N, R>,
) where
    R::Entry: Into<ProjectAndAgentInfo> + From<ProjectAndAgentInfo>,
{
    let project_id = project_id.to_string();
    let current_session_id = session_id;
    let session_id_str = current_session_id.0.clone();

    // Extract configuration
    let is_resume_session = config.is_resume_session;
    // Variables reserved for future degradation rebuild logic
    let _project_path = config.project_path;
    let _mcp_servers = config.mcp_servers;
    let _registry = config.registry;
    let _cancel_tx = config.cancel_tx;
    let _lifecycle_handle = config.lifecycle_handle;
    let _model_provider = config.model_provider;
    let notifier = config.notifier;

    tokio::task::spawn_local(async move {
        info!(
            "🚀 Project[{}] Prompt handler task started, listening for messages... (is_resume={})",
            project_id, is_resume_session
        );

        // Track if this is the first Prompt (for resume degradation detection)
        let mut is_first_prompt = true;
        // Track if already degraded (only degrade once per session)
        let mut has_fallback = false;

        while let Some(mut req) = prompt_rx.recv().await {
            info!("📨 Project[{}] received Prompt message from prompt_rx", project_id);

            // If received session_id differs from current, force override
            if req.session_id.0 != current_session_id.0 {
                warn!(
                    "Project[{}] received Prompt session_id({}) differs from current agent session({}), forcing override to current session",
                    project_id, req.session_id.0, current_session_id.0
                );
                req.session_id = current_session_id.clone();
            }

            info!(
                "Project[{}] received Prompt message, session_id={}",
                project_id, req.session_id.0
            );

            // Extract request_id from PromptRequest.meta
            let request_id = if let Some(ref meta) = req.meta {
                let req_id = meta
                    .get("request_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                debug!(
                    "🔍 Project[{}] extracted request_id={:?} from PromptRequest.meta",
                    project_id, req_id
                );
                req_id
            } else {
                debug!("⚠️ Project[{}] PromptRequest.meta is empty", project_id);
                None
            };

            // Send SessionPromptStart notification
            if let Err(e) = notifier
                .notify_prompt_start(&project_id, &session_id_str, request_id.clone())
                .await
            {
                error!("Project[{}] failed to send SessionPromptStart: {:?}", project_id, e);
            }

            // Call Agent to handle prompt
            // ⚠️ Note: No timeout set, as Agent tasks may take a long time (code generation, file operations, etc.)
            // Timeout protection is handled by Worker-level heartbeat monitoring
            match client_conn.prompt(req.clone()).await {
                Ok(resp) => {
                    info!(
                        "Project[{}] Prompt sent successfully, stop_reason={:?}",
                        project_id, resp.stop_reason
                    );

                    // Send SessionPromptEnd notification
                    if let Err(e) = notifier
                        .notify_prompt_end(
                            &project_id,
                            &session_id_str,
                            resp.stop_reason,
                            None,
                            request_id.clone(),
                        )
                        .await
                    {
                        error!("Project[{}] failed to send SessionPromptEnd: {:?}", project_id, e);
                    }

                    // First Prompt succeeded, resume is valid
                    is_first_prompt = false;
                }
                Err(e) => {
                    // Agent returned error
                    let error_message = e.message.clone();
                    error!("Project[{}] failed to send Prompt: {:?}", project_id, error_message);

                    // 🆕 Resume degradation logic refactored:
                    // When Resume session first Prompt failure is detected, don't degrade here
                    // Instead return degradation identifier via gRPC response, letting rcoder layer handle degradation
                    let should_fallback = is_first_prompt && is_resume_session && !has_fallback;

                    if should_fallback {
                        warn!(
                            "⚠️ Project[{}] Resume session first Prompt failed, needs degradation: {}",
                            project_id, error_message
                        );

                        // Send error notification
                        if let Err(notify_err) = notifier
                            .notify_prompt_error(
                                &project_id,
                                &session_id_str,
                                e.clone(),
                                request_id.clone(),
                            )
                            .await
                        {
                            error!(
                                "Project[{}] failed to send SessionPromptError: {:?}",
                                project_id, notify_err
                            );
                        }

                        // Send SessionPromptEnd notification
                        if let Err(notify_err) = notifier
                            .notify_prompt_end(
                                &project_id,
                                &session_id_str,
                                agent_client_protocol::StopReason::Cancelled,
                                Some(error_message.clone()),
                                request_id.clone(),
                            )
                            .await
                        {
                            error!(
                                "Project[{}] failed to send SessionPromptEnd: {:?}",
                                project_id, notify_err
                            );
                        }

                        // Mark as degraded to prevent repeated detection
                        has_fallback = true;

                        // Continue processing next Prompt
                        info!("⚠️ Project[{}] Resume failure notified, continuing to wait for retry", project_id);
                        continue;
                    }

                    // Normal error handling flow
                    // Send SessionPromptError notification
                    if let Err(notify_err) = notifier
                        .notify_prompt_error(&project_id, &session_id_str, e, request_id.clone())
                        .await
                    {
                        error!(
                            "Project[{}] failed to send SessionPromptError: {:?}",
                            project_id, notify_err
                        );
                    }

                    // Send SessionPromptEnd notification, marking session end
                    if let Err(notify_err) = notifier
                        .notify_prompt_end(
                            &project_id,
                            &session_id_str,
                            agent_client_protocol::StopReason::Cancelled,
                            Some(error_message),
                            request_id.clone(),
                        )
                        .await
                    {
                        error!(
                            "Project[{}] failed to send SessionPromptEnd: {:?}",
                            project_id, notify_err
                        );
                    }

                    // First Prompt processing complete (whether success or failure)
                    is_first_prompt = false;
                }
            }
        }

        info!("Project[{}] Prompt handler task ended", project_id);
    });
}
