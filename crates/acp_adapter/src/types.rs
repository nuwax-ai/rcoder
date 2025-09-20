//! ACP 适配器的通用类型定义
//!
//! 基于 agent_client_protocol crate 的类型重新导出和扩展

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;
use tokio::sync::oneshot;

// 重新导出 agent_client_protocol 中的核心类型
pub use agent_client_protocol::{
    SessionId, StopReason, Error, ErrorCode, ContentBlock,
    PromptRequest, PromptResponse, ToolCall, ToolCallContent,
    ToolCallStatus, SessionUpdate, PermissionOption, PermissionOptionKind,
    PermissionOptionId, RequestPermissionRequest, RequestPermissionResponse,
    ReadTextFileRequest, ReadTextFileResponse, WriteTextFileRequest,
    WriteTextFileResponse, ClientCapabilities, ProtocolVersion,
    SessionMode, SessionModeId, SessionModeState,
};

/// 用户消息 ID - 扩展类型
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct UserMessageId(Arc<str>);

impl UserMessageId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string().into())
    }

    pub fn from_string(s: String) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for UserMessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 权限请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub description: String,
    pub arguments: serde_json::Value,
}

/// 权限响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PermissionResponse {
    Allow { option_id: PermissionOptionId },
    Deny,
}

/// 工具调用信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallInfo {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: serde_json::Value,
    pub state: ToolCallState,
    pub timestamp: std::time::SystemTime,
}

/// 工具调用状态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolCallState {
    PendingAuthorization,
    Authorized,
    Rejected,
    InProgress,
    Completed,
    Failed(String),
    Canceled,
}

/// 工具调用 ID - 扩展类型
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct ToolCallId(Arc<str>);

impl ToolCallId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string().into())
    }

    pub fn from_string(s: String) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<agent_client_protocol::ToolCallId> for ToolCallId {
    fn from(protocol_id: agent_client_protocol::ToolCallId) -> Self {
        Self(format!("{}", protocol_id.0).into())
    }
}

/// 会话状态 - 扩展 agent_client_protocol 的状态
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    /// 初始化中
    Initializing,
    /// 已连接
    Connected,
    /// 正在处理提示
    Prompting,
    /// 已暂停
    Paused,
    /// 已关闭
    Closed,
    /// 错误状态
    Error(String),
}

/// 会话消息 - 内部消息类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionMessage {
    /// 用户消息
    User {
        id: UserMessageId,
        content: String,
        timestamp: std::time::SystemTime,
    },
    /// 助手消息
    Assistant {
        content_blocks: Vec<ContentBlock>,
        timestamp: std::time::SystemTime,
        tool_calls: Vec<ToolCall>,
    },
    /// 系统消息
    System {
        content: String,
        timestamp: std::time::SystemTime,
    },
    /// 工具调用结果
    ToolCallResult {
        tool_call_id: ToolCallId,
        result: serde_json::Value,
        timestamp: std::time::SystemTime,
    },
    /// 状态更新
    Status {
        state: SessionState,
        message: Option<String>,
        timestamp: std::time::SystemTime,
    },
}

/// MCP 服务器配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub working_dir: Option<PathBuf>,
    pub timeout_seconds: Option<u64>,
}

/// 环境配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentConfig {
    pub vars: HashMap<String, String>,
    pub working_dir: Option<PathBuf>,
    pub path_extensions: Vec<PathBuf>,
}

/// 流式更新事件 - 基于 Zed 的 SessionUpdate 实现
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamUpdate {
    /// 用户消息分块
    UserMessageChunk {
        session_id: SessionId,
        content: String,
    },
    /// 助手消息分块
    AgentMessageChunk {
        session_id: SessionId,
        content: String,
    },
    /// 助手思考过程分块
    AgentThoughtChunk {
        session_id: SessionId,
        content: String,
    },
    /// 工具调用
    ToolCall {
        session_id: SessionId,
        tool_call: ToolCall,
    },
    /// 工具调用更新
    ToolCallUpdate {
        session_id: SessionId,
        tool_call_update: ToolCall,
    },
    /// 会话状态改变
    SessionStateChanged {
        session_id: SessionId,
        new_state: SessionState,
        message: Option<String>,
    },
    /// 提示开始
    PromptStarted {
        session_id: SessionId,
        prompt: String,
    },
    /// 工具调用开始
    ToolCallStarted {
        session_id: SessionId,
        tool_call_id: ToolCallId,
        tool_name: String,
    },
    /// 计划更新
    Plan {
        session_id: SessionId,
        plan: serde_json::Value,
    },
    /// 可用命令更新
    AvailableCommandsUpdate {
        session_id: SessionId,
        available_commands: Vec<serde_json::Value>,
    },
    /// 当前模式更新
    CurrentModeUpdate {
        session_id: SessionId,
        current_mode_id: SessionModeId,
    },
    /// 处理完成
    PromptCompleted {
        session_id: SessionId,
        stop_reason: StopReason,
    },
    /// 错误
    Error {
        session_id: SessionId,
        error: String,
    },
}

/// 进程配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub working_dir: Option<PathBuf>,
    pub timeout_seconds: Option<u64>,
    pub restart_on_failure: bool,
    pub max_restarts: Option<u32>,
}

/// 连接配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub max_retries: u32,
    pub retry_delay_seconds: u64,
    pub timeout_seconds: u64,
    pub heartbeat_interval_seconds: u64,
    pub buffer_size: usize,
}

/// 会话配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    pub max_history_messages: usize,
    pub timeout_seconds: u64,
    pub auto_save: bool,
    pub save_path: Option<PathBuf>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_history_messages: 100,
            timeout_seconds: 300,
            auto_save: false,
            save_path: None,
        }
    }
}

/// 会话统计
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStats {
    pub total_messages: usize,
    pub total_tool_calls: usize,
    pub start_time: std::time::SystemTime,
    pub last_activity: std::time::SystemTime,
    pub tokens_used: Option<u64>,
}

/// 工具定义
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// 连接状态
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
    Failed(String),
}

/// 连接统计
#[derive(Debug, Clone)]
pub struct ConnectionStats {
    pub messages_sent: u64,
    pub messages_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub last_activity: std::time::SystemTime,
    pub connection_attempts: u32,
    pub failed_attempts: u32,
}

impl Default for ConnectionStats {
    fn default() -> Self {
        Self {
            messages_sent: 0,
            messages_received: 0,
            bytes_sent: 0,
            bytes_received: 0,
            last_activity: std::time::SystemTime::UNIX_EPOCH,
            connection_attempts: 0,
            failed_attempts: 0,
        }
    }
}

/// 消息条目类型 - 参考 Zed 的 AgentThreadEntry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentThreadEntry {
    UserMessage(UserMessage),
    AssistantMessage(AssistantMessage),
    ToolCall(ToolCall),
}

/// 用户消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    pub id: UserMessageId,
    pub content: String,
    pub timestamp: std::time::SystemTime,
}

/// 助手消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content_blocks: Vec<ContentBlock>,
    pub timestamp: std::time::SystemTime,
    pub tool_calls: Vec<ToolCall>,
}

/// 工具调用状态 - 完整实现 Zed 的状态定义
#[derive(Debug)]
pub enum ExtendedToolCallStatus {
    /// 工具调用尚未开始运行，但已向用户显示
    Pending,
    /// 工具调用等待用户确认
    WaitingForConfirmation {
        options: Vec<PermissionOption>,
        respond_tx: Option<oneshot::Sender<PermissionOptionId>>,
    },
    /// 工具调用正在运行
    InProgress,
    /// 工具调用成功完成
    Completed,
    /// 工具调用失败
    Failed,
    /// 用户拒绝工具调用
    Rejected,
    /// 用户取消生成，工具调用被取消
    Canceled,
}

/// 可序列化的工具调用状态 - 用于存储和传输
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SerializableToolCallStatus {
    Pending,
    WaitingForConfirmation {
        options: Vec<PermissionOption>,
    },
    InProgress,
    Completed,
    Failed,
    Rejected,
    Canceled,
}

impl From<&ExtendedToolCallStatus> for SerializableToolCallStatus {
    fn from(status: &ExtendedToolCallStatus) -> Self {
        match status {
            ExtendedToolCallStatus::Pending => Self::Pending,
            ExtendedToolCallStatus::WaitingForConfirmation { options, .. } => {
                Self::WaitingForConfirmation {
                    options: options.clone(),
                }
            }
            ExtendedToolCallStatus::InProgress => Self::InProgress,
            ExtendedToolCallStatus::Completed => Self::Completed,
            ExtendedToolCallStatus::Failed => Self::Failed,
            ExtendedToolCallStatus::Rejected => Self::Rejected,
            ExtendedToolCallStatus::Canceled => Self::Canceled,
        }
    }
}

impl std::fmt::Display for ExtendedToolCallStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExtendedToolCallStatus::Pending => write!(f, "Pending"),
            ExtendedToolCallStatus::WaitingForConfirmation { .. } => write!(f, "Waiting for confirmation"),
            ExtendedToolCallStatus::InProgress => write!(f, "In Progress"),
            ExtendedToolCallStatus::Completed => write!(f, "Completed"),
            ExtendedToolCallStatus::Failed => write!(f, "Failed"),
            ExtendedToolCallStatus::Rejected => write!(f, "Rejected"),
            ExtendedToolCallStatus::Canceled => write!(f, "Canceled"),
        }
    }
}

impl From<ToolCallStatus> for ExtendedToolCallStatus {
    fn from(status: ToolCallStatus) -> Self {
        match status {
            ToolCallStatus::Pending => ExtendedToolCallStatus::Pending,
            ToolCallStatus::InProgress => ExtendedToolCallStatus::InProgress,
            ToolCallStatus::Completed => ExtendedToolCallStatus::Completed,
            ToolCallStatus::Failed => ExtendedToolCallStatus::Failed,
        }
    }
}

impl From<ToolCallState> for ExtendedToolCallStatus {
    fn from(state: ToolCallState) -> Self {
        match state {
            ToolCallState::PendingAuthorization => ExtendedToolCallStatus::Pending,
            ToolCallState::Authorized => ExtendedToolCallStatus::InProgress,
            ToolCallState::Rejected => ExtendedToolCallStatus::Rejected,
            ToolCallState::InProgress => ExtendedToolCallStatus::InProgress,
            ToolCallState::Completed => ExtendedToolCallStatus::Completed,
            ToolCallState::Failed(_) => ExtendedToolCallStatus::Failed,
            ToolCallState::Canceled => ExtendedToolCallStatus::Canceled,
        }
    }
}

/// 权限请求状态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequestState {
    pub tool_call_id: ToolCallId,
    pub options: Vec<PermissionOption>,
    pub created_at: std::time::SystemTime,
    pub expires_at: Option<std::time::SystemTime>,
}

/// 权限响应结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PermissionOutcome {
    Selected { option_id: PermissionOptionId },
    Cancelled,
    Expired,
}