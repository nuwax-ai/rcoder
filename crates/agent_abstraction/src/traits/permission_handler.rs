use agent_client_protocol::Responder;
use agent_client_protocol::schema::{
    PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome,
};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

/// Runtime context for an ACP permission request.
#[derive(Debug, Clone)]
pub struct PermissionRequestContext {
    pub project_id: String,
    pub user_id: Option<String>,
    pub agent_mode: shared_types::AgentMode,
    pub service_type: shared_types::ServiceType,
    pub request_id: Option<String>,
}

/// Handles ACP permission requests without coupling agent_abstraction to agent_runner.
#[async_trait]
pub trait PermissionRequestHandler: Send + Sync + 'static {
    async fn handle_permission_request(
        &self,
        context: PermissionRequestContext,
        request: RequestPermissionRequest,
        responder: Responder<RequestPermissionResponse>,
    ) -> Result<(), agent_client_protocol::Error>;
}

/// Default handler used outside agent_runner. It preserves the historical YOLO behavior.
#[derive(Debug, Clone, Default)]
pub struct YoloPermissionRequestHandler;

#[async_trait]
impl PermissionRequestHandler for YoloPermissionRequestHandler {
    async fn handle_permission_request(
        &self,
        _context: PermissionRequestContext,
        request: RequestPermissionRequest,
        responder: Responder<RequestPermissionResponse>,
    ) -> Result<(), agent_client_protocol::Error> {
        let selected = request
            .options
            .iter()
            .find(|o| o.kind == PermissionOptionKind::AllowAlways)
            .or_else(|| {
                request
                    .options
                    .iter()
                    .find(|o| o.kind == PermissionOptionKind::AllowOnce)
            })
            .or_else(|| request.options.first());

        if let Some(option) = selected {
            responder.respond(RequestPermissionResponse::new(
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                    option.option_id.clone(),
                )),
            ))
        } else {
            responder.respond(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ))
        }
    }
}

// ============================================================================
// R-4: Interactive permission handler for CLI consumers
// ============================================================================

/// Terminal interaction abstraction — CLI consumers implement this trait
/// to render permission confirmation UI (e.g., numbered choices in terminal).
#[async_trait]
pub trait PermissionPrompt: Send + Sync + 'static {
    /// Display a permission request and wait for the user to select an option.
    ///
    /// # Arguments
    /// * `context` — runtime context (project_id, agent_mode, etc.)
    /// * `request` — the ACP permission request (contains tool name, options, etc.)
    ///
    /// # Returns
    /// The `option_id` of the user's selection, or `None` to cancel.
    async fn prompt_user(
        &self,
        context: &PermissionRequestContext,
        request: &RequestPermissionRequest,
    ) -> Result<Option<String>>;
}

/// Permission handler that delegates to a [`PermissionPrompt`] implementation
/// for interactive user confirmation.
///
/// # Usage
///
/// CLI consumers implement `PermissionPrompt` for terminal I/O, then wrap it
/// in `InteractivePermissionHandler` and pass it to `AcpClientBuilder::permission_handler()`.
///
/// ```ignore
/// struct TerminalPermissionPrompt;
///
/// #[async_trait]
/// impl PermissionPrompt for TerminalPermissionPrompt {
///     async fn prompt_user(
///         &self,
///         _context: &PermissionRequestContext,
///         request: &RequestPermissionRequest,
///     ) -> Result<Option<String>> {
///         eprintln!("[ACP] Agent requests permission:");
///         for (i, opt) in request.options.iter().enumerate() {
///             eprintln!("  [{}] {:?}", i + 1, opt.kind);
///         }
///         // read user input...
///         Ok(Some(request.options[0].option_id.clone()))
///     }
/// }
///
/// let handler = InteractivePermissionHandler::new(Arc::new(TerminalPermissionPrompt));
/// ```
pub struct InteractivePermissionHandler<P: PermissionPrompt> {
    prompt: Arc<P>,
}

impl<P: PermissionPrompt> InteractivePermissionHandler<P> {
    pub fn new(prompt: Arc<P>) -> Self {
        Self { prompt }
    }
}

impl<P: PermissionPrompt> std::fmt::Debug for InteractivePermissionHandler<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InteractivePermissionHandler").finish()
    }
}

#[async_trait]
impl<P: PermissionPrompt> PermissionRequestHandler for InteractivePermissionHandler<P> {
    async fn handle_permission_request(
        &self,
        context: PermissionRequestContext,
        request: RequestPermissionRequest,
        responder: Responder<RequestPermissionResponse>,
    ) -> Result<(), agent_client_protocol::Error> {
        match self.prompt.prompt_user(&context, &request).await {
            Ok(Some(option_id)) => {
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id)),
                ))
            }
            Ok(None) => {
                // User cancelled
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ))
            }
            Err(e) => {
                tracing::error!(
                    "[InteractivePermissionHandler] prompt_user failed: {:?}",
                    e
                );
                // On error, cancel the permission request
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ))
            }
        }
    }
}
