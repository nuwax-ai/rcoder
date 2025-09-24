//! 进度事件模块
//!
//! 提供进度事件的定义和管理功能

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 固定大小的循环缓冲区
#[derive(Debug)]
pub struct CircularBuffer<T> {
    buffer: Vec<Option<T>>,
    index: usize,
    count: usize,
}

impl<T: Clone> Clone for CircularBuffer<T> {
    fn clone(&self) -> Self {
        Self {
            buffer: self.buffer.clone(),
            index: self.index,
            count: self.count,
        }
    }
}

impl<T: Clone> CircularBuffer<T> {
    pub fn new(size: usize) -> Self {
        Self {
            buffer: vec![None; size],
            index: 0,
            count: 0,
        }
    }

    pub fn push(&mut self, item: T) {
        if self.buffer.is_empty() {
            return;
        }

        self.buffer[self.index] = Some(item);
        self.index = (self.index + 1) % self.buffer.len();
        self.count = std::cmp::min(self.count + 1, self.buffer.len());
    }

    pub fn to_vec(&self) -> Vec<T> {
        let mut result = Vec::with_capacity(self.count);

        if self.count == 0 {
            return result;
        }

        let start = if self.count >= self.buffer.len() {
            self.index
        } else {
            0
        };

        for i in 0..self.count {
            let pos = (start + i) % self.buffer.len();
            if let Some(ref item) = self.buffer[pos] {
                result.push(item.clone());
            }
        }

        result
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// 进度事件子类型
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressEventSubType {
    UserMessageChunk,
    AgentMessageChunk,
    AgentThoughtChunk,
    ToolCall,
    ToolCallUpdate,
    PlanUpdate,
    AvailableCommandsUpdate,
    CurrentModeUpdate,
    PromptCompleted,
    Error,
    FullUpdate,
    EntryStatusUpdate,
    EntryAdded,
    EntryRemoved,
    StatsUpdate,
    TaskStarted,
    TaskCompleted,
    TaskFailed,
    Executing,
    CommandOutput,
    KeepAlive,
    Message,
    Thought,
    ModeUpdate,
    Unknown,
}

/// 进度事件类型
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressEventType {
    /// 任务开始
    TaskStarted,
    /// 执行中
    Executing,
    /// 命令输出
    CommandOutput,
    /// 任务完成
    TaskCompleted,
    /// 任务失败
    TaskFailed,
    /// 保持连接
    KeepAlive,
    /// 消息事件
    Message,
    /// 思考事件
    Thought,
    /// 工具调用事件
    ToolCall,
    /// 工具调用更新
    ToolCallUpdate,
    /// 计划更新事件
    PlanUpdate,
    /// 模式更新事件
    ModeUpdate,
    /// 当前模式更新
    CurrentModeUpdate,
    /// 可用命令更新
    AvailableCommandsUpdate,
    /// 统计更新事件
    StatsUpdate,
    /// 错误事件
    Error,
    /// 未知类型
    Unknown,
}

impl From<&str> for ProgressEventType {
    fn from(s: &str) -> Self {
        match s {
            "task_started" => ProgressEventType::TaskStarted,
            "executing" => ProgressEventType::Executing,
            "command_output" => ProgressEventType::CommandOutput,
            "task_completed" => ProgressEventType::TaskCompleted,
            "task_failed" => ProgressEventType::TaskFailed,
            "keep_alive" => ProgressEventType::KeepAlive,
            "message" => ProgressEventType::Message,
            "thought" => ProgressEventType::Thought,
            "tool_call" => ProgressEventType::ToolCall,
            "tool_call_update" => ProgressEventType::ToolCallUpdate,
            "plan_update" => ProgressEventType::PlanUpdate,
            "mode_update" => ProgressEventType::ModeUpdate,
            "available_commands_update" => ProgressEventType::AvailableCommandsUpdate,
            "stats_update" => ProgressEventType::StatsUpdate,
            "error" => ProgressEventType::Error,
            _ => ProgressEventType::Unknown,
        }
    }
}

impl std::fmt::Display for ProgressEventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProgressEventType::TaskStarted => write!(f, "task_started"),
            ProgressEventType::Executing => write!(f, "executing"),
            ProgressEventType::CommandOutput => write!(f, "command_output"),
            ProgressEventType::TaskCompleted => write!(f, "task_completed"),
            ProgressEventType::TaskFailed => write!(f, "task_failed"),
            ProgressEventType::KeepAlive => write!(f, "keep_alive"),
            ProgressEventType::Message => write!(f, "message"),
            ProgressEventType::Thought => write!(f, "thought"),
            ProgressEventType::ToolCall => write!(f, "tool_call"),
            ProgressEventType::ToolCallUpdate => write!(f, "tool_call_update"),
            ProgressEventType::PlanUpdate => write!(f, "plan_update"),
            ProgressEventType::ModeUpdate => write!(f, "mode_update"),
            ProgressEventType::AvailableCommandsUpdate => write!(f, "available_commands_update"),
            ProgressEventType::CurrentModeUpdate => write!(f, "current_mode_update"),
            ProgressEventType::StatsUpdate => write!(f, "stats_update"),
            ProgressEventType::Error => write!(f, "error"),
            ProgressEventType::Unknown => write!(f, "unknown"),
        }
    }
}

impl From<&str> for ProgressEventSubType {
    fn from(s: &str) -> Self {
        match s {
            "user_message_chunk" => ProgressEventSubType::UserMessageChunk,
            "agent_message_chunk" => ProgressEventSubType::AgentMessageChunk,
            "agent_thought_chunk" => ProgressEventSubType::AgentThoughtChunk,
            "tool_call" => ProgressEventSubType::ToolCall,
            "tool_call_update" => ProgressEventSubType::ToolCallUpdate,
            "plan_update" => ProgressEventSubType::PlanUpdate,
            "available_commands_update" => ProgressEventSubType::AvailableCommandsUpdate,
            "current_mode_update" => ProgressEventSubType::CurrentModeUpdate,
            "prompt_completed" => ProgressEventSubType::PromptCompleted,
            "error" => ProgressEventSubType::Error,
            "full_update" => ProgressEventSubType::FullUpdate,
            "entry_status_update" => ProgressEventSubType::EntryStatusUpdate,
            "entry_added" => ProgressEventSubType::EntryAdded,
            "entry_removed" => ProgressEventSubType::EntryRemoved,
            "stats_update" => ProgressEventSubType::StatsUpdate,
            _ => ProgressEventSubType::Unknown,
        }
    }
}

/// 进度事件
#[derive(Debug, Clone, Serialize)]
pub struct ProgressEvent {
    /// 事件 ID
    pub id: String,
    /// 事件类型
    #[serde(rename = "type")]
    pub event_type: ProgressEventType,
    /// 事件子类型
    pub sub_type: ProgressEventSubType,
    /// 会话 ID
    pub session_id: String,
    /// 时间戳
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// 内容
    pub content: String,
    /// 元数据
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, serde_json::Value>,
    /// 进度
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
}

impl ProgressEvent {
    pub fn new(
        session_id: String,
        event_type: ProgressEventType,
        sub_type: ProgressEventSubType,
        content: String,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            event_type,
            sub_type,
            session_id,
            timestamp: chrono::Utc::now(),
            content,
            metadata: HashMap::new(),
            progress: None,
        }
    }

    pub fn with_metadata(mut self, key: String, value: serde_json::Value) -> Self {
        self.metadata.insert(key, value);
        self
    }

    pub fn with_progress(mut self, progress: f64) -> Self {
        self.progress = Some(progress);
        self
    }
}

/// Session 消息管理器 - 为每个 session_id 维护循环数组缓存
#[derive(Debug)]
pub struct SessionMessageManager {
    /// 消息缓存映射 (session_id -> 循环数组)
    message_cache: dashmap::DashMap<String, CircularBuffer<ProgressEvent>>,
    /// 缓存大小
    cache_size: usize,
}

impl SessionMessageManager {
    /// 创建新的消息管理器
    pub fn new(cache_size: usize) -> Self {
        Self {
            message_cache: dashmap::DashMap::new(),
            cache_size,
        }
    }

    /// 添加消息到指定 session 的缓存
    pub async fn add_message(&self, session_id: &str, message: ProgressEvent) {
        let mut cache = self.message_cache
            .entry(session_id.to_string())
            .or_insert_with(|| CircularBuffer::new(self.cache_size));

        cache.push(message);
    }

    /// 获取指定 session 的所有缓存消息
    pub async fn get_messages(&self, session_id: &str) -> Vec<ProgressEvent> {
        self.message_cache
            .get(session_id)
            .map(|cache| cache.to_vec())
            .unwrap_or_default()
    }

    /// 清理指定 session 的缓存
    pub fn clear_session(&self, session_id: &str) {
        self.message_cache.remove(session_id);
    }

    /// 获取当前缓存的会话数量
    pub fn session_count(&self) -> usize {
        self.message_cache.len()
    }
}