//! Computer Agent HTTP API 类型定义
//!
//! 这些类型用于 Computer Agent 的 HTTP REST API，
//! 由 rcoder 和 agent_runner 共享使用

use garde::Validate;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::{Attachment, ChatAgentConfig, ModelProviderConfig};

/// `/computer/chat` 的业务域路由标记（枚举而非自由字符串——匹配处穷尽，
/// 词表单一事实源；值与 `X-Service-Type` header 同为 `userapp`）。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ChatServiceScope {
    /// userApp 开发对话：路由到该 app 的 UserappBuilder 开发容器
    Userapp,
}

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
    /// userApp 开发对话场景（service_type=userapp）下必填且等于 app_id
    /// （路由到该 app 的 UserappBuilder 开发容器）
    #[schema(example = "proj_456")]
    pub project_id: Option<String>,

    /// 业务域路由标记（枚举）。可选值：**仅 `userapp`**（wire 词表
    /// snake_case）——本请求路由到该 app（project_id 兼任 app_id）的
    /// UserappBuilder 开发容器，ACP agent 直接在开发卷 workspace
    /// （`{USERAPP_WORKSPACE_DIR}/{app_id}`）上工作，生成的代码直接落卷。
    /// 仅开发阶段传；部署后无对话。缺省（不传）= 普通 computer 沙箱对话；
    /// 未知值反序列化即拒（fail-fast，不静默回落 computer——路由错容器
    /// 比 400 更难排查）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_type: Option<ChatServiceScope>,

    /// userApp 应用阶段 dev/prod（缺省 dev）——**project_id 兼任 app_id**；
    /// userApp 开发对话仅支持 dev：agent 会话只存在于 UserappBuilder 开发
    /// 容器，prod 运行容器无 agent 会话（形态对齐 agent 族五接口）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "dev")]
    pub app_stage: Option<String>,

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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::flexible_string::flexible_string"
    )]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID，用于区分租户下的不同空间
    /// 当 pod_id 有值时，此字段必须非空
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::flexible_string::flexible_string"
    )]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型，控制容器共享粒度和数据目录结构
    /// 可选值：tenant（租户隔离）、space（空间隔离）、project（项目隔离，默认）
    /// 当 pod_id 有值时，此字段必须非空
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,

    /// Agent 工作目录标识符（可选）
    /// 用于替代 project_id 参与工作目录路径拼接
    /// 未提供时使用 project_id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "custom_workspace_123")]
    pub agent_work_dir: Option<String>,
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::flexible_string::flexible_string"
    )]
    #[schema(example = "tenant_001")]
    pub tenant_id: Option<String>,

    /// 空间ID（可选）
    #[garde(skip)]
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::flexible_string::flexible_string"
    )]
    #[schema(example = "space_001")]
    pub space_id: Option<String>,

    /// 隔离类型（可选），如 "project", "tenant", "space"
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "project")]
    pub isolation_type: Option<String>,

    /// userApp 分派标记。可选值：`userapp`（推荐）、`user-app` / `application` /
    /// `app`（同义变体，均大小写不敏感）——与 project_id 搭配（**project_id
    /// 兼任 app_id**，对齐 /computer/chat 契约，不设独立 app_id 字段）定位
    /// UserappBuilder 开发容器，agent 会话仅存在于 dev 阶段。userApp 容器类型由 app_stage 推导
    /// （dev=UserappBuilder / prod=Userapp），**勿传 `user-app-builder`**。
    /// 不传时走既有 computer 路径
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "userapp")]
    pub service_type: Option<String>,

    /// userApp 应用阶段 dev/prod（缺省 dev）——**project_id 兼任 app_id**；
    /// 本接口 userApp 分派仅支持 dev：agent 会话只存在于 UserappBuilder
    /// 开发容器，prod 运行容器无 agent 会话
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "dev")]
    pub app_stage: Option<String>,
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::flexible_string::flexible_string"
    )]
    #[schema(example = "tenant_001")]
    pub tenant_id: Option<String>,

    /// 空间ID（可选）
    #[garde(skip)]
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::flexible_string::flexible_string"
    )]
    #[schema(example = "space_001")]
    pub space_id: Option<String>,

    /// 隔离类型（可选），如 "project", "tenant", "space"
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "project")]
    pub isolation_type: Option<String>,

    /// userApp 分派标记。可选值：`userapp`（推荐）、`user-app` / `application` /
    /// `app`（同义变体，均大小写不敏感）——与 project_id 搭配（**project_id
    /// 兼任 app_id**，对齐 /computer/chat 契约，不设独立 app_id 字段）定位
    /// UserappBuilder 开发容器，agent 会话仅存在于 dev 阶段。userApp 容器类型由 app_stage 推导
    /// （dev=UserappBuilder / prod=Userapp），**勿传 `user-app-builder`**。
    /// 不传时走既有 computer 路径
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "userapp")]
    pub service_type: Option<String>,

    /// userApp 应用阶段 dev/prod（缺省 dev）——**project_id 兼任 app_id**；
    /// 本接口 userApp 分派仅支持 dev：agent 会话只存在于 UserappBuilder
    /// 开发容器，prod 运行容器无 agent 会话
    #[garde(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "dev")]
    pub app_stage: Option<String>,
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

    /// 项目 ID（computer 路径与 userApp 分派均必填——userApp 场景下兼任
    /// app_id，即 UserappBuilder 开发容器标识）
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::flexible_string::flexible_string"
    )]
    #[param(example = "tenant_001")]
    #[schema(example = "tenant_001")]
    pub tenant_id: Option<String>,

    /// 空间ID（可选）
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::flexible_string::flexible_string"
    )]
    #[param(example = "space_001")]
    #[schema(example = "space_001")]
    pub space_id: Option<String>,

    /// 隔离类型（可选），如 "project", "tenant", "space"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "project")]
    #[schema(example = "project")]
    pub isolation_type: Option<String>,

    /// userApp 分派标记。可选值：`userapp`（推荐）、`user-app` / `application` /
    /// `app`（同义变体，均大小写不敏感）——与 project_id 搭配（**project_id
    /// 兼任 app_id**，对齐 /computer/chat 契约，不设独立 app_id 字段）定位
    /// UserappBuilder 开发容器，agent 会话仅存在于 dev 阶段。userApp 容器类型由 app_stage 推导
    /// （dev=UserappBuilder / prod=Userapp），**勿传 `user-app-builder`**。
    /// 不传时走既有 computer 路径
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "userapp")]
    #[schema(example = "userapp")]
    pub service_type: Option<String>,

    /// userApp 应用阶段 dev/prod（缺省 dev）——**project_id 兼任 app_id**；
    /// 本接口 userApp 分派仅支持 dev：agent 会话只存在于 UserappBuilder
    /// 开发容器，prod 运行容器无 agent 会话
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "dev")]
    #[schema(example = "dev")]
    pub app_stage: Option<String>,
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

#[cfg(test)]
mod userapp_dispatch_tests {
    use super::*;

    /// 契约钉住：agent 族接口（status/stop/cancel）的 userApp wire 形态 =
    /// service_type=userapp + **project_id 兼任 app_id** + app_stage（可缺省），
    /// 对齐 /computer/chat 契约——不设独立 app_id 字段，Java 侧统一传
    /// project_id。缺 service_type 时 app_stage 缺省字段也不影响既有形态。
    #[test]
    fn agent_requests_deserialize_userapp_wire_form() {
        let raw = r#"{"service_type":"userapp","project_id":"app-1","app_stage":"dev"}"#;
        let status: ComputerAgentStatusRequest =
            serde_json::from_str(raw).unwrap_or_else(|e| panic!("status {raw} 应可反序列化: {e}"));
        assert_eq!(status.service_type.as_deref(), Some("userapp"));
        assert_eq!(status.project_id.as_deref(), Some("app-1"));
        assert_eq!(status.app_stage.as_deref(), Some("dev"));

        let stop: ComputerAgentStopRequest =
            serde_json::from_str(raw).unwrap_or_else(|e| panic!("stop {raw} 应可反序列化: {e}"));
        assert_eq!(stop.service_type.as_deref(), Some("userapp"));

        // cancel 的 project_id 为 String 必填（userApp 形态必传——兼任 app_id）
        let cancel_raw = r#"{"service_type":"userapp","project_id":"app-1"}"#;
        let cancel: ComputerAgentCancelRequest = serde_json::from_str(cancel_raw)
            .unwrap_or_else(|e| panic!("cancel {cancel_raw} 应可反序列化: {e}"));
        assert_eq!(cancel.project_id, "app-1");
        assert!(cancel.app_stage.is_none());
    }

    /// 契约钉住：/computer/chat 的 userApp wire 形态（service_type 枚举 +
    /// project_id 兼任 app_id + app_stage 可缺省）——app_stage 与 agent 族
    /// 五接口同语义（缺省 dev，prod 服务端 400）。
    #[test]
    fn chat_request_deserializes_userapp_wire_form_with_app_stage() {
        for raw in [
            r#"{"service_type":"userapp","project_id":"app-1","prompt":"hi","user_id":"u1"}"#,
            r#"{"service_type":"userapp","project_id":"app-1","app_stage":"dev","prompt":"hi","user_id":"u1"}"#,
        ] {
            let chat: ComputerChatRequest = serde_json::from_str(raw)
                .unwrap_or_else(|e| panic!("chat userApp 形态 {raw} 应可反序列化: {e}"));
            assert_eq!(chat.project_id.as_deref(), Some("app-1"));
            assert!(
                chat.app_stage.as_deref().is_none_or(|s| s == "dev"),
                "app_stage 应可缺省或为 dev"
            );
        }
        // 非法 stage 是服务端校验（String 承接，非 serde 枚举）——反序列化应通过
        let chat: ComputerChatRequest = serde_json::from_str(
            r#"{"service_type":"userapp","project_id":"app-1","app_stage":"prod","prompt":"hi","user_id":"u1"}"#,
        )
        .unwrap();
        assert_eq!(chat.app_stage.as_deref(), Some("prod"));
    }

    /// 既有 computer 形态回归：不传三字段反序列化不受影响（全部缺省）。
    #[test]
    fn agent_requests_deserialize_legacy_computer_form() {
        let raw = r#"{"user_id":"user_123","project_id":"project_456"}"#;
        let status: ComputerAgentStatusRequest = serde_json::from_str(raw).unwrap();
        assert!(status.service_type.is_none());
        assert!(status.app_stage.is_none());
        let stop: ComputerAgentStopRequest = serde_json::from_str(raw).unwrap();
        assert!(stop.service_type.is_none());
    }
}
