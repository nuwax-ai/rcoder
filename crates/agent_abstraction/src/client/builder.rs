//! AcpClientBuilder — fluent API for assembling and launching an ACP agent.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use shared_types::{AgentMode, ChatAgentServerConfig, ModelProviderConfig, ProjectAndAgentInfo};

use crate::client::acp_client::{AcpClient, PromptCompletionSignal};
use crate::diagnostics::DiagnosticsListener;
use crate::launcher::model_env::{DirectModelRuntimeEnvResolver, ModelRuntimeEnvResolver};
use crate::session::AcpSessionManager;
use crate::traits::permission_handler::PermissionRequestHandler;
use crate::traits::session_notifier::SessionNotifier;
use crate::traits::session_registry::SessionRegistry;
use crate::traits::{AgentStartConfig, YoloPermissionRequestHandler};
use shared_types::SessionEntry;

/// Fluent builder for launching an ACP agent via [`AcpClient`].
///
/// # Minimal usage
///
/// ```ignore
/// let client = AcpClientBuilder::new(notifier, registry)
///     .command("my-agent")
///     .working_dir("/workspace")
///     .start()
///     .await?;
/// ```
pub struct AcpClientBuilder<N: SessionNotifier, R: SessionRegistry> {
    // Required
    notifier: Arc<N>,
    registry: Arc<R>,

    // Agent command
    command: Option<String>,
    args: Vec<String>,
    env: HashMap<String, String>,

    // Session / project
    working_dir: PathBuf,
    project_id: Option<String>,
    agent_id: String,
    session_id_hint: Option<String>,

    // Agent behavior
    system_prompt: Option<String>,
    agent_mode: AgentMode,
    service_type: shared_types::ServiceType,

    // Model
    model_provider: Option<ModelProviderConfig>,
    model_env_resolver: Option<Arc<dyn ModelRuntimeEnvResolver>>,

    // Permissions
    permission_handler: Option<Arc<dyn PermissionRequestHandler>>,

    // Timeouts
    timeout: Duration,

    // Diagnostics
    diagnostics_listener: Option<Arc<dyn DiagnosticsListener>>,

    // Completion signaling — the builder creates a oneshot channel,
    // the builder stores the sender in the notifier wrapper (if desired).
    // For now, expose the receiver so the caller can await it.
    completion_signal: Option<PromptCompletionSignal>,
}

impl<N: SessionNotifier + 'static, R: SessionRegistry + 'static> AcpClientBuilder<N, R>
where
    R::Entry: Into<ProjectAndAgentInfo> + From<ProjectAndAgentInfo>,
{
    /// Create a new builder with required dependencies.
    ///
    /// # Arguments
    /// * `notifier` — receives real-time agent notifications (SSE, terminal, etc.)
    /// * `registry` — stores session state (can be a simple in-memory map for CLI)
    pub fn new(notifier: N, registry: R) -> Self {
        Self {
            notifier: Arc::new(notifier),
            registry: Arc::new(registry),
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            working_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            project_id: None,
            agent_id: "custom-agent".to_string(),
            session_id_hint: None,
            system_prompt: None,
            agent_mode: AgentMode::Yolo,
            service_type: shared_types::ServiceType::RCoder,
            model_provider: None,
            model_env_resolver: None,
            permission_handler: None,
            timeout: Duration::from_secs(300),
            diagnostics_listener: None,
            completion_signal: None,
        }
    }

    /// Agent startup command (e.g. `"python"`, `"./my-agent"`).
    pub fn command(mut self, cmd: impl Into<String>) -> Self {
        self.command = Some(cmd.into());
        self
    }

    /// Agent command arguments.
    pub fn args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    /// Add a single environment variable for the agent subprocess.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Set multiple environment variables at once.
    pub fn envs(mut self, envs: HashMap<String, String>) -> Self {
        self.env.extend(envs);
        self
    }

    /// Agent working directory.
    pub fn working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_dir = dir.into();
        self
    }

    /// Project ID (auto-generated UUID if not set).
    pub fn project_id(mut self, id: impl Into<String>) -> Self {
        self.project_id = Some(id.into());
        self
    }

    /// Agent identifier (default: `"custom-agent"`).
    pub fn agent_id(mut self, id: impl Into<String>) -> Self {
        self.agent_id = id.into();
        self
    }

    /// Resume an existing session by its ID.
    pub fn resume_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id_hint = Some(session_id.into());
        self
    }

    /// Custom system prompt appended to the agent's base prompt.
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Permission approval mode (`Yolo` auto-approves, `Ask` requires user confirmation).
    pub fn agent_mode(mut self, mode: AgentMode) -> Self {
        self.agent_mode = mode;
        self
    }

    /// Service type (default: `RCoder`).
    pub fn service_type(mut self, st: shared_types::ServiceType) -> Self {
        self.service_type = st;
        self
    }

    /// Model provider configuration (API key, base URL, model name).
    pub fn model_provider(mut self, config: ModelProviderConfig) -> Self {
        self.model_provider = Some(config);
        self
    }

    /// Override the default model environment resolver.
    ///
    /// By default, [`DirectModelRuntimeEnvResolver`] is used (pass-through).
    pub fn model_env_resolver(mut self, resolver: Arc<dyn ModelRuntimeEnvResolver>) -> Self {
        self.model_env_resolver = Some(resolver);
        self
    }

    /// Override the default permission handler.
    ///
    /// By default, [`YoloPermissionRequestHandler`] is used (auto-approve).
    pub fn permission_handler(mut self, handler: Arc<dyn PermissionRequestHandler>) -> Self {
        self.permission_handler = Some(handler);
        self
    }

    /// Prompt timeout (default: 300 seconds).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Inject a diagnostics listener to receive agent process lifecycle events.
    ///
    /// The listener receives callbacks when the agent subprocess starts,
    /// finishes ACP initialization, exits, or encounters an error.
    /// This is primarily useful for CLI tools that need detailed error output.
    pub fn diagnostics_listener(mut self, listener: Arc<dyn DiagnosticsListener>) -> Self {
        self.diagnostics_listener = Some(listener);
        self
    }

    /// Provide an external completion signal so the caller can await prompt completion.
    ///
    /// The `PromptCompletionSignal` contains a `tokio::sync::Notify` that the caller's
    /// `SessionNotifier` implementation should trigger on `notify_prompt_end` or
    /// `notify_prompt_error`.
    pub fn completion_signal(mut self, signal: PromptCompletionSignal) -> Self {
        self.completion_signal = Some(signal);
        self
    }

    /// Launch the agent and return an [`AcpClient`] handle.
    ///
    /// This method:
    /// 1. Assembles `SacpAgentLaunchConfig` from builder fields
    /// 2. Creates an `AcpSessionManager` with injected dependencies
    /// 3. Calls `get_or_create_session()` to start the agent subprocess
    /// 4. Returns an `AcpClient` wrapping the session handles
    pub async fn start(self) -> Result<AcpClient<N, R>> {
        let project_id = self
            .project_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        // Build model env resolver (default: direct pass-through)
        let model_env_resolver: Arc<dyn ModelRuntimeEnvResolver> = self
            .model_env_resolver
            .unwrap_or_else(|| Arc::new(DirectModelRuntimeEnvResolver));

        // Build permission handler (default: yolo)
        let permission_handler: Arc<dyn PermissionRequestHandler> = self
            .permission_handler
            .unwrap_or_else(|| Arc::new(YoloPermissionRequestHandler));

        // Build AgentStartConfig with agent server override
        let agent_server_override = if self.command.is_some() {
            Some(ChatAgentServerConfig {
                agent_id: Some(self.agent_id.clone()),
                command: self.command.clone(),
                args: if self.args.is_empty() {
                    None
                } else {
                    Some(self.args.clone())
                },
                env: if self.env.is_empty() {
                    None
                } else {
                    Some(self.env.clone())
                },
                model_env_bindings: Vec::new(),
                agent_mode: Some(format!("{:?}", self.agent_mode).to_lowercase()),
                metadata: None,
            })
        } else {
            None
        };

        let mut start_config = AgentStartConfig::new(self.service_type.clone())
            .with_agent_mode(self.agent_mode);

        if let Some(sp) = self.system_prompt {
            start_config = start_config.with_system_prompt(sp);
        }
        if let Some(override_cfg) = agent_server_override {
            start_config = start_config.with_agent_server_override(override_cfg);
        }
        if let Some(ref sid) = self.session_id_hint {
            start_config = start_config.with_resume_session_id(sid.clone());
        }

        // Create session manager
        let session_manager = Arc::new(
            AcpSessionManager::<N, R>::with_dependencies(
                self.notifier.clone(),
                self.registry.clone(),
                model_env_resolver,
                permission_handler,
            ),
        );

        // Ensure working directory exists
        AcpSessionManager::<N, R>::ensure_project_dir(&self.working_dir).await?;

        // Launch agent via get_or_create_session
        let (entry, is_new_session) = session_manager
            .get_or_create_session(
                &project_id,
                self.working_dir.clone(),
                self.session_id_hint.clone(),
                self.model_provider.clone(),
                start_config,
                None, // service_uuid
            )
            .await?;

        let entry_info: ProjectAndAgentInfo = entry.into();
        let session_id = entry_info.session_id().to_string();
        let lifecycle_guard = entry_info.stop_handle.clone();

        // Insert the entry back into the registry if needed for subsequent operations
        // (get_or_create_session already inserted it, so we just need the handles)

        tracing::info!(
            "[AcpClient] Agent started: project_id={}, session_id={}, new_session={}",
            project_id,
            session_id,
            is_new_session
        );

        Ok(AcpClient::new(
            session_manager,
            project_id,
            session_id,
            lifecycle_guard,
            self.timeout,
            self.completion_signal,
        ))
    }
}
