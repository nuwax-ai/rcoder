use std::path::PathBuf;

use super::{AgentType, Attachment};
use derive_builder::Builder;

#[derive(Debug, Clone, Default, Builder)]
#[builder(setter(into))]
pub struct ChatPrompt {
    /// 项目ID, 再 ./project_workspace/{project_id} 对应
    pub project_id: String,
    /// 项目路径, 再 ./project_workspace/{project_id}
    pub project_path: PathBuf,
    /// agent 的会话ID ,可能没有,如果没有,agent使用自动创建会话,返回会话id
    pub session_id: Option<String>,
    /// 提示内容 prompt
    pub prompt: String,
    /// 可选的附件列表
    #[builder(default)]
    pub attachments: Vec<Attachment>,
    /// agent 类型
    #[builder(default)]
    pub agent_type: AgentType,
    /// 可选的请求ID，用于标识和追踪请求
    #[builder(default)]
    pub request_id: Option<String>,
    /// 可选的 Context7 API 密钥，如果提供会启用 Context7 MCP 服务
    #[builder(default)]
    pub context7_api_key: Option<String>,
    /// 是否使用简化提示词（默认为 false，使用完整系统提示词）
    #[builder(default)]
    pub use_simple_prompt: bool,
}

/// 返回用户 prompt 的提示,一定有project_id ,session_id ,否则报错
#[derive(Debug, Clone)]
pub struct ChatPromptResponse {
    /// 项目ID, 再 ./project_workspace/{project_id} 对应
    pub project_id: String,
    /// agent 的会话ID ,可能没有,如果没有,agent使用自动创建会话,返回会话id
    pub session_id: String,
}
