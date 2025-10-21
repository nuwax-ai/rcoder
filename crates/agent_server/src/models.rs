//! Agent Server 数据模型

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// 模型提供商配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProviderConfig {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub default_model: String,
    pub api_protocol: String,
}

/// 通用 HTTP 响应结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpResult<T> {
    pub success: bool,
    pub data: Option<T>,
    pub error: Option<ApiError>,
}

impl<T> HttpResult<T> {
    pub fn success(data: T) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn error(code: &str, message: &str) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(ApiError {
                code: code.to_string(),
                message: message.to_string(),
            }),
        }
    }
}

/// API 错误结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

/// 会话信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    /// 会话 ID
    pub session_id: String,
    /// 项目 ID
    pub project_id: String,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// 最后活动时间
    pub last_activity: DateTime<Utc>,
    /// 会话状态
    pub status: SessionStatus,
    /// Agent 类型
    pub agent_type: String,
    /// 当前任务 ID (如果有)
    pub current_task_id: Option<String>,
    /// 会话元数据
    pub metadata: HashMap<String, serde_json::Value>,
}

/// 会话状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    /// 初始化中
    Initializing,
    /// 空闲状态
    Idle,
    /// 处理中
    Processing,
    /// 已完成
    Completed,
    /// 已取消
    Cancelled,
    /// 错误状态
    Error,
}

/// 用户请求结构 - 支持多媒体内容
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ChatRequest {
    /// 用户输入的 prompt
    pub prompt: String,
    /// 可选的项目 ID
    pub project_id: Option<String>,
    /// 可选的会话 ID，如果不提供则创建新会话
    pub session_id: Option<String>,
    /// 可选的附件列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// 数据源附件列表 - 用于AI开发时获取外部数据源信息（如API接口、数据库等）
    /// 直接传递 JSON 字符串数组，简化使用方式
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_source_attachments: Vec<String>,
    /// 模型配置
    pub model_provider: Option<ModelProviderConfig>,
    /// 可选的请求ID，如果不提供则自动生成，用于标识和追踪请求
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// 聊天响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    /// 会话 ID
    pub session_id: String,
    /// 请求 ID
    pub request_id: String,
    /// 项目 ID
    pub project_id: String,
    /// 响应状态
    pub status: ResponseStatus,
    /// 响应内容 (如果有)
    pub content: Option<String>,
    /// 错误信息 (如果有)
    pub error: Option<String>,
    /// 创建时间
    pub created_at: DateTime<Utc>,
}

/// 响应状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResponseStatus {
    /// 已接受
    Accepted,
    /// 处理中
    Processing,
    /// 已完成
    Completed,
    /// 已失败
    Failed,
    /// 已取消
    Cancelled,
}

/// 附件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    /// 附件 ID
    pub id: String,
    /// 附件名称
    pub name: String,
    /// 附件类型
    pub attachment_type: AttachmentType,
    /// 文件路径 (对于本地文件)
    pub file_path: Option<String>,
    /// URL (对于网络资源)
    pub url: Option<String>,
    /// MIME 类型
    pub mime_type: String,
    /// 文件大小 (字节)
    pub size: Option<u64>,
}

/// 附件类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachmentType {
    /// 文本
    Text,
    /// 图像
    Image,
    /// 音频
    Audio,
    /// 文档
    Document,
    /// 其他
    Other,
}

/// 取消请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelRequest {
    /// 会话 ID
    pub session_id: String,
    /// 请求 ID
    pub request_id: String,
    /// 取消原因
    pub reason: Option<String>,
}

/// 取消响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelResponse {
    /// 是否成功
    pub success: bool,
    /// 会话 ID
    pub session_id: String,
    /// 请求 ID
    pub request_id: String,
    /// 消息
    pub message: Option<String>,
}

impl std::fmt::Display for CancelResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "CancelResponse {{ success: {}, session_id: {}, request_id: {}, message: {:?} }}",
            self.success, self.session_id, self.request_id, self.message
        )
    }
}

/// 停止 Agent 请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopAgentRequest {
    /// 项目 ID
    pub project_id: String,
    /// 强制停止
    pub force: bool,
    /// 停止原因
    pub reason: Option<String>,
}

/// 停止 Agent 响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopAgentResponse {
    /// 是否成功
    pub success: bool,
    /// 项目 ID
    pub project_id: String,
    /// 消息
    pub message: Option<String>,
}

/// Agent 状态请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatusRequest {
    /// 项目 ID
    pub project_id: String,
    /// 包含详细信息
    pub include_details: bool,
}

/// Agent 状态查询响应 - 完全复制 rcoder 的结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatusResponse {
    /// 项目ID
    pub project_id: String,
    /// Agent 是否存活
    pub is_alive: bool,
    /// 会话ID（仅当 is_alive 为 true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Agent 服务状态（仅当 is_alive 为 true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<AgentStatus>,
    /// 最后活动时间（仅当 is_alive 为 true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<DateTime<Utc>>,
    /// 创建时间（仅当 is_alive 为 true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    /// 模型提供商安全信息（仅当 is_alive 为 true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<ModelProviderConfig>,
}

/// Agent 状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    /// 启动中
    Starting,
    /// 运行中
    Running,
    /// 停止中
    Stopping,
    /// 已停止
    Stopped,
    /// 错误状态
    Error,
}

/// 系统信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfo {
    /// CPU 使用率 (0-100)
    pub cpu_usage_percent: f64,
    /// 内存使用量 (字节)
    pub memory_usage_bytes: u64,
    /// 内存总量 (字节)
    pub memory_total_bytes: u64,
    /// 磁盘使用量 (字节)
    pub disk_usage_bytes: u64,
    /// 磁盘总量 (字节)
    pub disk_total_bytes: u64,
    /// 活跃连接数
    pub active_connections: usize,
    /// 处理的请求数
    pub total_requests: u64,
}

/// 进度通知事件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressEvent {
    /// 事件 ID
    pub event_id: String,
    /// 会话 ID
    pub session_id: String,
    /// 请求 ID
    pub request_id: String,
    /// 事件类型
    pub event_type: ProgressEventType,
    /// 事件数据
    pub data: serde_json::Value,
    /// 时间戳
    pub timestamp: DateTime<Utc>,
}

/// 进度事件类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProgressEventType {
    /// 任务开始
    TaskStarted,
    /// 任务进度更新
    TaskProgress,
    /// 任务完成
    TaskCompleted,
    /// 任务失败
    TaskFailed,
    /// 任务取消
    TaskCancelled,
    /// 会话状态变更
    SessionStatusChanged,
    /// Agent 状态变更
    AgentStatusChanged,
    /// 错误
    Error,
}

/// 健康检查响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    /// 服务状态
    pub status: HealthStatus,
    /// 服务版本
    pub version: String,
    /// 启动时间
    pub started_at: DateTime<Utc>,
    /// 运行时间 (秒)
    pub uptime_seconds: u64,
    /// 活跃会话数
    pub active_sessions: usize,
    /// 总请求数
    pub total_requests: u64,
    /// 检查详情
    pub checks: HashMap<String, HealthCheck>,
}

/// 健康状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HealthStatus {
    /// 健康
    Healthy,
    /// 警告
    Warning,
    /// 不健康
    Unhealthy,
}

/// 健康检查项
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheck {
    /// 检查状态
    pub status: HealthStatus,
    /// 检查消息
    pub message: String,
    /// 检查耗时 (毫秒)
    pub duration_ms: u64,
    /// 检查时间戳
    pub timestamp: DateTime<Utc>,
}

impl ProgressEvent {
    /// 创建新的进度事件
    pub fn new(
        session_id: String,
        request_id: String,
        event_type: ProgressEventType,
        data: serde_json::Value,
    ) -> Self {
        Self {
            event_id: Uuid::new_v4().to_string(),
            session_id,
            request_id,
            event_type,
            data,
            timestamp: Utc::now(),
        }
    }
}

impl SessionInfo {
    /// 创建新的会话信息
    pub fn new(session_id: String, project_id: String, agent_type: String) -> Self {
        let now = Utc::now();
        Self {
            session_id,
            project_id,
            created_at: now,
            last_activity: now,
            status: SessionStatus::Initializing,
            agent_type,
            current_task_id: None,
            metadata: HashMap::new(),
        }
    }

    /// 更新最后活动时间
    pub fn update_activity(&mut self) {
        self.last_activity = Utc::now();
    }

    /// 更新状态
    pub fn update_status(&mut self, status: SessionStatus) {
        self.status = status;
        self.update_activity();
    }

    /// 设置当前任务
    pub fn set_current_task(&mut self, task_id: Option<String>) {
        self.current_task_id = task_id;
        self.update_activity();
    }
}