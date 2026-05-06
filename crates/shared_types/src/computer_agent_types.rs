//! Computer Agent HTTP API 类型定义
//!
//! 这些类型用于 Computer Agent 的 HTTP REST API，
//! 由 rcoder 和 agent_runner 共享使用

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use garde::Validate;

use crate::{Attachment, ChatAgentConfig, ModelProviderConfig};

/// Computer Agent 聊天请求
///
/// 与标准 ChatRequest 的主要区别：
/// - `user_id` 是必填字段（用于容器标识）
/// - 一个 user_id 对应一个容器，容器内可以有多个 project_id 的 Agent 实例
#[derive(Debug, Deserialize, Serialize, Clone, ToSchema)]
pub struct ComputerChatRequest {
    /// 用户 ID (必填) - 一个用户对应一个容器
    #[schema(example = "user_123")]
    pub user_id: String,

    /// 项目 ID (可选) - 一个容器内可以有多个项目
    /// 若未提供，系统自动生成 UUID
    #[schema(example = "proj_456")]
    pub project_id: Option<String>,

    /// 用户输入的 prompt
    #[schema(example = "帮我打开浏览器访问 https://example.com")]
    pub prompt: String,

    /// 可选的会话 ID，如果不提供则创建新会话
    #[schema(example = "session789")]
    pub session_id: Option<String>,

    /// 可选的附件列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,

    /// 数据源附件列表 - 用于AI开发时获取外部数据源信息
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
    pub system_prompt: Option<String>,

    /// 可选的用户提示词模板
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_prompt: Option<String>,

    /// Agent 运行时配置
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_config: Option<ChatAgentConfig>,

    // === 新增字段 (v2 - 隔离类型支持) ===
    /// 容器唯一标识，若传值则使用此 ID 标识容器，实现容器复用
    /// 若不传则使用 user_id 作为容器标识（保持原有逻辑）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_tenant_123")]
    pub pod_id: Option<String>,

    /// 租户 ID，用于多租户场景下的数据隔离
    /// 当 pod_id 有值时，此字段必须非空
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::flexible_string::flexible_string")]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID，用于区分租户下的不同空间
    /// 当 pod_id 有值时，此字段必须非空
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::flexible_string::flexible_string")]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型，控制容器共享粒度和数据目录结构
    /// 可选值：tenant（租户隔离）、space（空间隔离）、project（项目隔离，默认）
    /// 当 pod_id 有值时，此字段必须非空
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,
}

/// Computer Agent 状态查询请求
#[derive(Debug, Clone, Deserialize, Serialize, Validate, ToSchema)]
pub struct ComputerAgentStatusRequest {
    /// 用户 ID（可与 pod_id 二选一）
    #[garde(skip)]
    #[serde(default)]
    #[schema(example = "user_123")]
    pub user_id: Option<String>,

    /// 项目 ID（必填）
    #[garde(required, length(min = 1))]
    #[serde(default)]
    #[schema(example = "project_456")]
    pub project_id: Option<String>,

    /// Pod ID，用于共享容器模式下的容器定位（可与 user_id 二选一）
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_abc123")]
    pub pod_id: Option<String>,

    /// 租户ID（可选）
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::flexible_string::flexible_string")]
    #[schema(example = "tenant_001")]
    pub tenant_id: Option<String>,

    /// 空间ID（可选）
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::flexible_string::flexible_string")]
    #[schema(example = "space_001")]
    pub space_id: Option<String>,

    /// 隔离类型（可选），如 "project", "tenant", "space"
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "project")]
    pub isolation_type: Option<String>,
}

/// Computer Agent 状态查询响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ComputerAgentStatusResponse {
    /// 用户 ID（可选，因为请求中 user_id 和 pod_id 二选一）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "user_123")]
    pub user_id: Option<String>,

    /// 项目 ID
    #[schema(example = "project_456")]
    pub project_id: String,

    /// Agent 是否存活
    #[schema(example = true)]
    pub is_alive: bool,

    /// 会话 ID（仅当 is_alive=true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "session_789")]
    pub session_id: Option<String>,

    /// Agent 状态（仅当 is_alive=true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "idle")]
    pub status: Option<String>,

    /// 最后活跃时间（仅当 is_alive=true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<chrono::DateTime<chrono::Utc>>,

    /// 创建时间（仅当 is_alive=true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl ComputerAgentStatusResponse {
    /// 创建 Agent 未启动的响应
    pub fn not_alive(user_id: Option<String>, project_id: String) -> Self {
        Self {
            user_id,
            project_id,
            is_alive: false,
            session_id: None,
            status: None,
            last_activity: None,
            created_at: None,
        }
    }
}

/// Computer Agent 停止请求
#[derive(Debug, Clone, Deserialize, Serialize, Validate, ToSchema)]
pub struct ComputerAgentStopRequest {
    /// 用户 ID（可与 pod_id 二选一）
    #[garde(skip)]
    #[serde(default)]
    #[schema(example = "user_123")]
    pub user_id: Option<String>,

    /// 项目 ID（必填）
    #[garde(required, length(min = 1))]
    #[serde(default)]
    #[schema(example = "project_456")]
    pub project_id: Option<String>,

    /// 可选的会话 ID
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "session789")]
    pub session_id: Option<String>,

    /// Pod ID，用于共享容器模式下的容器定位（可与 user_id 二选一）
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_abc123")]
    pub pod_id: Option<String>,

    /// 租户ID（可选）
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::flexible_string::flexible_string")]
    #[schema(example = "tenant_001")]
    pub tenant_id: Option<String>,

    /// 空间ID（可选）
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::flexible_string::flexible_string")]
    #[schema(example = "space_001")]
    pub space_id: Option<String>,

    /// 隔离类型（可选），如 "project", "tenant", "space"
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "project")]
    pub isolation_type: Option<String>,
}

/// Computer Agent 停止响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ComputerAgentStopResponse {
    /// 是否成功
    #[schema(example = true)]
    pub success: bool,

    /// 结果消息
    #[schema(example = "Agent stopped successfully")]
    pub message: String,

    /// 用户 ID（可选，因为请求中 user_id 和 pod_id 二选一）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "user_123")]
    pub user_id: Option<String>,

    /// Pod ID（可选，因为请求中 user_id 和 pod_id 二选一）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_abc123")]
    pub pod_id: Option<String>,

    /// 项目 ID
    #[schema(example = "project_456")]
    pub project_id: String,
}

/// Computer Agent 取消任务的请求参数
#[derive(Debug, Clone, Deserialize, Serialize, IntoParams, ToSchema)]
pub struct ComputerAgentCancelRequest {
    /// 用户 ID（可与 pod_id 二选一）
    #[param(example = "user_123")]
    #[schema(example = "user_123")]
    pub user_id: Option<String>,

    /// 项目 ID（必填）
    #[param(example = "project_456")]
    #[schema(example = "project_456")]
    pub project_id: String,

    /// 会话 ID（可选，未提供时从 registry 查找）
    #[param(example = "session_789")]
    #[schema(example = "session_789")]
    #[serde(default)]
    pub session_id: Option<String>,

    /// Pod ID，用于共享容器模式下的容器定位（可与 user_id 二选一）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "pod_abc123")]
    #[schema(example = "pod_abc123")]
    pub pod_id: Option<String>,

    /// 租户ID（可选）
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::flexible_string::flexible_string")]
    #[param(example = "tenant_001")]
    #[schema(example = "tenant_001")]
    pub tenant_id: Option<String>,

    /// 空间ID（可选）
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::flexible_string::flexible_string")]
    #[param(example = "space_001")]
    #[schema(example = "space_001")]
    pub space_id: Option<String>,

    /// 隔离类型（可选），如 "project", "tenant", "space"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "project")]
    #[schema(example = "project")]
    pub isolation_type: Option<String>,
}

/// Computer Agent 取消响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ComputerAgentCancelResponse {
    /// 是否成功
    #[schema(example = true)]
    pub success: bool,

    /// 会话 ID
    #[schema(example = "session_789")]
    pub session_id: String,
}
