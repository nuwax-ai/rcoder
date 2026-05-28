//! AcpClient — handle to a running ACP agent subprocess.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use shared_types::{AgentLifecycle, ProjectAndAgentInfo, SessionEntry};

use crate::session::AcpSessionManager;
use crate::traits::session_notifier::SessionNotifier;
use crate::traits::session_registry::SessionRegistry;
use crate::traits::PromptMessage;

/// Completion signal for synchronizing prompt send/response.
///
/// The caller creates a `PromptCompletionSignal`, passes it to the builder,
/// and shares the `notify` handle with their `SessionNotifier` implementation.
/// When `notify_prompt_end` or `notify_prompt_error` fires, the notifier
/// calls `notify.notify_one()`, unblocking `AcpClient::send_prompt_and_wait()`.
#[derive(Debug, Clone)]
pub struct PromptCompletionSignal {
    /// Shared notify handle — trigger this from the notifier on prompt completion.
    pub notify: Arc<tokio::sync::Notify>,
}

impl PromptCompletionSignal {
    /// Create a new signal pair.
    pub fn new() -> Self {
        Self {
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

impl Default for PromptCompletionSignal {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle to a running ACP agent.
///
/// Created by [`AcpClientBuilder::start()`](super::AcpClientBuilder::start).
/// Provides methods to send prompts, cancel operations, and stop the agent.
///
/// # Prompt completion
///
/// `send_prompt()` is fire-and-forget at the channel level — the actual
/// agent response flows asynchronously through the `SessionNotifier`.
/// For synchronous prompt-then-wait, use `send_prompt_and_wait()` with
/// a `PromptCompletionSignal`.
pub struct AcpClient<N: SessionNotifier, R: SessionRegistry> {
    session_manager: Arc<AcpSessionManager<N, R>>,
    project_id: String,
    session_id: String,
    lifecycle_guard: Option<Arc<dyn AgentLifecycle>>,
    timeout: Duration,
    completion_signal: Option<PromptCompletionSignal>,
}

impl<N: SessionNotifier + 'static, R: SessionRegistry + 'static> AcpClient<N, R>
where
    R::Entry: Into<ProjectAndAgentInfo> + From<ProjectAndAgentInfo>,
{
    pub(crate) fn new(
        session_manager: Arc<AcpSessionManager<N, R>>,
        project_id: String,
        session_id: String,
        lifecycle_guard: Option<Arc<dyn AgentLifecycle>>,
        timeout: Duration,
        completion_signal: Option<PromptCompletionSignal>,
    ) -> Self {
        Self {
            session_manager,
            project_id,
            session_id,
            lifecycle_guard,
            timeout,
            completion_signal,
        }
    }

    /// Send a text prompt to the agent (fire-and-forget).
    ///
    /// The prompt is sent through the channel. The actual response flows
    /// through the `SessionNotifier` callbacks. Use `send_prompt_and_wait()`
    /// if you need to block until the agent finishes processing.
    pub async fn send_prompt(&self, prompt: impl Into<String>) -> Result<()> {
        let prompt_message = PromptMessage::new(
            prompt.into(),
            self.project_id.clone(),
            std::path::PathBuf::from("."), // not used for prompt building
            uuid::Uuid::new_v4().to_string(),
            shared_types::ServiceType::RCoder,
        );

        self.session_manager
            .send_text_prompt(&self.project_id, &prompt_message)
            .await
    }

    /// Send a prompt and wait for completion using the configured signal.
    ///
    /// Requires that a `PromptCompletionSignal` was provided to the builder.
    /// The caller's `SessionNotifier` must call `signal.notify.notify_one()`
    /// when `notify_prompt_end` or `notify_prompt_error` fires.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No completion signal was configured
    /// - The timeout expires before the agent finishes
    /// - The prompt send fails
    pub async fn send_prompt_and_wait(&self, prompt: impl Into<String>) -> Result<()> {
        let signal = self
            .completion_signal
            .as_ref()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "send_prompt_and_wait requires a completion_signal; \
                     configure it via AcpClientBuilder::completion_signal()"
                )
            })?;

        // Send the prompt
        self.send_prompt(prompt).await?;

        // Wait for completion with timeout
        tokio::select! {
            _ = signal.notify.notified() => {
                tracing::debug!(
                    "[AcpClient] Prompt completed: project_id={}, session_id={}",
                    self.project_id, self.session_id
                );
                Ok(())
            }
            _ = tokio::time::sleep(self.timeout) => {
                anyhow::bail!(
                    "Prompt timed out after {:?}: project_id={}, session_id={}",
                    self.timeout, self.project_id, self.session_id
                );
            }
        }
    }

    /// Cancel the current prompt operation.
    ///
    /// Sends a cancel request through the cancel channel. The agent
    /// should stop processing the current prompt.
    pub async fn cancel(&self) -> Result<()> {
        use agent_client_protocol::schema::CancelNotification;
        use shared_types::CancelNotificationRequestWrapper;

        // Build cancel notification using the constructor
        let session_id = agent_client_protocol::schema::SessionId::new(Arc::from(
            self.session_id.as_str(),
        ));
        let cancel_notification = CancelNotification::new(session_id);

        // Get the cancel_tx from the session
        let entry = self
            .session_manager
            .get_session(&self.project_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", self.project_id))?;

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        entry
            .cancel_tx()
            .send(CancelNotificationRequestWrapper {
                cancel_notification,
                result_tx,
            })
            .await
            .map_err(|e| anyhow::anyhow!("Failed to send cancel: {:?}", e))?;

        // Wait for cancel result with a short timeout
        match tokio::time::timeout(Duration::from_secs(10), result_rx).await {
            Ok(Ok(result)) => {
                tracing::debug!("[AcpClient] Cancel result: {:?}", result);
                Ok(())
            }
            Ok(Err(e)) => anyhow::bail!("Cancel result channel closed: {:?}", e),
            Err(_) => anyhow::bail!("Cancel timed out after 10 seconds"),
        }
    }

    /// Get the session ID assigned by the agent.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Get the project ID.
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// Get the completion signal (if configured) for sharing with notifiers.
    pub fn completion_signal(&self) -> Option<&PromptCompletionSignal> {
        self.completion_signal.as_ref()
    }

    /// Gracefully stop the agent subprocess.
    ///
    /// Sends SIGTERM, waits briefly, then SIGKILL if the process hasn't exited.
    /// After this call, the agent is no longer usable.
    pub async fn stop(self) -> Result<()> {
        tracing::info!(
            "[AcpClient] Stopping agent: project_id={}, session_id={}",
            self.project_id,
            self.session_id
        );

        if let Some(guard) = &self.lifecycle_guard {
            guard.graceful_stop().await?;
        }

        Ok(())
    }
}

impl<N: SessionNotifier, R: SessionRegistry> std::fmt::Debug for AcpClient<N, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpClient")
            .field("project_id", &self.project_id)
            .field("session_id", &self.session_id)
            .field("timeout", &self.timeout)
            .finish()
    }
}
