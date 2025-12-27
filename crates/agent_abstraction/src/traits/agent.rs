//! Agent trait definition.

use crate::AgentConnection;
use crate::AgentStatus;
use crate::error::AgentAbstractionError;
use agent_client_protocol::{McpServer, SessionId};
use agent_config::AgentConfig;
use async_trait::async_trait;

/// Agent 启动配置
///
/// 包含启动 Agent 所需的额外配置信息，如系统提示词、MCP 服务器配置等。
/// 这些配置通过 ACP 协议的 NewSessionRequest 传递给 Agent。
#[derive(Debug, Clone)]
pub struct AgentStartConfig {
    /// 系统提示词（通过 meta.systemPrompt.append 传递）
    ///
    /// 使用 ACP 协议的 `_meta.systemPrompt.append` 模式，保留 claude_code preset 的基础能力，
    /// 同时追加自定义的系统提示词。
    pub system_prompt: Option<String>,

    /// MCP 服务器配置
    pub mcp_servers: Vec<McpServer>,

    /// 额外的 meta 字段
    ///
    /// 可以传递任何额外的配置信息给 Agent
    pub extra_meta: Option<serde_json::Map<String, serde_json::Value>>,

    /// 服务类型（用于加载对应的配置，必填）
    pub service_type: shared_types::ServiceType,

    /// 用于恢复会话的 session_id
    ///
    /// 当需要恢复之前的会话时，传入之前的 session_id，
    /// 这将通过 `_meta.claudeCode.options.resume` 传递给 Agent。
    pub resume_session_id: Option<String>,
}

impl AgentStartConfig {
    /// 创建新的 AgentStartConfig
    ///
    /// # 参数
    /// - `service_type`: 服务类型（必填）
    pub fn new(service_type: shared_types::ServiceType) -> Self {
        Self {
            system_prompt: None,
            mcp_servers: Vec::new(),
            extra_meta: None,
            service_type,
            resume_session_id: None,
        }
    }

    /// 设置系统提示词
    pub fn with_system_prompt(mut self, system_prompt: String) -> Self {
        self.system_prompt = Some(system_prompt);
        self
    }

    /// 设置 MCP 服务器配置
    pub fn with_mcp_servers(mut self, mcp_servers: Vec<McpServer>) -> Self {
        self.mcp_servers = mcp_servers;
        self
    }

    /// 设置额外的 meta 字段
    pub fn with_extra_meta(
        mut self,
        extra_meta: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        self.extra_meta = Some(extra_meta);
        self
    }

    /// 设置服务类型
    pub fn with_service_type(mut self, service_type: shared_types::ServiceType) -> Self {
        self.service_type = service_type;
        self
    }

    /// 设置用于恢复会话的 session_id
    pub fn with_resume_session_id(mut self, session_id: String) -> Self {
        self.resume_session_id = Some(session_id);
        self
    }

    /// 构建 NewSessionRequest 的 meta 字段
    ///
    /// 将系统提示词和额外的 meta 字段合并为一个 JSON Map，
    /// 用于传递给 ACP 协议的 NewSessionRequest。
    ///
    /// # 返回值
    /// 返回包含 `systemPrompt: { append: "..." }` 的 Meta 对象
    pub fn build_meta(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut meta = serde_json::Map::new();

        // 添加系统提示词（使用 append 模式）
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

        // 添加 session_id 到 resume，用于恢复会话
        // 参考 agent 端的 TypeScript 代码:
        // resume: (params._meta as NewSessionMeta | undefined)?.claudeCode?.options?.resume
        if let Some(ref session_id) = self.resume_session_id {
            // 构建 claudeCode.options.resume 结构
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

        // 合并额外的 meta 字段
        if let Some(ref extra) = self.extra_meta {
            for (key, value) in extra {
                // 如果键已存在（如 systemPrompt），不覆盖
                if !meta.contains_key(key) {
                    meta.insert(key.clone(), value.clone());
                }
            }
        }

        meta
    }

    /// 检查是否有系统提示词
    pub fn has_system_prompt(&self) -> bool {
        self.system_prompt.is_some()
    }

    /// 检查是否有 MCP 服务器配置
    pub fn has_mcp_servers(&self) -> bool {
        !self.mcp_servers.is_empty()
    }

    /// 构建不包含 resume 参数的 meta
    ///
    /// 当 list_sessions 检查发现会话不存在时使用。
    /// 与 build_meta() 类似，但不包含 claudeCode.options.resume 字段。
    ///
    /// # 使用场景
    /// - 用户传入了 session_id，但通过 list_sessions API 验证发现该会话不存在
    /// - 此时创建新会话，但保留其他配置（系统提示词、extra_meta 等）
    pub fn build_meta_without_resume(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut meta = serde_json::Map::new();

        // 只添加系统提示词（使用 append 模式，与 build_meta 保持一致）
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

        // 合并额外的 meta 字段（不覆盖已存在的键）
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

/// Agent 通用的 Prompt 消息
///
/// 这是 Agent 抽象层的核心数据结构，只包含 Agent 执行任务所需的核心信息。
/// 不包含业务层配置（如 model_provider、mcp 配置等）。
#[derive(Debug, Clone)]
pub struct PromptMessage {
    /// 用户输入的主要 prompt 文本
    pub content: String,

    /// 项目 ID
    pub project_id: String,

    /// 项目工作目录路径
    pub project_path: std::path::PathBuf,

    /// 会话 ID（可选，如果为 None 则 Agent 会创建新会话）
    pub session_id: Option<String>,

    /// 请求追踪 ID
    pub request_id: String,

    /// 附件列表（支持文本、图像、音频、文档等）
    pub attachments: Vec<shared_types::Attachment>,

    /// 数据源附件（JSON 字符串数组）
    pub data_source_attachments: Vec<String>,

    /// 服务类型
    pub service_type: shared_types::ServiceType,

    // === 新增字段 (v2) ===
    /// 系统提示词覆盖
    ///
    /// 如果提供，将覆盖默认的系统提示词配置
    pub system_prompt_override: Option<String>,

    /// 用户提示词模板覆盖
    ///
    /// 如果提供，将使用此模板替换 `{user_prompt}` 变量
    pub user_prompt_template_override: Option<String>,

    /// Agent 运行时配置覆盖（MCP 服务器等）
    ///
    /// 包含 Agent 服务器配置和 MCP 服务器配置
    pub agent_config_override: Option<shared_types::ChatAgentConfig>,
}

impl PromptMessage {
    /// 创建新的 PromptMessage
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
            // 新增字段默认为 None
            system_prompt_override: None,
            user_prompt_template_override: None,
            agent_config_override: None,
        }
    }

    /// 设置会话 ID
    pub fn with_session_id(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }

    /// 设置附件列表
    pub fn with_attachments(mut self, attachments: Vec<shared_types::Attachment>) -> Self {
        self.attachments = attachments;
        self
    }

    /// 设置数据源附件
    pub fn with_data_source_attachments(mut self, data_source_attachments: Vec<String>) -> Self {
        self.data_source_attachments = data_source_attachments;
        self
    }

    /// 设置系统提示词覆盖
    pub fn with_system_prompt_override(mut self, system_prompt: Option<String>) -> Self {
        self.system_prompt_override = system_prompt;
        self
    }

    /// 设置用户提示词模板覆盖
    pub fn with_user_prompt_template_override(mut self, template: Option<String>) -> Self {
        self.user_prompt_template_override = template;
        self
    }

    /// 设置 Agent 配置覆盖
    pub fn with_agent_config_override(
        mut self,
        config: Option<shared_types::ChatAgentConfig>,
    ) -> Self {
        self.agent_config_override = config;
        self
    }
}

/// 从 ChatPrompt 转换为 PromptMessage
impl From<shared_types::ChatPrompt> for PromptMessage {
    fn from(chat_prompt: shared_types::ChatPrompt) -> Self {
        Self {
            content: chat_prompt.prompt,
            project_id: chat_prompt.project_id,
            project_path: chat_prompt.project_path,
            session_id: chat_prompt.session_id,
            request_id: chat_prompt.request_id.unwrap_or_else(|| {
                // 如果 ChatPrompt 中没有 request_id，生成一个
                uuid::Uuid::new_v4().to_string().replace("-", "")
            }),
            attachments: chat_prompt.attachments,
            data_source_attachments: chat_prompt.data_source_attachments,
            service_type: chat_prompt.service_type,
            // 新增字段映射
            system_prompt_override: chat_prompt.system_prompt_override,
            user_prompt_template_override: chat_prompt.user_prompt_template_override,
            agent_config_override: chat_prompt.agent_config_override,
        }
    }
}

/// Process launch information
#[derive(Debug, Clone)]
pub struct ProcessLaunchInfo {
    /// Agent ID
    pub id: String,
    /// Agent name
    pub name: String,
    /// Command to execute
    pub command: String,
    /// Command arguments
    pub args: Vec<String>,
    /// Working directory
    pub working_dir: std::path::PathBuf,
    /// Environment variables
    pub env: std::collections::HashMap<String, String>,
    /// Agent configuration
    pub config: agent_config::AgentConfig,
}

/// Agent trait for abstracting different agent types
#[async_trait(?Send)]
pub trait Agent: Send + Sync {
    /// Get the agent ID
    fn id(&self) -> &str;

    /// Get the agent name
    fn name(&self) -> &str;

    /// Get the agent configuration
    ///
    /// Returns the current agent configuration if available.
    /// This is useful for introspection and debugging.
    fn config(&self) -> Option<&AgentConfig>;

    /// Start the agent with a prompt
    async fn start(&self, prompt: PromptMessage) -> Result<AgentConnection, AgentAbstractionError>;

    /// Stop the agent by session ID
    async fn stop(&self, session_id: &SessionId) -> Result<(), AgentAbstractionError>;

    /// Get agent status by session ID
    async fn get_status(
        &self,
        session_id: &SessionId,
    ) -> Result<AgentStatus, AgentAbstractionError>;
}
