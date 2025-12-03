//! ACP 适配器的通用类型定义
//!
//! 基于 agent_client_protocol crate 的类型重新导出和扩展

use agent_client_protocol::{
    ContentBlock, PermissionOption, PermissionOptionId, SessionId, SessionModeId, StopReason,
    ToolCall, ToolCallStatus,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::oneshot;
use uuid::Uuid;

/// 用户消息 ID - 扩展类型
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct UserMessageId(Arc<str>);

impl Default for UserMessageId {
    fn default() -> Self {
        Self::new()
    }
}

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

impl Default for ToolCallId {
    fn default() -> Self {
        Self::new()
    }
}

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
    /// Plan更新事件
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
    WaitingForConfirmation { options: Vec<PermissionOption> },
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
            ExtendedToolCallStatus::WaitingForConfirmation { .. } => {
                write!(f, "Waiting for confirmation")
            }
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
            // 处理未来可能添加的新状态
            _ => ExtendedToolCallStatus::Failed,
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

// ==================== Plan 相关结构 ====================

/// Plan - 任务计划
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    /// Plan条目列表
    pub entries: Vec<PlanEntry>,
    /// Plan创建时间
    pub created_at: std::time::SystemTime,
    /// 最后更新时间
    pub updated_at: std::time::SystemTime,
    /// Plan标题
    pub title: Option<String>,
    /// Plan描述
    pub description: Option<String>,
    /// Plan类型/分类
    pub category: Option<String>,
    /// 总估计耗时（秒）
    pub total_estimated_duration: Option<u64>,
    /// 总实际耗时（秒）
    pub total_actual_duration: Option<u64>,
    /// Plan状态
    pub status: PlanStatus,
    /// 元数据
    pub meta: Option<serde_json::Value>,
}

/// Plan条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanEntry {
    /// 条目ID
    pub id: String,
    /// 任务内容（支持Markdown）
    pub content: String,
    /// 优先级
    pub priority: PlanEntryPriority,
    /// 状态
    pub status: PlanEntryStatus,
    /// 创建时间
    pub created_at: std::time::SystemTime,
    /// 更新时间
    pub updated_at: std::time::SystemTime,
    /// 开始时间
    pub started_at: Option<std::time::SystemTime>,
    /// 完成时间
    pub completed_at: Option<std::time::SystemTime>,
    /// 预计耗时（秒）
    pub estimated_duration: Option<u64>,
    /// 实际耗时（秒）
    pub actual_duration: Option<u64>,
    /// 标签
    pub tags: Vec<String>,
    /// 描述
    pub description: Option<String>,
    /// 依赖的条目ID列表
    pub dependencies: Vec<String>,
    /// 进度百分比（0-100）
    pub progress: Option<u8>,
    /// 元数据
    pub meta: Option<serde_json::Value>,
}

/// Plan条目优先级
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PlanEntryPriority {
    Low,
    Normal,
    High,
    Critical,
}

/// Plan条目状态
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PlanEntryStatus {
    /// 待执行
    Pending,
    /// 执行中
    InProgress,
    /// 已完成
    Completed,
    /// 已取消
    Cancelled,
    /// 失败
    Failed,
}

/// Plan状态
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PlanStatus {
    /// 未开始
    NotStarted,
    /// 进行中
    InProgress,
    /// 已完成
    Completed,
    /// 已暂停
    Paused,
    /// 已取消
    Cancelled,
    /// 部分失败
    PartiallyFailed,
}

/// Plan统计信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStats {
    /// 待执行数量
    pub pending: u32,
    /// 执行中数量
    pub in_progress: u32,
    /// 已完成数量
    pub completed: u32,
    /// 已取消数量
    pub cancelled: u32,
    /// 失败数量
    pub failed: u32,
    /// 总数量
    pub total: u32,
    /// 当前执行中的条目ID
    pub current_in_progress_entry: Option<String>,
}

impl Default for Plan {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            created_at: std::time::SystemTime::now(),
            updated_at: std::time::SystemTime::now(),
            title: None,
            description: None,
            category: None,
            total_estimated_duration: None,
            total_actual_duration: None,
            status: PlanStatus::NotStarted,
            meta: None,
        }
    }
}

impl Plan {
    /// 创建新的Plan
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 获取统计信息
    pub fn stats(&self) -> PlanStats {
        let mut stats = PlanStats {
            pending: 0,
            in_progress: 0,
            completed: 0,
            cancelled: 0,
            failed: 0,
            total: self.entries.len() as u32,
            current_in_progress_entry: None,
        };

        for entry in &self.entries {
            match entry.status {
                PlanEntryStatus::Pending => stats.pending += 1,
                PlanEntryStatus::InProgress => {
                    stats.in_progress += 1;
                    if stats.current_in_progress_entry.is_none() {
                        stats.current_in_progress_entry = Some(entry.id.clone());
                    }
                }
                PlanEntryStatus::Completed => stats.completed += 1,
                PlanEntryStatus::Cancelled => stats.cancelled += 1,
                PlanEntryStatus::Failed => stats.failed += 1,
            }
        }

        stats
    }

    /// 添加新条目
    pub fn add_entry(&mut self, content: String, priority: PlanEntryPriority) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now();

        let entry = PlanEntry {
            id: id.clone(),
            content,
            priority,
            status: PlanEntryStatus::Pending,
            created_at: now,
            updated_at: now,
            started_at: None,
            completed_at: None,
            estimated_duration: None,
            actual_duration: None,
            tags: Vec::new(),
            description: None,
            dependencies: Vec::new(),
            progress: Some(0),
            meta: None,
        };

        self.entries.push(entry);
        self.updated_at = now;

        id
    }

    /// 更新条目状态
    pub fn update_entry_status(&mut self, entry_id: &str, status: PlanEntryStatus) -> bool {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == entry_id) {
            entry.status = status;
            entry.updated_at = std::time::SystemTime::now();
            self.updated_at = entry.updated_at;
            true
        } else {
            false
        }
    }

    /// 获取条目
    pub fn get_entry(&self, entry_id: &str) -> Option<&PlanEntry> {
        self.entries.iter().find(|e| e.id == entry_id)
    }

    /// 移除已完成的条目
    pub fn clear_completed(&mut self) {
        self.entries
            .retain(|entry| entry.status != PlanEntryStatus::Completed);
        self.updated_at = std::time::SystemTime::now();
    }
}

impl PlanEntry {
    /// 创建新条目
    pub fn new(content: String, priority: PlanEntryPriority) -> Self {
        let now = std::time::SystemTime::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            content,
            priority,
            status: PlanEntryStatus::Pending,
            created_at: now,
            updated_at: now,
            started_at: None,
            completed_at: None,
            estimated_duration: None,
            actual_duration: None,
            tags: Vec::new(),
            description: None,
            dependencies: Vec::new(),
            progress: Some(0),
            meta: None,
        }
    }

    /// 标记为进行中
    pub fn mark_in_progress(&mut self) {
        self.status = PlanEntryStatus::InProgress;
        self.started_at = Some(std::time::SystemTime::now());
        self.updated_at = self.started_at.unwrap();
        self.progress = Some(0);
    }

    /// 标记为完成
    pub fn mark_completed(&mut self) {
        let now = std::time::SystemTime::now();
        self.status = PlanEntryStatus::Completed;
        self.completed_at = Some(now);
        self.updated_at = now;
        self.progress = Some(100);

        // 计算实际耗时
        if let Some(started) = self.started_at
            && let Ok(duration) = now.duration_since(started)
        {
            self.actual_duration = Some(duration.as_secs());
        }
    }

    /// 标记为失败
    pub fn mark_failed(&mut self) {
        let now = std::time::SystemTime::now();
        self.status = PlanEntryStatus::Failed;
        self.updated_at = now;

        // 计算实际耗时（即使失败了）
        if let Some(started) = self.started_at
            && let Ok(duration) = now.duration_since(started)
        {
            self.actual_duration = Some(duration.as_secs());
        }
    }

    /// 更新进度
    pub fn update_progress(&mut self, progress: u8) {
        self.progress = Some(progress.min(100));
        self.updated_at = std::time::SystemTime::now();
    }

    /// 添加标签
    pub fn add_tag(&mut self, tag: String) {
        if !self.tags.contains(&tag) {
            self.tags.push(tag);
            self.updated_at = std::time::SystemTime::now();
        }
    }

    /// 设置描述
    pub fn set_description(&mut self, description: String) {
        self.description = Some(description);
        self.updated_at = std::time::SystemTime::now();
    }

    /// 添加依赖
    pub fn add_dependency(&mut self, dependency_id: String) {
        if !self.dependencies.contains(&dependency_id) {
            self.dependencies.push(dependency_id);
            self.updated_at = std::time::SystemTime::now();
        }
    }

    /// 设置预计耗时
    pub fn set_estimated_duration(&mut self, duration_seconds: u64) {
        self.estimated_duration = Some(duration_seconds);
        self.updated_at = std::time::SystemTime::now();
    }

    /// 检查依赖是否满足
    pub fn are_dependencies_satisfied(&self, plan: &Plan) -> bool {
        self.dependencies.iter().all(|dep_id| {
            plan.entries
                .iter()
                .find(|entry| entry.id == *dep_id)
                .map(|entry| entry.status == PlanEntryStatus::Completed)
                .unwrap_or(false)
        })
    }

    /// 获取耗时（已完成的实际耗时或预计耗时）
    pub fn get_duration(&self) -> Option<u64> {
        self.actual_duration.or(self.estimated_duration)
    }
}
