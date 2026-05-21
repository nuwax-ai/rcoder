//! RCoder Agent HTTP API 类型定义
//!
//! 这些类型用于 RCoder 模式的 HTTP REST API，
//! 由 rcoder 和 agent_runner 共享使用
//!
//! 与 Computer Agent 类型的主要区别：
//! - 无 user_id 字段（以 project_id 为核心标识）
//! - 无容器管理字段（pod_id, tenant_id, space_id, isolation_type）
//!   因为 Agent Runner 不管理容器，这些由 RCoder 服务端处理

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::{Attachment, ChatAgentConfig, ModelProviderConfig};

/// RCoder Agent 聊天请求
///
/// 与 ComputerChatRequest 的主要区别：
/// - 以 project_id 为核心标识（无 user_id）
/// - 无容器管理字段（Agent Runner 不管理容器）
#[derive(Debug, Deserialize, Serialize, Clone, ToSchema)]
pub struct RcoderChatRequest {
    /// 用户输入的 prompt
    #[schema(example = "帮我写一个 Rust 的 Hello World 程序")]
    pub prompt: String,

    /// 可选的项目 ID，若未提供则自动生成
    #[schema(example = "test_project")]
    pub project_id: Option<String>,

    /// 可选的会话 ID，若不提供则创建新会话
    #[schema(example = "session456")]
    pub session_id: Option<String>,

    /// 可选的附件列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,

    /// 数据源附件列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_source_attachments: Vec<String>,

    /// 模型配置
    pub model_provider: Option<ModelProviderConfig>,

    /// 可选的请求ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "req_123456789")]
    pub request_id: Option<String>,

    /// 可选的系统提示词覆盖
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "你是一个专业的 Rust 开发者")]
    pub system_prompt: Option<String>,

    /// 可选的用户提示词模板
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "请用 Rust 完成：{user_prompt}")]
    pub user_prompt: Option<String>,

    /// Agent 运行时配置（Agent 服务器 + MCP 服务器）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_config: Option<ChatAgentConfig>,
}

/// RCoder Agent 取消任务请求
#[derive(Debug, Clone, Deserialize, Serialize, IntoParams, ToSchema)]
pub struct RcoderAgentCancelRequest {
    /// 项目 ID（必填）
    #[param(example = "project_456")]
    #[schema(example = "project_456")]
    pub project_id: String,

    /// 会话 ID（可选，未提供时从 registry 查找）
    #[param(example = "session_789")]
    #[schema(example = "session_789")]
    #[serde(default)]
    pub session_id: Option<String>,
}

/// RCoder Agent 取消响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RcoderAgentCancelResponse {
    /// 是否成功
    #[schema(example = true)]
    pub success: bool,

    /// 会话 ID
    #[schema(example = "session_789")]
    pub session_id: String,
}

/// RCoder Agent 停止请求
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct RcoderAgentStopRequest {
    /// 项目 ID（必填）
    #[schema(example = "project_456")]
    pub project_id: String,
}

/// RCoder Agent 停止响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RcoderAgentStopResponse {
    /// 是否成功
    #[schema(example = true)]
    pub success: bool,

    /// 项目 ID
    #[schema(example = "project_456")]
    pub project_id: String,

    /// 会话 ID（如果存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "session_789")]
    pub session_id: Option<String>,

    /// 结果消息
    #[schema(example = "Agent stopped successfully")]
    pub message: String,
}
