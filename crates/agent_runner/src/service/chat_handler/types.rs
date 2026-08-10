//! Chat Handler 类型定义（gRPC / HTTP 共享）。

use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use shared_types::{Attachment, ChatAgentConfig, ModelProviderConfig, ServiceType};

use crate::service::{AgentSessionService, PendingGuard};

/// Chat Handler 输入参数
///
/// 包含处理 chat 请求所需的所有参数，与协议无关（gRPC/HTTP）
#[derive(Debug, Clone)]
pub struct ChatHandlerInput {
    /// 项目 ID
    pub project_id: String,
    /// 项目工作目录（由调用方根据环境决定）
    pub project_dir: PathBuf,
    /// 会话 ID（可选，用于复用会话）
    pub session_id: Option<String>,
    /// 用户提示词
    pub prompt: String,
    /// 请求 ID（用于追踪）
    pub request_id: String,
    /// 附件列表
    pub attachments: Vec<Attachment>,
    /// 数据源附件列表
    pub data_source_attachments: Vec<String>,
    /// 模型配置（可选）
    pub model_config: Option<ModelProviderConfig>,
    /// 服务类型
    pub service_type: ServiceType,
    /// 用户 ID（ComputerAgentRunner 模式使用）
    pub user_id: Option<String>,
    /// Agent 配置覆盖（可选）
    pub agent_config_override: Option<ChatAgentConfig>,
    /// 系统提示覆盖（可选）
    pub system_prompt_override: Option<String>,
    /// 用户提示模板覆盖（可选）
    pub user_prompt_template_override: Option<String>,
    /// 是否是 DevComputer 接口请求
    ///
    /// 用于 `{PREFIX_WORKSPACE_DIR}` 变量解析：
    /// - `true`：LOG_DIR 解析为 `/home/user/`
    /// - `false`：LOG_DIR 解析为 `/app/container-logs`
    pub is_devcomputer: bool,
}

/// Chat Handler 输出结果
///
/// 统一的响应结构，可转换为 gRPC 或 HTTP 响应
#[derive(Debug, Clone)]
pub struct ChatHandlerOutput {
    /// 项目 ID
    pub project_id: String,
    /// 会话 ID
    pub session_id: String,
    /// 是否成功
    pub success: bool,
    /// 错误消息（可选）
    pub error: Option<String>,
    /// 错误码（可选）
    pub error_code: Option<String>,
    /// 请求 ID（可选）
    pub request_id: Option<String>,
    /// 是否需要降级处理
    pub need_fallback: bool,
    /// 降级原因（可选）
    pub fallback_reason: Option<String>,
    /// 是否触发了 agent 二进制热重载
    pub reloaded: bool,
    /// agent 版本号（可选，检测失败时为 None）
    pub agent_version: Option<String>,
}

impl ChatHandlerOutput {
    /// 创建错误响应
    pub fn error(
        project_id: String,
        session_id: String,
        error_msg: String,
        error_code: String,
    ) -> Self {
        Self {
            project_id,
            session_id,
            success: false,
            error: Some(error_msg),
            error_code: Some(error_code),
            request_id: None,
            need_fallback: false,
            fallback_reason: None,
            reloaded: false,
            agent_version: None,
        }
    }
}

/// Chat Handler 依赖上下文
///
/// 包含处理 chat 请求所需的运行时依赖
pub struct ChatHandlerContext {
    /// Agent 会话服务
    pub agent_session_service: Arc<AgentSessionService>,
    /// 共享的 API 密钥管理器
    pub shared_api_key_manager: Arc<DashMap<String, ModelProviderConfig>>,
    /// project_id -> UUID 映射
    pub project_uuid_map: Arc<DashMap<String, String>>,
}

/// 会话准备阶段的产出
///
/// 由 `prepare_session` 返回，传递给后续的任务下发与结果组装阶段。
/// 仅在 `chat_handler` 模块内部使用。
pub struct SessionPreparation {
    /// RAII 状态守卫（成功提交前保持 Pending，失败时自动清理）
    pub(super) pending_guard: PendingGuard<'static>,
    /// agent 版本号（检测失败时为 None）
    pub(super) agent_version: Option<String>,
    /// 是否触发了 agent 二进制热重载
    pub(super) was_reloaded: bool,
    /// auto_reload 重启前的旧 session_id（用于 resume 恢复上下文）
    pub(super) resume_session_id: Option<String>,
}
