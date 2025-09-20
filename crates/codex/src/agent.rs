use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::SystemTime;

use acp_adapter::{AcpAdapter, AcpConfig, StreamUpdate};
use agent_client_protocol::{
    Agent, AgentCapabilities, AuthMethod, AuthMethodId, AuthenticateRequest, AuthenticateResponse,
    AvailableCommand, CancelNotification, ContentBlock,
    EmbeddedResourceResource, Error, ExtNotification, ExtRequest, ExtResponse, InitializeRequest,
    InitializeResponse, LoadSessionRequest, LoadSessionResponse, McpCapabilities,
    NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse,
    RequestPermissionRequest, RequestPermissionResponse, SessionId, SessionNotification, SessionUpdate,
    SetSessionModeRequest, SetSessionModeResponse, StopReason, V1,
};
use serde_json::json;
use tokio::sync::{mpsc, oneshot, oneshot::Sender};
use tokio::task;
use tracing::{info, warn};

mod commands;

/// Codex 配置
#[derive(Debug, Clone)]
pub struct CodexConfig {
    pub cwd: PathBuf,
    pub codex_home: PathBuf,
    pub model: String,
}
// Placeholder for per-session state. Holds the Codex session
// handle, its id (for status/reporting), and bookkeeping for streaming.
#[derive(Clone)]
struct SessionState {
    #[allow(dead_code)]
    created: SystemTime,
    // Conversation id string for display/logging purposes.
    conversation_id: String,
    token_usage: Option<u64>,
    acp_adapter: Option<Arc<AcpAdapter>>,
}

pub struct CodexAgent {
    session_update_tx: mpsc::UnboundedSender<(SessionNotification, Sender<()>)>,
    next_session_id: Cell<u64>,
    sessions: Rc<RefCell<HashMap<String, SessionState>>>,
    config: CodexConfig,
    next_submit_seq: Cell<u64>,
    extra_available_commands: Rc<RefCell<Vec<AvailableCommand>>>,
    client_tx: mpsc::UnboundedSender<ClientOp>,
}

impl CodexAgent {
    pub fn with_config(
        session_update_tx: mpsc::UnboundedSender<(SessionNotification, Sender<()>)>,
        client_tx: mpsc::UnboundedSender<ClientOp>,
        config: CodexConfig,
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

    /// 初始化 ACP 适配器
    async fn initialize_acp_adapter(&self) -> Result<Arc<AcpAdapter>, Error> {
        let acp_config = AcpConfig::codex()
            .with_working_dir(self.config.cwd.clone())
            .with_env("OPENAI_API_KEY".to_string(), std::env::var("OPENAI_API_KEY").unwrap_or_default())
            .with_env("CODEX_HOME".to_string(), self.config.codex_home.to_string_lossy().to_string());

        let adapter = Arc::new(AcpAdapter::new(acp_config));

        // 初始化适配器
        adapter.initialize().await
            .map_err(|e| Error::internal_error().with_data(format!("初始化 ACP 适配器失败: {}", e)))?;

        Ok(adapter)
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
}

#[derive(Debug)]
pub enum ClientOp {
    RequestPermission(
        RequestPermissionRequest,
        Sender<Result<RequestPermissionResponse, Error>>,
    ),
}

#[async_trait::async_trait(?Send)]
impl Agent for CodexAgent {
    async fn initialize(&self, args: InitializeRequest) -> Result<InitializeResponse, Error> {
        info!(?args, "Received initialize request");
        // Advertise supported auth methods. We surface both ChatGPT and API key.
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
                // For ChatGPT, check if we have a way to authenticate
                // In the new adapter approach, we'd rely on the adapter's auth
                return Ok(Default::default());
            }
            "apikey" => {
                // Check for OPENAI_API_KEY
                if std::env::var("OPENAI_API_KEY").is_ok() {
                    return Ok(Default::default());
                }
                Err(Error::auth_required().with_data("OPENAI_API_KEY not set"))
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

        // 生成唯一的会话 ID
        let conversation_id = uuid::Uuid::new_v4().to_string();

        // 初始化 ACP 适配器
        let acp_adapter = match self.initialize_acp_adapter().await {
            Ok(adapter) => Some(adapter),
            Err(e) => {
                warn!("Failed to initialize ACP adapter: {}", e);
                None
            }
        };

        // Track the session
        self.sessions.borrow_mut().insert(
            session_id.to_string(),
            SessionState {
                created: SystemTime::now(),
                conversation_id,
                token_usage: None,
                acp_adapter,
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
        // Ensure an entry exists for this session. If absent, create one similar to new_session.
        let sid_str = args.session_id.0.to_string();

        let missing = { !self.sessions.borrow().contains_key(&sid_str) };
        if missing {
            // 初始化 ACP 适配器
            let acp_adapter = match self.initialize_acp_adapter().await {
                Ok(adapter) => Some(adapter),
                Err(e) => {
                    warn!("Failed to initialize ACP adapter: {}", e);
                    None
                }
            };

            // Track the session
            self.sessions.borrow_mut().insert(
                sid_str.clone(),
                SessionState {
                    created: SystemTime::now(),
                    conversation_id: uuid::Uuid::new_v4().to_string(),
                    token_usage: None,
                    acp_adapter,
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
        } else {
            // Even if the session exists, re-emit available commands so the client UI can hydrate.
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
        // Validate session exists
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

        // 使用 ACP 适配器处理提示
        self.handle_codex_prompt_with_acp(&args.session_id, &args.prompt).await?;

        Ok(PromptResponse {
            stop_reason: StopReason::EndTurn,
            meta: None,
        })
    }

    async fn cancel(&self, args: CancelNotification) -> Result<(), Error> {
        info!(?args, "Received cancel request");
        let sid_str = args.session_id.0.to_string();

        // 检查会话是否存在
        let session_exists = {
            let sessions = self.sessions.borrow();
            sessions.contains_key(&sid_str)
        };

        if !session_exists {
            return Err(Error::invalid_params());
        }

        // 在 ACP 适配器中处理取消操作
        // 这里可以添加通过 ACP 适配器发送取消请求的逻辑
        info!("Cancel request for session {} acknowledged", sid_str);

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

impl CodexAgent {
    async fn handle_codex_prompt_with_acp(
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

        // 获取 ACP 适配器
        let sid_str = session_id.0.to_string();
        let adapter = {
            let sessions = self.sessions.borrow();
            let session = sessions.get(&sid_str)
                .ok_or_else(|| Error::internal_error().with_data("Session not found"))?;
            session.acp_adapter.clone()
                .ok_or_else(|| Error::internal_error().with_data("ACP adapter not available"))?
        };

        // 创建 ACP 会话
        let session_handle = adapter.create_session().await
            .map_err(|e| Error::internal_error().with_data(format!("创建会话失败: {}", e)))?;

        // 订阅流式更新
        let mut update_receiver = session_handle.subscribe_to_updates().await;

        // 创建任务来处理流式更新
        let _session_id_clone = session_id.clone();
        let session_update_tx = self.session_update_tx.clone();
        task::spawn_local(async move {
            while let Some(update) = update_receiver.recv().await {
                match update {
                    StreamUpdate::AgentMessageChunk { session_id, content } => {
                        let (tx, rx) = oneshot::channel();
                        let _ = session_update_tx.send((
                            SessionNotification {
                                session_id,
                                update: SessionUpdate::AgentMessageChunk {
                                    content: ContentBlock::Text(agent_client_protocol::TextContent {
                                        annotations: None,
                                        text: content,
                                        meta: None,
                                    })
                                },
                                meta: None,
                            },
                            tx,
                        ));
                        let _ = rx.await;
                    }
                    StreamUpdate::ToolCallStarted { session_id, tool_call_id: _, tool_name } => {
                        let (tx, rx) = oneshot::channel();
                        let _ = session_update_tx.send((
                            SessionNotification {
                                session_id,
                                update: SessionUpdate::AgentThoughtChunk {
                                    content: ContentBlock::Text(agent_client_protocol::TextContent {
                                        annotations: None,
                                        text: format!("开始调用工具: {}", tool_name),
                                        meta: None,
                                    })
                                },
                                meta: None,
                            },
                            tx,
                        ));
                        let _ = rx.await;
                    }
                    StreamUpdate::ToolCall { session_id, tool_call } => {
                        let (tx, rx) = oneshot::channel();
                        let _ = session_update_tx.send((
                            SessionNotification {
                                session_id,
                                update: SessionUpdate::ToolCall(tool_call),
                                meta: None,
                            },
                            tx,
                        ));
                        let _ = rx.await;
                    }
                    _ => {}
                }
            }
        });

        // 构建 ACP 请求
        let acp_request = PromptRequest {
            session_id: session_id.clone(),
            prompt: prompt.to_vec(),
            meta: None,
        };

        // 发送提示请求
        let response = session_handle.send_prompt(acp_request).await
            .map_err(|e| Error::internal_error().with_data(format!("发送提示失败: {}", e)))?;

        info!(?response.stop_reason, "提示处理完成");

        Ok(())
    }
}
