//! Agent HTTP API 类型定义
//!
//! 这些类型用于 Agent (RCoder Service) 的 HTTP REST API，
//! 由 rcoder 和 agent_runner 共享使用

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use garde::Validate;

use crate::{Attachment, ChatAgentConfig, ModelProviderConfig};

/// Agent 聊天请求
#[derive(Debug, Deserialize, Serialize, Clone, ToSchema)]
pub struct AgentChatRequest {
    /// 项目 ID (可选)
    /// 若未提供，系统自动生成 UUID
    #[schema(example = "proj_456")]
    pub project_id: Option<String>,

    /// 用户输入的 prompt
    #[schema(example = "帮我写一段 hello world 代码")]
    pub prompt: String,

    /// 会话 ID (可选)
    /// 如果不提供则创建新会话
    #[schema(example = "session789")]
    pub session_id: Option<String>,

    /// 附件列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,

    /// 数据源附件列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_source_attachments: Vec<String>,

    /// 模型配置
    pub model_provider: Option<ModelProviderConfig>,

    /// 请求 ID (可选)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "req_123456789")]
    pub request_id: Option<String>,

    /// 系统提示词覆盖 (可选)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,

    /// 用户提示词模板 (可选)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_prompt: Option<String>,

    /// Agent 运行时配置 (可选)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_config: Option<ChatAgentConfig>,

    /// 容器唯一标识 (可选)
    /// 用于共享容器模式下的容器定位
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_abc123")]
    pub pod_id: Option<String>,

    /// 租户 ID (可选)
    /// 用于多租户场景下的数据隔离
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "tenant_001")]
    pub tenant_id: Option<String>,

    /// 空间 ID (可选)
    /// 用于区分租户下的不同空间
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型 (可选)
    /// 控制容器共享粒度：tenant（租户隔离）、space（空间隔离）、project（项目隔离，默认）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "project")]
    pub isolation_type: Option<String>,
}

/// Agent 取消会话请求
#[derive(Debug, Deserialize, Serialize, Clone, Validate, ToSchema)]
pub struct AgentCancelRequest {
    /// 项目 ID (必填)
    /// 用于标识要取消的特定项目的 agent
    #[garde(required, length(min = 1))]
    #[serde(default)]
    #[schema(example = "proj_456")]
    pub project_id: Option<String>,

    /// 会话 ID (可选)
    /// 如果不提供则取消该项目的所有活跃会话
    #[garde(skip)]
    #[serde(default)]
    #[schema(example = "session789")]
    pub session_id: Option<String>,

    /// Pod ID (可选)
    /// 用于共享容器模式下的容器定位
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_abc123")]
    pub pod_id: Option<String>,

    /// 租户 ID (可选)
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "tenant_001")]
    pub tenant_id: Option<String>,

    /// 空间 ID (可选)
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型 (可选)
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "project")]
    pub isolation_type: Option<String>,
}

/// Agent 取消会话响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AgentCancelResponse {
    /// 取消操作是否成功
    #[schema(example = true)]
    pub success: bool,

    /// 被取消的会话 ID
    #[schema(example = "session456")]
    pub session_id: String,
}

/// Agent 停止请求
#[derive(Debug, Deserialize, Serialize, Clone, Validate, ToSchema)]
pub struct AgentStopRequest {
    /// 项目 ID (必填)
    #[garde(required, length(min = 1))]
    #[serde(default)]
    #[schema(example = "proj_456")]
    pub project_id: Option<String>,

    /// 会话 ID (可选)
    /// 如果不提供则停止该项目的所有活跃会话
    #[garde(skip)]
    #[serde(default)]
    #[schema(example = "session789")]
    pub session_id: Option<String>,

    /// Pod ID (可选)
    /// 用于共享容器模式下的容器定位
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_abc123")]
    pub pod_id: Option<String>,

    /// 租户 ID (可选)
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "tenant_001")]
    pub tenant_id: Option<String>,

    /// 空间 ID (可选)
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型 (可选)
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "project")]
    pub isolation_type: Option<String>,
}

/// Agent 停止响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AgentStopResponse {
    /// 是否成功停止
    #[schema(example = true)]
    pub success: bool,

    /// 项目 ID
    #[schema(example = "proj_456")]
    pub project_id: String,

    /// 会话 ID
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "session789")]
    pub session_id: Option<String>,

    /// 消息
    #[schema(example = "Agent stopped successfully")]
    pub message: String,
}
