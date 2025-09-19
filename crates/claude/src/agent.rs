use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::SystemTime;

use agent_client_protocol::{
    Agent, AgentCapabilities, AuthMethod, AuthMethodId, AuthenticateRequest, AuthenticateResponse,
    AvailableCommand, CancelNotification, ContentBlock,
    EmbeddedResourceResource, Error, ExtNotification, ExtRequest, ExtResponse, InitializeRequest,
    InitializeResponse, LoadSessionRequest, LoadSessionResponse, McpCapabilities,
    NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, SessionId,
    SessionNotification, SessionUpdate, SetSessionModeRequest, SetSessionModeResponse, StopReason,
    V1,
};
use serde_json::json;
use tokio::sync::{mpsc, oneshot, oneshot::Sender};
use tokio::task;
use tracing::{info, warn};

mod commands;

// Placeholder for per-session state. Holds the Claude conversation
// handle, its id (for status/reporting), and bookkeeping for streaming.
#[derive(Clone)]
struct SessionState {
    #[allow(dead_code)]
    created: SystemTime,
    // Conversation id string for display/logging purposes.
    conversation_id: String,
    current_approval: ApprovalPolicy,
    token_usage: Option<TokenUsage>,
}

#[derive(Debug, Clone)]
pub enum ApprovalPolicy {
    OnRequest,
    OnFailure,
    Never,
    UnlessTrusted,
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self::OnRequest
    }
}

#[derive(Debug, Clone)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct ClaudeConfig {
    pub claude_home: std::path::PathBuf,
    pub cwd: std::path::PathBuf,
    pub model: String,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            claude_home: std::path::PathBuf::from("~/.claude"),
            cwd: std::env::current_dir().unwrap_or_default(),
            model: "claude-3-5-sonnet-20241022".to_string(),
        }
    }
}

pub struct ClaudeAgent {
    session_update_tx: mpsc::UnboundedSender<(SessionNotification, Sender<()>)>,
    next_session_id: Cell<u64>,
    sessions: Rc<RefCell<HashMap<String, SessionState>>>,
    config: ClaudeConfig,
    next_submit_seq: Cell<u64>,
    extra_available_commands: Rc<RefCell<Vec<AvailableCommand>>>,
    client_tx: mpsc::UnboundedSender<ClientOp>,
}

impl ClaudeAgent {
    pub fn with_config(
        session_update_tx: mpsc::UnboundedSender<(SessionNotification, Sender<()>)>,
        client_tx: mpsc::UnboundedSender<ClientOp>,
        config: ClaudeConfig,
    ) -> Self {
        Self {
            session_update_tx,
            next_session_id: Cell::new(1),
            sessions: Rc::new(RefCell::new(HashMap::new())),
            config,
            next_submit_seq: Cell::new(1),
            extra_available_commands: Rc::new(RefCell::new(Vec::new())),
            client_tx,
        }
    }

    pub fn send_message_chunk(
        &self,
        session_id: &SessionId,
        content: ContentBlock,
        tx: Sender<()>,
    ) -> Result<(), Error> {
        self.session_update_tx
            .send((
                SessionNotification {
                    session_id: session_id.clone(),
                    update: SessionUpdate::AgentMessageChunk { content },
                    meta: None,
                },
                tx,
            ))
            .map_err(Error::into_internal_error)?;
        Ok(())
    }

    fn handle_response_outcome(&self, resp: RequestPermissionResponse) -> ApprovalDecision {
        match resp.outcome {
            RequestPermissionOutcome::Selected { option_id } => {
                if option_id.0.as_ref() == "approve" {
                    ApprovalDecision::Approved
                } else if option_id.0.as_ref() == "approve_for_session" {
                    ApprovalDecision::ApprovedForSession
                } else {
                    ApprovalDecision::Denied
                }
            }
            RequestPermissionOutcome::Cancelled => ApprovalDecision::Abort,
        }
    }
}

#[derive(Debug)]
pub enum ClientOp {
    RequestPermission(
        RequestPermissionRequest,
        Sender<Result<RequestPermissionResponse, Error>>,
    ),
}

#[derive(Debug)]
pub enum ApprovalDecision {
    Approved,
    ApprovedForSession,
    Denied,
    Abort,
}

#[async_trait::async_trait(?Send)]
impl Agent for ClaudeAgent {
    async fn initialize(&self, args: InitializeRequest) -> Result<InitializeResponse, Error> {
        info!(?args, "Received initialize request");
        // Advertise supported auth methods for Claude Code
        let auth_methods = vec![
            AuthMethod {
                id: AuthMethodId("claude".into()),
                name: "Claude".into(),
                description: Some("Sign in with Claude Code".into()),
                meta: None,
            },
            AuthMethod {
                id: AuthMethodId("api_key".into()),
                name: "API Key".into(),
                description: Some("Use CLAUDE_API_KEY from environment".into()),
                meta: None,
            },
        ];

        let capacities = AgentCapabilities {
            load_session: true,
            prompt_capabilities: PromptCapabilities {
                image: true,
                audio: false,
                embedded_context: true,
                meta: None,
            },
            mcp_capabilities: McpCapabilities {
                http: true,
                sse: true,
                meta: None,
            },
            meta: None,
        };

        Ok(InitializeResponse {
            protocol_version: V1,
            agent_capabilities: capacities,
            auth_methods,
            meta: None,
        })
    }

    async fn authenticate(&self, args: AuthenticateRequest) -> Result<AuthenticateResponse, Error> {
        info!(?args, "Received authenticate request");
        let method = args.method_id.0.as_ref();
        match method {
            "claude" => {
                // Check if Claude is available and authenticated
                if std::env::var("CLAUDE_API_KEY").is_ok() {
                    return Ok(Default::default());
                }
                Err(Error::auth_required()
                    .with_data("Not authenticated with Claude. Please set CLAUDE_API_KEY or run 'claude auth'"))
            }
            "api_key" => {
                if std::env::var("CLAUDE_API_KEY").is_ok() {
                    return Ok(Default::default());
                }
                Err(Error::auth_required().with_data("CLAUDE_API_KEY not set"))
            }
            other => {
                Err(Error::invalid_params().with_data(format!("unknown auth method: {}", other)))
            }
        }
    }

    async fn new_session(&self, args: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        info!(?args, "Received new session request");
        let session_id = self.next_session_id.get();
        self.next_session_id.set(session_id + 1);

        // Generate a unique conversation ID for this session
        let conversation_id = uuid::Uuid::new_v4().to_string();

        // Track the session
        self.sessions.borrow_mut().insert(
            session_id.to_string(),
            SessionState {
                created: SystemTime::now(),
                conversation_id,
                current_approval: ApprovalPolicy::default(),
                token_usage: None,
            },
        );

        // Advertise available slash commands to the client right after
        // the session is created. Send it asynchronously to avoid racing
        // with the NewSessionResponse delivery.
        {
            let available_commands = self.available_commands();

            let session_id_for_update = SessionId(session_id.to_string().into());
            let tx_updates = self.session_update_tx.clone();
            task::spawn_local(async move {
                let (tx, rx) = oneshot::channel();
                let _ = tx_updates.send((
                    SessionNotification {
                        session_id: session_id_for_update,
                        update: SessionUpdate::AvailableCommandsUpdate { available_commands },
                        meta: None,
                    },
                    tx,
                ));
                let _ = rx.await;
            });
        }

        Ok(NewSessionResponse {
            session_id: SessionId(session_id.to_string().into()),
            modes: None,
            meta: None,
        })
    }

    async fn load_session(&self, args: LoadSessionRequest) -> Result<LoadSessionResponse, Error> {
        info!(?args, "Received load session request");
        let sid_str = args.session_id.0.to_string();

        // Ensure an entry exists for this session. If absent, create one similar to new_session.
        let missing = { !self.sessions.borrow().contains_key(&sid_str) };
        if missing {
            let conversation_id = uuid::Uuid::new_v4().to_string();

            self.sessions.borrow_mut().insert(
                sid_str.clone(),
                SessionState {
                    created: SystemTime::now(),
                    conversation_id,
                    current_approval: ApprovalPolicy::default(),
                    token_usage: None,
                },
            );
        }

        // Re-emit available commands so the client UI can hydrate.
        let available_commands = self.available_commands();

        let session_id_for_update = args.session_id.clone();
        let tx_updates = self.session_update_tx.clone();
        task::spawn_local(async move {
            let (tx, rx) = oneshot::channel();
            let _ = tx_updates.send((
                SessionNotification {
                    session_id: session_id_for_update,
                    update: SessionUpdate::AvailableCommandsUpdate { available_commands },
                    meta: None,
                },
                tx,
            ));
            let _ = rx.await;
        });

        Ok(LoadSessionResponse {
            modes: None,
            meta: None,
        })
    }

    async fn set_session_mode(
        &self,
        args: SetSessionModeRequest,
    ) -> Result<SetSessionModeResponse, Error> {
        info!(?args, "Received set session mode request");
        let sid_str = args.session_id.0.to_string();
        if !self.sessions.borrow().contains_key(&sid_str) {
            return Err(Error::invalid_params());
        }

        // Notify client about the new current mode immediately.
        let (tx, rx) = oneshot::channel();
        self.session_update_tx
            .send((
                SessionNotification {
                    session_id: args.session_id.clone(),
                    update: SessionUpdate::CurrentModeUpdate {
                        current_mode_id: args.mode_id,
                    },
                    meta: None,
                },
                tx,
            ))
            .map_err(Error::into_internal_error)?;
        let _ = rx.await;

        Ok(SetSessionModeResponse { meta: None })
    }

    async fn prompt(&self, args: PromptRequest) -> Result<PromptResponse, Error> {
        info!(?args, "Received prompt request");
        let sid = args.session_id.0.to_string();
        if !self.sessions.borrow().contains_key(&sid) {
            warn!(session_id = %sid, "unknown session_id");
            return Err(Error::invalid_params());
        }

        let sid_str = args.session_id.0.to_string();
        let _session = self
            .sessions
            .borrow()
            .get(&sid_str)
            .cloned()
            .ok_or_else(Error::invalid_params)?;

        // Handle slash commands (e.g., "/status") when the first block is text starting with '/'
        if let Some(ContentBlock::Text(t)) = args.prompt.first() {
            let line = t.text.trim();
            if let Some(cmd) = line.strip_prefix('/') {
                let mut parts = cmd.split_whitespace();
                let name = parts.next().unwrap_or("").to_lowercase();
                let rest = parts.collect::<Vec<_>>().join(" ");
                if self
                    .handle_slash_command(&args.session_id, &name, &rest)
                    .await?
                {
                    return Ok(PromptResponse {
                        stop_reason: StopReason::EndTurn,
                        meta: None,
                    });
                }
            }
        }

        // Process the prompt with Claude Code
        self.handle_claude_prompt(&args.session_id, &args.prompt).await?;

        Ok(PromptResponse {
            stop_reason: StopReason::EndTurn,
            meta: None,
        })
    }

    async fn cancel(&self, args: CancelNotification) -> Result<(), Error> {
        info!(?args, "Received cancel request");
        let sid_str = args.session_id.0.to_string();

        // Check if session exists
        if !self.sessions.borrow().contains_key(&sid_str) {
            return Err(Error::invalid_params());
        }

        // For now, just acknowledge the cancel request
        // In a full implementation, we would interrupt any ongoing Claude operations
        Ok(())
    }

    async fn ext_method(&self, args: ExtRequest) -> Result<ExtResponse, Error> {
        info!(method = %args.method, params = ?args.params, "Received extension method call");
        Ok(serde_json::value::to_raw_value(&json!({"example": "response"}))?.into())
    }

    async fn ext_notification(&self, args: ExtNotification) -> Result<(), Error> {
        info!(method = %args.method, params = ?args.params, "Received extension notification call");
        Ok(())
    }
}

impl ClaudeAgent {
    async fn handle_claude_prompt(
        &self,
        session_id: &SessionId,
        prompt: &[ContentBlock],
    ) -> Result<(), Error> {
        // Extract text content from the prompt
        let mut text_content = String::new();
        for block in prompt {
            match block {
                ContentBlock::Text(t) => {
                    text_content.push_str(&t.text);
                }
                ContentBlock::Image(img) => {
                    // For images, we would need to handle them differently
                    text_content.push_str(&format!("[Image: {}]", img.mime_type));
                }
                ContentBlock::Resource(res) => {
                    if let EmbeddedResourceResource::TextResourceContents(trc) = &res.resource {
                        text_content.push_str(&trc.text);
                    }
                }
                ContentBlock::ResourceLink(link) => {
                    text_content.push_str(&format!("[Resource: {}]", link.uri));
                }
                ContentBlock::Audio(_) => {
                    // Audio not supported yet
                }
            }
        }

        // Here we would integrate with the actual Claude Code API
        // For now, we'll send a mock response
        let response = format!("Claude processed your prompt: {}", text_content);

        let (tx, rx) = oneshot::channel();
        self.send_message_chunk(session_id, response.into(), tx)?;
        rx.await.map_err(Error::into_internal_error)?;

        Ok(())
    }
}