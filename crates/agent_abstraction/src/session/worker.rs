//! Agent Worker 抽象
//!
//! 定义请求处理的通用接口

use std::{path::PathBuf, sync::Arc};

use agent_client_protocol::{ContentBlock, PromptRequest};
use anyhow::Result;
use async_trait::async_trait;
use shared_types::{AgentLifecycle, CancelNotificationRequestWrapper, ModelProviderConfig, ServiceType};
use tokio::sync::mpsc;

use crate::PromptMessage;

/// Worker 请求
#[derive(Debug, Clone)]
pub struct WorkerRequest {
    /// Agent 提示消息
    pub prompt_message: PromptMessage,
    /// 模型提供商配置
    pub model_provider: Option<ModelProviderConfig>,
    /// 预处理的附件内容块
    /// 由 agent_runner 使用 ContentBuilder 预处理
    pub attachment_blocks: Option<Vec<ContentBlock>>,
}

impl WorkerRequest {
    /// 创建新的 Worker 请求
    pub fn new(prompt_message: PromptMessage, model_provider: Option<ModelProviderConfig>) -> Self {
        Self {
            prompt_message,
            model_provider,
            attachment_blocks: None,
        }
    }

    /// 获取项目 ID
    pub fn project_id(&self) -> &str {
        &self.prompt_message.project_id
    }

    /// 获取项目路径
    pub fn project_path(&self) -> &PathBuf {
        &self.prompt_message.project_path
    }

    /// 获取请求 ID
    pub fn request_id(&self) -> &str {
        &self.prompt_message.request_id
    }
}

/// 会话句柄
///
/// 用于传递会话句柄给 agent_runner 更新全局 MAP
#[derive(Clone)]
pub struct SessionHandles {
    pub prompt_tx: mpsc::UnboundedSender<PromptRequest>,
    pub cancel_tx: mpsc::UnboundedSender<CancelNotificationRequestWrapper>,
    pub lifecycle_handle: Option<Arc<dyn AgentLifecycle>>,
}

// 手动实现 Debug，跳过不支持 Debug 的 lifecycle_handle
impl std::fmt::Debug for SessionHandles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHandles")
            .field("prompt_tx", &"UnboundedSender<PromptRequest>")
            .field("cancel_tx", &"UnboundedSender<CancelNotificationRequestWrapper>")
            .field("lifecycle_handle", &self.lifecycle_handle.as_ref().map(|_| "Some(AgentLifecycle)"))
            .finish()
    }
}

/// Worker 响应
#[derive(Debug, Clone)]
pub struct WorkerResponse {
    /// 项目 ID
    pub project_id: String,
    /// 会话 ID
    pub session_id: String,
    /// 错误信息（如果有）
    pub error: Option<String>,
    /// 请求 ID
    pub request_id: Option<String>,
    /// 服务类型
    pub service_type: ServiceType,
    /// 标识是否是新创建的会话
    pub is_new_session: bool,
    /// 会话句柄（仅新会话时有值）
    pub session_handles: Option<SessionHandles>,
}

impl WorkerResponse {
    /// 创建新会话的成功响应
    pub fn new_session_success(
        project_id: String,
        session_id: String,
        request_id: Option<String>,
        service_type: ServiceType,
        handles: SessionHandles,
    ) -> Self {
        Self {
            project_id,
            session_id,
            error: None,
            request_id,
            service_type,
            is_new_session: true,
            session_handles: Some(handles),
        }
    }

    /// 创建复用会话的成功响应
    pub fn reuse_session_success(
        project_id: String,
        session_id: String,
        request_id: Option<String>,
        service_type: ServiceType,
    ) -> Self {
        Self {
            project_id,
            session_id,
            error: None,
            request_id,
            service_type,
            is_new_session: false,
            session_handles: None,
        }
    }

    /// 创建成功响应（旧版本，保持向后兼容）
    #[deprecated(note = "Use new_session_success or reuse_session_success instead")]
    pub fn success(
        project_id: String,
        session_id: String,
        request_id: Option<String>,
        service_type: ServiceType,
    ) -> Self {
        Self {
            project_id,
            session_id,
            error: None,
            request_id,
            service_type,
            is_new_session: false,
            session_handles: None,
        }
    }

    /// 创建错误响应
    pub fn error(
        project_id: String,
        session_id: String,
        error: String,
        request_id: Option<String>,
        service_type: ServiceType,
    ) -> Self {
        Self {
            project_id,
            session_id,
            error: Some(error),
            request_id,
            service_type,
            is_new_session: false,
            session_handles: None,
        }
    }

    /// 检查是否成功
    pub fn is_success(&self) -> bool {
        self.error.is_none()
    }
}

/// Agent Worker Trait
///
/// 定义请求处理的抽象接口，允许不同的实现策略
#[async_trait]
pub trait AgentWorker: Send + Sync {
    /// 处理单个请求
    ///
    /// # Arguments
    /// * `request` - Worker 请求
    ///
    /// # Returns
    /// 处理结果
    async fn process_request(&self, request: WorkerRequest) -> Result<WorkerResponse>;

    /// 获取 Worker 名称（用于日志和调试）
    fn name(&self) -> &'static str;
}
