use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::SystemTime;

use agent_client_protocol::{
    Agent, AgentCapabilities, AuthMethod, AuthMethodId, AuthenticateRequest, AuthenticateResponse,
    AvailableCommand, AvailableCommandInput, CancelNotification, ContentBlock,
    EmbeddedResourceResource, Error, ExtNotification, ExtRequest, ExtResponse, InitializeRequest,
    InitializeResponse, LoadSessionRequest, LoadSessionResponse, McpCapabilities,
    NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse,
    RequestPermissionRequest, RequestPermissionResponse, SessionId, SessionNotification, SessionUpdate,
    SetSessionModeRequest, SetSessionModeResponse, StopReason, ToolCall, ToolCallContent, ToolCallId,
    ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind, V1,
    PermissionOptionKind, PermissionOptionId, PermissionOption, RequestPermissionOutcome,
};
use codex_core::{
    config::{Config as CodexConfig, ConfigOverrides}, protocol::{
        AskForApproval, EventMsg, InputItem, Op, ReviewDecision, SandboxPolicy, Submission,
        TokenUsage,
    }, AuthManager, CodexConversation,
    ConversationManager,
    NewConversation,
};
use serde_json::json;
use tokio::sync::{mpsc, oneshot, oneshot::Sender};
use tokio::task;
use tracing::{info, warn};

mod commands;

/// Codex 配置
pub type Config = CodexConfig;

/// 会话状态
#[derive(Clone)]
struct SessionState {
    #[allow(dead_code)]
    created: SystemTime,
    conversation_id: String,
    conversation: Option<Arc<CodexConversation>>,
    current_approval: AskForApproval,
    current_sandbox: SandboxPolicy,
    token_usage: Option<TokenUsage>,
}

#[derive(Clone)]
pub struct CodexAgent {
    session_update_tx: mpsc::UnboundedSender<(SessionNotification, Sender<()>)>,
    next_session_id: Cell<u64>,
    sessions: Rc<RefCell<HashMap<String, SessionState>>>,
    config: Config,
    conversation_manager: Arc<ConversationManager>,
    next_submit_seq: Cell<u64>,
    auth_manager: Arc<std::sync::RwLock<Arc<AuthManager>>>,
    extra_available_commands: Rc<RefCell<Vec<AvailableCommand>>>,
    client_tx: mpsc::UnboundedSender<ClientOp>,
}

#[derive(Debug)]
pub enum ClientOp {
    RequestPermission(
        RequestPermissionRequest,
        Sender<Result<RequestPermissionResponse, Error>>,
    ),
}

impl CodexAgent {
    /// 创建一个新的 CodexAgent 实例，使用默认配置加载
    pub fn new(
        session_update_tx: mpsc::UnboundedSender<(SessionNotification, oneshot::Sender<()>)>,
    ) -> Self {
        // Fallback config load. This will be replaced by an explicit configuration
        // path in main.rs and a dedicated constructor once wired.
        let config = Config::load_with_cli_overrides(vec![], ConfigOverrides::default())
            .unwrap_or_else(|_| {
                // As a last resort, build a config from defaults.
                Config::load_from_base_config_with_overrides(
                    Default::default(),
                    ConfigOverrides::default(),
                    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
                )
                .expect("failed to synthesize default config")
            });
        let (client_tx, _client_rx) = mpsc::unbounded_channel();
        Self::with_config(session_update_tx, client_tx, config)
    }

    pub fn with_config(
        session_update_tx: mpsc::UnboundedSender<(SessionNotification, Sender<()>)>,
        client_tx: mpsc::UnboundedSender<ClientOp>,
        config: Config,
    ) -> Self {
        let auth = AuthManager::shared(
            config.codex_home.clone(),
        );
        let conversation_manager = ConversationManager::new(auth.clone());

        Self {
            session_update_tx,
            next_session_id: Cell::new(1),
            sessions: Rc::new(RefCell::new(HashMap::new())),
            config,
            conversation_manager: Arc::new(conversation_manager),
            next_submit_seq: Cell::new(1),
            auth_manager: Arc::new(std::sync::RwLock::new(auth)),
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

    fn handle_response_outcome(&self, resp: RequestPermissionResponse) -> ReviewDecision {
        let decision = match resp.outcome {
            RequestPermissionOutcome::Selected { option_id } => {
                if option_id.0.as_ref() == "approve" {
                    ReviewDecision::Approved
                } else if option_id.0.as_ref() == "approve_for_session" {
                    ReviewDecision::ApprovedForSession
                } else {
                    ReviewDecision::Denied
                }
            }
            RequestPermissionOutcome::Cancelled => ReviewDecision::Abort,
        };
        decision
    }
}

#[async_trait::async_trait(?Send)]
impl Agent for CodexAgent {
    async fn initialize(&self, args: InitializeRequest) -> Result<InitializeResponse, Error> {
        info!(?args, "Received initialize request");

        let auth_methods = vec![
            AuthMethod {
                id: AuthMethodId("chatgpt".into()),
                name: "ChatGPT".into(),
                description: Some("Sign in with ChatGPT to use your plan".into()),
                meta: None,
            },
            AuthMethod {
                id: AuthMethodId("apikey".into()),
                name: "OpenAI API Key".into(),
                description: Some("Use OPENAI_API_KEY from environment or auth.json".into()),
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
            "chatgpt" => {
                if let Ok(am) = self.auth_manager.read() {
                    am.reload();
                    if am.auth().is_some() {
                        return Ok(Default::default());
                    }
                }
                Err(Error::auth_required()
                    .with_data("Not signed in. Please run 'codex login' to sign in with ChatGPT."))
            }
            "apikey" => {
                if let Ok(am) = self.auth_manager.write() {
                    am.reload();
                    if am.auth().is_some() {
                        return Ok(Default::default());
                    }
                }
                Err(Error::auth_required().with_data("Failed to load API key auth"))
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

        // Start a new Codex conversation for this session
        let codex_config = self.config.clone();

        let (conversation_id, conversation_opt) = match self
            .conversation_manager
            .new_conversation(codex_config)
            .await
        {
            Ok(NewConversation {
                conversation_id,
                conversation,
                session_configured: _,
            }) => (conversation_id, Some(conversation)),
            Err(e) => {
                warn!(error = %e, "Failed to create Codex conversation");
                (codex_protocol::mcp_protocol::ConversationId::new(), None)
            }
        };

        // Track the session
        self.sessions.borrow_mut().insert(
            session_id.to_string(),
            SessionState {
                created: SystemTime::now(),
                conversation_id: conversation_id.to_string(),
                conversation: conversation_opt,
                current_approval: AskForApproval::OnRequest,
                current_sandbox: SandboxPolicy::new_workspace_write_policy(),
                token_usage: None,
            },
        );

        // Advertise available slash commands to the client
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

        // Discover custom prompts and advertise them as additional commands
        {
            let sid_str = session_id.to_string();
            let tx_updates = self.session_update_tx.clone();
            let submit_seq = self.next_submit_seq.get();
            self.next_submit_seq.set(submit_seq + 1);
            let submit_id = format!("s{}-{}", sid_str, submit_seq);
            let session_map = self.sessions.borrow();
            let extra_cache = self.extra_available_commands.clone();
            if let Some(state) = session_map.get(&sid_str) {
                let conversation = state.conversation.clone();
                let session_id_for_update = SessionId(sid_str.clone().into());
                task::spawn_local(async move {
                    let Some(conversation) = conversation else {
                        return;
                    };

                    // Request custom prompts
                    let _ = conversation
                        .submit_with_id(Submission {
                            id: submit_id.clone(),
                            op: Op::ListCustomPrompts,
                        })
                        .await;

                    // Wait for response and then update available commands
                    loop {
                        match conversation.next_event().await {
                            Ok(event) if event.id == submit_id => {
                                match event.msg {
                                    EventMsg::ListCustomPromptsResponse(resp) => {
                                        // Build extra commands from custom prompts and cache them
                                        let mut extra: Vec<AvailableCommand> = Vec::new();
                                        for p in resp.custom_prompts {
                                            let desc =
                                                format!("custom prompt ({})", p.path.display());
                                            extra.push(AvailableCommand {
                                                name: p.name,
                                                description: desc,
                                                input: Some(AvailableCommandInput::Unstructured {
                                                    hint: "Additional input (optional)".into(),
                                                }),
                                                meta: None,
                                            });
                                        }
                                        {
                                            let mut cache = extra_cache.borrow_mut();
                                            *cache = extra.clone();
                                        }
                                        // Merge built-ins + cached extra
                                        let mut cmds = Self::built_in_commands();
                                        cmds.extend(extra);
                                        let (tx, rx) = oneshot::channel();
                                        let _ = tx_updates.send((
                                            SessionNotification {
                                                session_id: session_id_for_update,
                                                update: SessionUpdate::AvailableCommandsUpdate {
                                                    available_commands: cmds,
                                                },
                                                meta: None,
                                            },
                                            tx,
                                        ));
                                        let _ = rx.await;
                                        break;
                                    }
                                    EventMsg::Error(_) => break,
                                    _ => {}
                                }
                            }
                            Ok(_) => continue,
                            Err(_) => break,
                        }
                    }
                });
            }
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

        let missing = { !self.sessions.borrow().contains_key(&sid_str) };
        if missing {
            // Try to start a Codex conversation for this restored session
            let codex_config = self.config.clone();

            let (conversation_id, conversation_opt) = match self
                .conversation_manager
                .new_conversation(codex_config)
                .await
            {
                Ok(NewConversation {
                    conversation_id,
                    conversation,
                    session_configured: _,
                }) => (conversation_id, Some(conversation)),
                Err(e) => {
                    return Err(Error::into_internal_error(e));
                }
            };

            // Track the session
            self.sessions.borrow_mut().insert(
                sid_str.clone(),
                SessionState {
                    created: SystemTime::now(),
                    conversation_id: conversation_id.to_string(),
                    conversation: conversation_opt,
                    current_approval: AskForApproval::OnRequest,
                    current_sandbox: SandboxPolicy::new_workspace_write_policy(),
                    token_usage: None,
                },
            );

            // Immediately advertise available commands to the client
            {
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
            }

            // Discover custom prompts and refresh available commands
            {
                let sid = sid_str.clone();
                let tx_updates = self.session_update_tx.clone();
                let submit_seq = self.next_submit_seq.get();
                self.next_submit_seq.set(submit_seq + 1);
                let submit_id = format!("s{}-{}", sid, submit_seq);
                let session_map = self.sessions.borrow();
                let extra_cache = self.extra_available_commands.clone();
                if let Some(state) = session_map.get(&sid) {
                    let conversation = state.conversation.clone();
                    let session_id_for_update = args.session_id.clone();
                    task::spawn_local(async move {
                        let Some(conversation) = conversation else {
                            return;
                        };
                        let _ = conversation
                            .submit_with_id(Submission {
                                id: submit_id.clone(),
                                op: Op::ListCustomPrompts,
                            })
                            .await;
                        loop {
                            match conversation.next_event().await {
                                Ok(event) if event.id == submit_id => match event.msg {
                                    EventMsg::ListCustomPromptsResponse(resp) => {
                                        let mut extra: Vec<AvailableCommand> = Vec::new();
                                        for p in resp.custom_prompts {
                                            let desc =
                                                format!("custom prompt ({})", p.path.display());
                                            extra.push(AvailableCommand {
                                                name: p.name,
                                                description: desc,
                                                input: Some(AvailableCommandInput::Unstructured {
                                                    hint: "Additional input (optional)".into(),
                                                }),
                                                meta: None,
                                            });
                                        }
                                        {
                                            let mut cache = extra_cache.borrow_mut();
                                            *cache = extra.clone();
                                        }
                                        let mut cmds = Self::built_in_commands();
                                        cmds.extend(extra.into_iter());
                                        let (tx, rx) = oneshot::channel();
                                        let _ = tx_updates.send((
                                            SessionNotification {
                                                session_id: session_id_for_update,
                                                update: SessionUpdate::AvailableCommandsUpdate {
                                                    available_commands: cmds,
                                                },
                                                meta: None,
                                            },
                                            tx,
                                        ));
                                        let _ = rx.await;
                                        break;
                                    }
                                    EventMsg::Error(_) => break,
                                    _ => {}
                                },
                                Ok(_) => continue,
                                Err(_) => break,
                            }
                        }
                    });
                }
            }
        } else {
            // Even if the session exists, re-emit available commands
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
        }

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

        // Notify client about the new current mode immediately
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
        let session = self
            .sessions
            .borrow()
            .get(&sid_str)
            .cloned()
            .ok_or_else(Error::invalid_params)?;

        // Handle slash commands
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

        // Ensure we have a Codex conversation for non-slash content
        if self
            .sessions
            .borrow()
            .get(&sid_str)
            .and_then(|s| s.conversation.as_ref())
            .is_none()
        {
            let msg = "No Codex backend available. Use slash commands like /status";
            let (tx, rx) = oneshot::channel();
            self.send_message_chunk(&args.session_id, msg.into(), tx)?;
            let _ = rx.await;
            return Ok(PromptResponse {
                stop_reason: StopReason::EndTurn,
                meta: None,
            });
        }
        let conversation = self
            .sessions
            .borrow()
            .get(&sid_str)
            .and_then(|s| s.conversation.clone())
            .unwrap();

        // Build user input submission items from prompt content blocks
        let mut items: Vec<InputItem> = Vec::new();
        for block in &args.prompt {
            match block {
                ContentBlock::Text(t) => {
                    items.push(InputItem::Text {
                        text: t.text.clone(),
                    });
                }
                ContentBlock::Image(img) => {
                    let url = format!("data:{};base64,{}", img.mime_type, img.data);
                    items.push(InputItem::Image { image_url: url });
                }
                ContentBlock::Audio(_a) => {
                    // Not supported by Codex input yet; skip.
                }
                ContentBlock::Resource(res) => {
                    if let EmbeddedResourceResource::TextResourceContents(trc) = &res.resource {
                        items.push(InputItem::Text {
                            text: trc.text.clone(),
                        });
                    }
                }
                ContentBlock::ResourceLink(link) => {
                    items.push(InputItem::Text {
                        text: format!("Resource: {}", link.uri),
                    });
                }
            }
        }
        let submit_id = format!("s{}-{}", sid_str, self.next_submit_seq.get());
        self.next_submit_seq.set(self.next_submit_seq.get() + 1);

        let submission = Submission {
            id: submit_id.clone(),
            op: Op::UserInput { items },
        };

        // Enqueue work and then stream corresponding events back as ACP updates
        conversation
            .submit_with_id(submission)
            .await
            .map_err(Error::into_internal_error)?;

        let pos = Arc::new(vec![
            PermissionOption {
                id: PermissionOptionId("approve_for_session".into()),
                name: "Approve for Session".into(),
                kind: PermissionOptionKind::AllowAlways,
                meta: None,
            },
            PermissionOption {
                id: PermissionOptionId("approve".into()),
                name: "Approve".into(),
                kind: PermissionOptionKind::AllowOnce,
                meta: None,
            },
            PermissionOption {
                id: PermissionOptionId("deny".into()),
                name: "Deny".into(),
                kind: PermissionOptionKind::RejectOnce,
                meta: None,
            },
        ]);

        loop {
            let event = conversation
                .next_event()
                .await
                .map_err(Error::into_internal_error)?;
            if event.id != submit_id {
                continue;
            }

            match event.msg {
                EventMsg::AgentMessageDelta(delta) => {
                    let (tx, rx) = oneshot::channel();
                    self.send_message_chunk(&args.session_id, delta.delta.into(), tx)?;
                    rx.await.map_err(Error::into_internal_error)?;
                }
                EventMsg::AgentMessage(msg) => {
                    let (tx, rx) = oneshot::channel();
                    self.send_message_chunk(&args.session_id, msg.message.into(), tx)?;
                    rx.await.map_err(Error::into_internal_error)?;
                }
                EventMsg::AgentReasoningDelta(delta) => {
                    let (tx, rx) = oneshot::channel();
                    self.send_message_chunk(&args.session_id, delta.delta.into(), tx)?;
                    rx.await.map_err(Error::into_internal_error)?;
                }
                EventMsg::AgentReasoning(reason) => {
                    let (tx, rx) = oneshot::channel();
                    self.send_message_chunk(&args.session_id, reason.text.into(), tx)?;
                    rx.await.map_err(Error::into_internal_error)?;
                }
                // MCP tool calls → ACP ToolCall/ToolCallUpdate
                EventMsg::McpToolCallBegin(begin) => {
                    let title = format!("{}.{}", begin.invocation.server, begin.invocation.tool);
                    let tool = ToolCall {
                        id: ToolCallId(begin.call_id.clone().into()),
                        title,
                        kind: ToolKind::Fetch,
                        status: ToolCallStatus::InProgress,
                        content: Vec::new(),
                        locations: Vec::new(),
                        raw_input: begin.invocation.arguments,
                        raw_output: None,
                        meta: None,
                    };
                    let (tx, rx) = oneshot::channel();
                    self.session_update_tx
                        .send((
                            SessionNotification {
                                session_id: args.session_id.clone(),
                                update: SessionUpdate::ToolCall(tool),
                                meta: None,
                            },
                            tx,
                        ))
                        .map_err(Error::into_internal_error)?;
                    let _ = rx.await;
                }
                EventMsg::McpToolCallEnd(end) => {
                    let status = if end.is_success() {
                        ToolCallStatus::Completed
                    } else {
                        ToolCallStatus::Failed
                    };
                    let raw_output = serde_json::to_value(&end.result).ok();
                    let update = ToolCallUpdate {
                        id: ToolCallId(end.call_id.clone().into()),
                        fields: ToolCallUpdateFields {
                            status: Some(status),
                            title: Some(format!(
                                "{}.{}",
                                end.invocation.server, end.invocation.tool
                            )),
                            raw_output,
                            ..Default::default()
                        },
                        meta: None,
                    };
                    let (tx, rx) = oneshot::channel();
                    self.session_update_tx
                        .send((
                            SessionNotification {
                                session_id: args.session_id.clone(),
                                update: SessionUpdate::ToolCallUpdate(update),
                                meta: None,
                            },
                            tx,
                        ))
                        .map_err(Error::into_internal_error)?;
                    let _ = rx.await;
                }
                // Exec command begin/end → ACP ToolCall/ToolCallUpdate
                EventMsg::ExecCommandBegin(beg) => {
                    let title = beg.command.join(" ");
                    let loc = ToolCallLocation {
                        path: beg.cwd.clone(),
                        line: None,
                        meta: None,
                    };
                    let tool = ToolCall {
                        id: ToolCallId(beg.call_id.clone().into()),
                        title,
                        kind: ToolKind::Execute,
                        status: ToolCallStatus::InProgress,
                        content: Vec::new(),
                        locations: vec![loc],
                        raw_input: Some(json!({"command": beg.command, "cwd": beg.cwd})),
                        raw_output: None,
                        meta: None,
                    };
                    let (tx, rx) = oneshot::channel();
                    self.session_update_tx
                        .send((
                            SessionNotification {
                                session_id: args.session_id.clone(),
                                update: SessionUpdate::ToolCall(tool),
                                meta: None,
                            },
                            tx,
                        ))
                        .map_err(Error::into_internal_error)?;
                    let _ = rx.await;
                }
                EventMsg::ExecCommandEnd(end) => {
                    let status = if end.exit_code == 0 {
                        ToolCallStatus::Completed
                    } else {
                        ToolCallStatus::Failed
                    };

                    let mut content: Vec<ToolCallContent> = Vec::new();
                    if !end.aggregated_output.is_empty() {
                        content.push(ToolCallContent::from(end.aggregated_output.clone()));
                    } else if !end.stdout.is_empty() || !end.stderr.is_empty() {
                        let merged = if !end.stderr.is_empty() {
                            format!("{}\n{}", end.stdout, end.stderr)
                        } else {
                            end.stdout.clone()
                        };
                        if !merged.is_empty() {
                            content.push(ToolCallContent::from(merged));
                        }
                    }

                    let update = ToolCallUpdate {
                        id: ToolCallId(end.call_id.clone().into()),
                        fields: ToolCallUpdateFields {
                            status: Some(status),
                            content: if content.is_empty() {
                                None
                            } else {
                                Some(content)
                            },
                            raw_output: Some(json!({
                                "exit_code": end.exit_code,
                                "duration_ms": end.duration.as_millis(),
                                "formatted_output": end.formatted_output,
                            })),
                            ..Default::default()
                        },
                        meta: None,
                    };
                    let (tx, rx) = oneshot::channel();
                    self.session_update_tx
                        .send((
                            SessionNotification {
                                session_id: args.session_id.clone(),
                                update: SessionUpdate::ToolCallUpdate(update),
                                meta: None,
                            },
                            tx,
                        ))
                        .map_err(Error::into_internal_error)?;
                    let _ = rx.await;
                }
                EventMsg::ExecApprovalRequest(req) => {
                    // Build a ToolCallUpdate describing the pending exec
                    let title = format!("`{}`", req.command.join(" "));
                    let update = ToolCallUpdate {
                        id: ToolCallId(req.call_id.clone().into()),
                        fields: ToolCallUpdateFields {
                            kind: Some(ToolKind::Execute),
                            status: Some(ToolCallStatus::Pending),
                            title: Some(title),
                            locations: Some(vec![ToolCallLocation {
                                path: req.cwd.clone(),
                                line: None,
                                meta: None,
                            }]),
                            ..Default::default()
                        },
                        meta: None,
                    };

                    let reqp = RequestPermissionRequest {
                        session_id: args.session_id.clone(),
                        tool_call: update,
                        options: pos.as_ref().clone(),
                        meta: None,
                    };
                    let (txp, rxp) = oneshot::channel();
                    let _ = self.client_tx.send(ClientOp::RequestPermission(reqp, txp));
                    let outcome = rxp.await.map_err(|_| Error::internal_error())?;
                    if let Ok(resp) = outcome {
                        let decision = self.handle_response_outcome(resp);
                        // Send ExecApproval back to Codex
                        let approval_submit_id =
                            format!("perm-{}-{}", sid_str, self.next_submit_seq.get());
                        self.next_submit_seq.set(self.next_submit_seq.get() + 1);
                        if let Some(conv) = session.conversation.as_ref() {
                            conv.submit_with_id(Submission {
                                id: approval_submit_id,
                                op: Op::ExecApproval {
                                    id: event.id.clone(),
                                    decision,
                                },
                            })
                            .await
                            .map_err(Error::into_internal_error)?;
                        } else {
                            warn!("Dev mock mode: ExecApproval ignored (no backend)");
                        }
                    }
                }
                EventMsg::ApplyPatchApprovalRequest(req) => {
                    // Summarize patch as content lines
                    let mut lines = Vec::new();
                    for (path, change) in req.changes.iter() {
                        use codex_core::protocol::FileChange as FC;
                        let s = match change {
                            FC::Add { .. } => format!("Add {}", path.display()),
                            FC::Delete { .. } => format!("Delete {}", path.display()),
                            FC::Update { .. } => format!("Update {}", path.display()),
                        };
                        lines.push(s);
                    }
                    let title = if req.changes.len() == 1 {
                        lines
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "Apply changes".into())
                    } else {
                        format!("Edit {} files", req.changes.len())
                    };
                    let update = ToolCallUpdate {
                        id: ToolCallId(req.call_id.clone().into()),
                        fields: ToolCallUpdateFields {
                            kind: Some(ToolKind::Edit),
                            status: Some(ToolCallStatus::Pending),
                            title: Some(title),
                            content: if lines.is_empty() {
                                None
                            } else {
                                Some(vec![ToolCallContent::from(lines.join("\n"))])
                            },
                            ..Default::default()
                        },
                        meta: None,
                    };

                    let reqp = RequestPermissionRequest {
                        session_id: args.session_id.clone(),
                        tool_call: update,
                        options: pos.as_ref().clone(),
                        meta: None,
                    };
                    let (txp, rxp) = oneshot::channel();
                    let _ = self.client_tx.send(ClientOp::RequestPermission(reqp, txp));
                    let outcome = rxp.await.map_err(|_| Error::internal_error())?;
                    if let Ok(resp) = outcome {
                        let decision = self.handle_response_outcome(resp);
                        let approval_submit_id =
                            format!("perm-{}-{}", sid_str, self.next_submit_seq.get());
                        self.next_submit_seq.set(self.next_submit_seq.get() + 1);
                        if let Some(conv) = session.conversation.as_ref() {
                            conv.submit_with_id(Submission {
                                id: approval_submit_id,
                                op: Op::PatchApproval {
                                    id: event.id.clone(),
                                    decision,
                                },
                            })
                            .await
                            .map_err(Error::into_internal_error)?;
                        } else {
                            warn!("Dev mock mode: PatchApproval ignored (no backend)");
                        }
                    }
                }
                EventMsg::TokenCount(tc) => {
                    if let Some(info) = tc.info
                        && let Ok(mut map) = self.sessions.try_borrow_mut()
                        && let Some(state) = map.get_mut(&sid_str)
                    {
                        state.token_usage = Some(info.total_token_usage.clone());
                    }
                }
                EventMsg::TaskComplete(_) => {
                    break;
                }
                EventMsg::Error(err) => {
                    let (tx, rx) = oneshot::channel();
                    self.send_message_chunk(&args.session_id, err.message.into(), tx)?;
                    let _ = rx.await;
                    break;
                }
                // Ignore other events for now
                _ => {}
            }
        }

        Ok(PromptResponse {
            stop_reason: StopReason::EndTurn,
            meta: None,
        })
    }

    async fn cancel(&self, args: CancelNotification) -> Result<(), Error> {
        info!(?args, "Received cancel request");
        let sid_str = args.session_id.0.to_string();

        // If we have an active Codex conversation, forward an interrupt
        let conv_opt = {
            let sessions = self.sessions.borrow();
            sessions.get(&sid_str).and_then(|s| s.conversation.clone())
        };
        if let Some(conv) = conv_opt {
            // Best-effort: we don't need the submission id here
            let _ = conv.submit(Op::Interrupt).await;
        } else {
            return Err(Error::invalid_params());
        }
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