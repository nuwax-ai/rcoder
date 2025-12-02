use std::path::PathBuf;

use super::{AgentType, Attachment, ModelProviderConfig};
use derive_builder::Builder;

#[derive(Debug, Clone, Builder)]
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
    /// 数据源附件列表 - 用于AI开发时获取外部数据源信息（如API接口、数据库等）
    /// 直接传递 JSON 字符串数组，简化使用方式
    #[builder(default)]
    pub data_source_attachments: Vec<String>,
    /// agent 类型
    #[builder(default)]
    pub agent_type: AgentType,
    /// 必填：服务类型选择 (强制要求指定)
    /// "rcoder" 或 "agent-runner"，不允许为空
    pub service_type: crate::ServiceType,
    /// 可选的请求ID，用于标识和追踪请求
    #[builder(default)]
    pub request_id: Option<String>,
    /// 模型提供商配置
    #[builder(default)]
    pub model_provider: Option<ModelProviderConfig>,
}

/// 返回用户 prompt 的提示,一定有project_id ,session_id ,否则报错
#[derive(Debug, Clone)]
pub struct ChatPromptResponse {
    /// 项目ID, 再 ./project_workspace/{project_id} 对应
    pub project_id: String,
    /// agent 的会话ID ,可能没有,如果没有,agent使用自动创建会话,返回会话id
    pub session_id: String,
    /// 错误信息，如果有的话
    pub error: Option<String>,
    /// 请求ID，用于标识和追踪请求
    pub request_id: Option<String>,
    /// 使用的服务类型
    pub service_type: crate::ServiceType,
}
