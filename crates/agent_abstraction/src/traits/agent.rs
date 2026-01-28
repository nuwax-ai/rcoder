//! Agent trait definition.

// SACP type imports
use sacp::schema::McpServer;

/// Agent startup configuration
///
/// Contains additional configuration information required to start an Agent,
/// such as system prompts, MCP server configurations, etc.
/// These configurations are passed to the Agent via ACP protocol's NewSessionRequest.
#[derive(Debug, Clone)]
pub struct AgentStartConfig {
    /// System prompt (passed via meta.systemPrompt.append)
    ///
    /// Uses the `_meta.systemPrompt.append` mode of the ACP protocol to preserve
    /// the base capabilities of the claude_code preset while appending custom system prompts.
    pub system_prompt: Option<String>,

    /// MCP server configuration
    pub mcp_servers: Vec<McpServer>,

    /// Additional meta fields
    ///
    /// Can pass any additional configuration information to the Agent
    pub extra_meta: Option<serde_json::Map<String, serde_json::Value>>,

    /// Service type (for loading corresponding config, required)
    pub service_type: shared_types::ServiceType,

    /// Session ID for resuming sessions
    ///
    /// When resuming a previous session, pass the previous session_id,
    /// which will be passed to the Agent via `_meta.claudeCode.options.resume`.
    pub resume_session_id: Option<String>,

    /// 🆕 Agent server config override (for custom Agent startup command)
    ///
    /// When the user request specifies agent_server config, use this config
    /// to override the default config. Includes command, args, env, etc.
    pub agent_server_override: Option<shared_types::ChatAgentServerConfig>,

    /// 🆕 ACP session creation timeout (seconds), default 100
    ///
    /// Maximum wait time for Agent to create a new session
    /// (may require more time when there are many MCP tools)
    pub acp_session_create_timeout_secs: Option<u64>,

    /// 🆕 Agent cancel call timeout (seconds), default 10
    ///
    /// Maximum wait time for Agent internal cancel operations
    pub agent_cancel_timeout_secs: Option<u64>,
}

impl AgentStartConfig {
    /// Create a new AgentStartConfig
    ///
    /// # Arguments
    /// - `service_type`: Service type (required)
    pub fn new(service_type: shared_types::ServiceType) -> Self {
        Self {
            system_prompt: None,
            mcp_servers: Vec::new(),
            extra_meta: None,
            service_type,
            resume_session_id: None,
            agent_server_override: None,
            acp_session_create_timeout_secs: None,
            agent_cancel_timeout_secs: None,
        }
    }

    /// Set system prompt
    pub fn with_system_prompt(mut self, system_prompt: String) -> Self {
        self.system_prompt = Some(system_prompt);
        self
    }

    /// Set MCP server configuration
    pub fn with_mcp_servers(mut self, mcp_servers: Vec<McpServer>) -> Self {
        self.mcp_servers = mcp_servers;
        self
    }

    /// Set additional meta fields
    pub fn with_extra_meta(
        mut self,
        extra_meta: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        self.extra_meta = Some(extra_meta);
        self
    }

    /// Set service type
    pub fn with_service_type(mut self, service_type: shared_types::ServiceType) -> Self {
        self.service_type = service_type;
        self
    }

    /// Set session ID for resuming sessions
    pub fn with_resume_session_id(mut self, session_id: String) -> Self {
        self.resume_session_id = Some(session_id);
        self
    }

    /// 🆕 Set Agent server config override
    pub fn with_agent_server_override(
        mut self,
        agent_server: shared_types::ChatAgentServerConfig,
    ) -> Self {
        self.agent_server_override = Some(agent_server);
        self
    }

    /// 🆕 Set ACP session creation timeout
    pub fn with_acp_session_create_timeout(mut self, timeout_secs: u64) -> Self {
        self.acp_session_create_timeout_secs = Some(timeout_secs);
        self
    }

    /// 🆕 Set Agent cancel call timeout
    pub fn with_agent_cancel_timeout(mut self, timeout_secs: u64) -> Self {
        self.agent_cancel_timeout_secs = Some(timeout_secs);
        self
    }

    /// Build the meta field for NewSessionRequest
    ///
    /// Merges the system prompt and additional meta fields into a JSON Map
    /// for passing to the ACP protocol's NewSessionRequest.
    ///
    /// # Returns
    /// Returns a Meta object containing `systemPrompt: { append: "..." }`
    pub fn build_meta(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut meta = serde_json::Map::new();

        // Add system prompt (using append mode)
        if let Some(ref system_prompt) = self.system_prompt {
            let mut system_prompt_obj = serde_json::Map::new();
            system_prompt_obj.insert(
                "append".to_string(),
                serde_json::Value::String(system_prompt.clone()),
            );
            meta.insert(
                "systemPrompt".to_string(),
                serde_json::Value::Object(system_prompt_obj),
            );
        }

        // Add session_id to resume, for resuming sessions
        // Refer to the TypeScript code on the Agent side:
        // resume: (params._meta as NewSessionMeta | undefined)?.claudeCode?.options?.resume
        if let Some(ref session_id) = self.resume_session_id {
            // Build claudeCode.options.resume structure
            let mut options = serde_json::Map::new();
            options.insert(
                "resume".to_string(),
                serde_json::Value::String(session_id.clone()),
            );

            let mut claude_code = serde_json::Map::new();
            claude_code.insert("options".to_string(), serde_json::Value::Object(options));

            meta.insert(
                "claudeCode".to_string(),
                serde_json::Value::Object(claude_code),
            );
        }

        // Merge additional meta fields
        if let Some(ref extra) = self.extra_meta {
            for (key, value) in extra {
                // Don't overwrite if key already exists (e.g., systemPrompt)
                if !meta.contains_key(key) {
                    meta.insert(key.clone(), value.clone());
                }
            }
        }

        meta
    }

    /// Check if there is a system prompt
    pub fn has_system_prompt(&self) -> bool {
        self.system_prompt.is_some()
    }

    /// Check if there are MCP server configurations
    pub fn has_mcp_servers(&self) -> bool {
        !self.mcp_servers.is_empty()
    }

    /// Build meta without resume parameter
    ///
    /// Used when the list_sessions check finds the session does not exist.
    /// Similar to build_meta(), but without the claudeCode.options.resume field.
    ///
    /// # Use Cases
    /// - User passed a session_id, but verification via list_sessions API found it doesn't exist
    /// - Create a new session while preserving other configs (system prompt, extra_meta, etc.)
    pub fn build_meta_without_resume(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut meta = serde_json::Map::new();

        // Only add system prompt (using append mode, consistent with build_meta)
        if let Some(ref system_prompt) = self.system_prompt {
            let mut system_prompt_obj = serde_json::Map::new();
            system_prompt_obj.insert(
                "append".to_string(),
                serde_json::Value::String(system_prompt.clone()),
            );
            meta.insert(
                "systemPrompt".to_string(),
                serde_json::Value::Object(system_prompt_obj),
            );
        }

        // Merge additional meta fields (don't overwrite existing keys)
        if let Some(ref extra) = self.extra_meta {
            for (key, value) in extra {
                if !meta.contains_key(key) {
                    meta.insert(key.clone(), value.clone());
                }
            }
        }

        meta
    }
}

/// Agent common Prompt message
///
/// This is the core data structure of the Agent abstraction layer, containing only
/// the core information needed for the Agent to execute tasks.
/// Does not contain business layer configurations (such as model_provider, mcp configs, etc.).
#[derive(Debug, Clone)]
pub struct PromptMessage {
    /// Main prompt text entered by the user
    pub content: String,

    /// Project ID
    pub project_id: String,

    /// Project working directory path
    pub project_path: std::path::PathBuf,

    /// Session ID (optional, if None the Agent will create a new session)
    pub session_id: Option<String>,

    /// Request tracking ID
    pub request_id: String,

    /// Attachment list (supports text, images, audio, documents, etc.)
    pub attachments: Vec<shared_types::Attachment>,

    /// Data source attachments (JSON string array)
    pub data_source_attachments: Vec<String>,

    /// Service type
    pub service_type: shared_types::ServiceType,

    // === New fields (v2) ===
    /// System prompt override
    ///
    /// If provided, will override the default system prompt configuration
    pub system_prompt_override: Option<String>,

    /// User prompt template override
    ///
    /// If provided, will use this template to replace the `{user_prompt}` variable
    pub user_prompt_template_override: Option<String>,

    /// Agent runtime config override (MCP servers, etc.)
    ///
    /// Contains Agent server configuration and MCP server configuration
    pub agent_config_override: Option<shared_types::ChatAgentConfig>,
}

impl PromptMessage {
    /// Create a new PromptMessage
    pub fn new(
        content: String,
        project_id: String,
        project_path: std::path::PathBuf,
        request_id: String,
        service_type: shared_types::ServiceType,
    ) -> Self {
        Self {
            content,
            project_id,
            project_path,
            session_id: None,
            request_id,
            attachments: Vec::new(),
            data_source_attachments: Vec::new(),
            service_type,
            // New fields default to None
            system_prompt_override: None,
            user_prompt_template_override: None,
            agent_config_override: None,
        }
    }

    /// Set session ID
    pub fn with_session_id(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }

    /// Set attachment list
    pub fn with_attachments(mut self, attachments: Vec<shared_types::Attachment>) -> Self {
        self.attachments = attachments;
        self
    }

    /// Set data source attachments
    pub fn with_data_source_attachments(mut self, data_source_attachments: Vec<String>) -> Self {
        self.data_source_attachments = data_source_attachments;
        self
    }

    /// Set system prompt override
    pub fn with_system_prompt_override(mut self, system_prompt: Option<String>) -> Self {
        self.system_prompt_override = system_prompt;
        self
    }

    /// Set user prompt template override
    pub fn with_user_prompt_template_override(mut self, template: Option<String>) -> Self {
        self.user_prompt_template_override = template;
        self
    }

    /// Set Agent config override
    pub fn with_agent_config_override(
        mut self,
        config: Option<shared_types::ChatAgentConfig>,
    ) -> Self {
        self.agent_config_override = config;
        self
    }
}

/// Convert from ChatPrompt to PromptMessage
impl From<shared_types::ChatPrompt> for PromptMessage {
    fn from(chat_prompt: shared_types::ChatPrompt) -> Self {
        Self {
            content: chat_prompt.prompt,
            project_id: chat_prompt.project_id,
            project_path: chat_prompt.project_path,
            session_id: chat_prompt.session_id,
            request_id: chat_prompt.request_id.unwrap_or_else(|| {
                // Generate one if ChatPrompt doesn't have request_id
                uuid::Uuid::new_v4().to_string().replace("-", "")
            }),
            attachments: chat_prompt.attachments,
            data_source_attachments: chat_prompt.data_source_attachments,
            service_type: chat_prompt.service_type,
            // Map new fields
            system_prompt_override: chat_prompt.system_prompt_override,
            user_prompt_template_override: chat_prompt.user_prompt_template_override,
            agent_config_override: chat_prompt.agent_config_override,
        }
    }
}
