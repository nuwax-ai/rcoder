//! # ACP Session Manager
//!
//! 基于 project_id 管理 ACP 会话的核心模块。
//!
//! ## SACP 迁移说明
//!
//! 本模块已迁移至 SACP 实现：
//! - 移除了 `Client` trait 泛型参数（SACP 内部处理）
//! - 简化了 `create_session` 参数（不再需要 client 和 shared_api_key_manager）
//! - 使用标准 tokio::spawn（无需 LocalSet）
//!
//! ## 职责范围
//!
//! 1. **会话生命周期管理**
//!    - 创建新会话 (`create_session`)
//!    - 获取或复用会话 (`get_or_create_session`)
//!    - 移除会话 (`remove_session`)
//!    - 检测会话健康状态（channel 是否关闭）
//!
//! 2. **Prompt 构建**
//!    - 构建纯文本 Prompt (`build_text_prompt_request`)
//!    - 构建带附件的 Prompt (`build_prompt_request_with_attachments`)
//!    - 将 request_id 放入 meta 字段
//!
//! 3. **路径处理**
//!    - 路径规范化 (`normalize_path`)
//!    - 确保项目目录存在 (`ensure_project_dir`)
//!
//! ## 架构说明
//!
//! 使用依赖注入的 `SessionRegistry` 进行会话存储：
//! - 统一使用注入的 `SessionRegistry`（通常是 `AGENT_REGISTRY`）
//!
//! ## 与 Worker 的协作
//!
//! ```text
//! AcpAgentWorker (acp_worker.rs)
//!       │
//!       │ 1. 调用 get_or_create_session()
//!       ▼
//! AcpSessionManager
//!       │
//!       │ 2. 通过 SessionRegistry 获取/创建会话
//!       │ 3. 通过 SacpClaudeCodeLauncher 启动 Agent
//!       ▼
//! SessionRegistry (注入的 AGENT_REGISTRY)
//! ```

mod create_guard;

use create_guard::SessionCreateHandle;
use std::path::{Component, PathBuf};
use std::sync::Arc;

use agent_client_protocol::schema::v1::{ContentBlock, PromptRequest, SessionId, TextContent};
use agent_config::PromptBuilder;
use anyhow::Result;
use dashmap::DashMap;
use shared_types::{ProjectAndAgentInfo, SessionEntry};
use tracing::{debug, error, info};

use crate::PromptMessage;
use crate::diagnostics::DiagnosticsListener;
use crate::launcher::ModelRuntimeEnvResolver;
use crate::traits::{
    PermissionRequestHandler, SessionNotifier, SessionRegistry, YoloPermissionRequestHandler,
};

/// ACP 会话管理器 (SACP 版本)
///
/// 管理所有活跃的 ACP 会话，提供：
/// - 会话创建和复用
/// - 模型配置变化检测
/// - 会话生命周期管理
///
/// ## 泛型参数
/// - `N`: SessionNotifier 实现，用于推送 SSE 消息
/// - `R`: SessionRegistry 实现，用于存储会话数据（通常是 AGENT_REGISTRY）
pub struct AcpSessionManager<N: SessionNotifier, R: SessionRegistry> {
    /// 会话注册表（注入的 SessionRegistry）
    registry: Arc<R>,
    /// 会话通知器
    notifier: Arc<N>,
    /// 模型运行时环境解析策略
    model_env_resolver: Arc<dyn ModelRuntimeEnvResolver>,
    /// ACP permission request handler.
    permission_handler: Arc<dyn PermissionRequestHandler>,
    /// 进程诊断监听器（可选，注入自 AcpClientBuilder）
    diagnostics_listener: Option<Arc<dyn DiagnosticsListener>>,
    /// project_id → 进行中的会话创建（并发合流：同 project 并发 chat 共享同一次
    /// spawn，消除双 spawn + 败者 SIGKILL 健康进程）
    in_flight: Arc<DashMap<String, Arc<SessionCreateHandle<R::Entry>>>>,
}

impl<N: SessionNotifier + 'static, R: SessionRegistry> AcpSessionManager<N, R>
where
    R::Entry: Into<ProjectAndAgentInfo> + From<ProjectAndAgentInfo>,
{
    /// 创建新的会话管理器
    ///
    /// # 参数
    /// - `notifier`: 会话通知器
    /// - `registry`: 会话注册表（通常注入 AGENT_REGISTRY）
    pub fn new(notifier: Arc<N>, registry: Arc<R>) -> Self {
        Self::with_model_env_resolver(
            notifier,
            registry,
            crate::launcher::direct_model_runtime_env_resolver(),
        )
    }

    pub fn with_model_env_resolver(
        notifier: Arc<N>,
        registry: Arc<R>,
        model_env_resolver: Arc<dyn ModelRuntimeEnvResolver>,
    ) -> Self {
        Self::with_dependencies(
            notifier,
            registry,
            model_env_resolver,
            Arc::new(YoloPermissionRequestHandler),
            None,
        )
    }

    pub fn with_dependencies(
        notifier: Arc<N>,
        registry: Arc<R>,
        model_env_resolver: Arc<dyn ModelRuntimeEnvResolver>,
        permission_handler: Arc<dyn PermissionRequestHandler>,
        diagnostics_listener: Option<Arc<dyn DiagnosticsListener>>,
    ) -> Self {
        Self {
            registry,
            notifier,
            model_env_resolver,
            permission_handler,
            diagnostics_listener,
            in_flight: Arc::new(DashMap::new()),
        }
    }

    /// 获取会话信息
    pub fn get_session(&self, project_id: &str) -> Option<R::Entry> {
        self.registry.get(project_id)
    }

    /// 检查会话是否存在
    pub fn contains_session(&self, project_id: &str) -> bool {
        self.registry.contains(project_id)
    }

    /// 移除会话
    pub fn remove_session(&self, project_id: &str) -> Option<R::Entry> {
        self.registry.remove(project_id)
    }

    /// 获取所有会话的 project_id 列表
    pub fn list_sessions(&self) -> Vec<String> {
        self.registry.list_project_ids()
    }

    /// 获取会话数量
    pub fn session_count(&self) -> usize {
        self.registry.count()
    }

    /// 获取 registry 的 Arc 引用
    pub fn registry(&self) -> Arc<R> {
        self.registry.clone()
    }

    /// 获取 notifier 的 Arc 引用
    pub fn notifier(&self) -> Arc<N> {
        self.notifier.clone()
    }

    /// 规范化项目路径
    ///
    /// - 如果是相对路径，先与当前目录拼接
    /// - 去除路径中的 "./"（CurDir 组件）
    /// - 解析 ".."（ParentDir）：绝对路径弹出上一个普通组件，相对路径保留
    pub fn normalize_path(path: &PathBuf) -> PathBuf {
        let joined_path = if path.is_absolute() {
            path.clone()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        };

        let mut components = Vec::new();
        for c in joined_path.components() {
            match c {
                Component::CurDir => {} // skip .
                Component::ParentDir => {
                    // 绝对路径：弹出最后一个普通组件，防止目录穿越
                    // 相对路径：保留 ..（由调用方负责处理）
                    if joined_path.is_absolute() {
                        if let Some(Component::Normal(_)) = components.last() {
                            components.pop();
                        }
                    } else {
                        components.push(c);
                    }
                }
                _ => components.push(c),
            }
        }
        components.iter().collect()
    }

    /// 确保项目目录存在
    pub async fn ensure_project_dir(path: &PathBuf) -> Result<()> {
        if !path.exists() {
            info!("Project path does not exist, creating: {:?}", path);
            tokio::fs::create_dir_all(path).await?;
        }
        Ok(())
    }

    /// 构建基础 Prompt 请求（仅文本内容）
    ///
    /// 使用 ACP 协议分离模式：
    /// - 系统提示词已在 NewSessionRequest._meta.systemPrompt 中传递
    /// - 这里只构建纯用户提示词
    ///
    /// 注意：附件转换由调用方处理（因为涉及文件 I/O）
    pub fn build_text_prompt_request(
        prompt: &PromptMessage,
        session_id: SessionId,
    ) -> Result<PromptRequest> {
        // 构建纯用户提示词
        let final_prompt = if prompt.data_source_attachments.is_empty() {
            PromptBuilder::build_user_prompt(&prompt.content)
        } else {
            PromptBuilder::build_user_prompt_with_data_sources(
                &prompt.content,
                &prompt.data_source_attachments,
            )
        };

        // 创建文本内容块
        let text_block = ContentBlock::Text(TextContent::new(final_prompt));
        let content_blocks = vec![text_block];

        // 将 request_id 放入 meta 字段
        debug!(
            "🔧 [build_prompt] Putting request_id={} into PromptRequest.meta",
            prompt.request_id
        );
        let mut meta = serde_json::Map::new();
        meta.insert(
            "request_id".to_string(),
            serde_json::Value::String(prompt.request_id.clone()),
        );

        Ok(PromptRequest::new(session_id, content_blocks).meta(meta))
    }

    /// 构建带附件的 Prompt 请求
    ///
    /// # Arguments
    /// * `prompt` - 提示消息
    /// * `session_id` - 会话 ID
    /// * `attachment_blocks` - 已转换的附件内容块
    pub fn build_prompt_request_with_attachments(
        prompt: &PromptMessage,
        session_id: SessionId,
        attachment_blocks: Vec<ContentBlock>,
    ) -> Result<PromptRequest> {
        // 构建纯用户提示词
        let final_prompt = if prompt.data_source_attachments.is_empty() {
            PromptBuilder::build_user_prompt(&prompt.content)
        } else {
            PromptBuilder::build_user_prompt_with_data_sources(
                &prompt.content,
                &prompt.data_source_attachments,
            )
        };

        // 创建文本内容块
        let text_block = ContentBlock::Text(TextContent::new(final_prompt));
        let mut content_blocks = vec![text_block];

        // 添加附件内容块
        content_blocks.extend(attachment_blocks);

        // 将 request_id 放入 meta 字段
        debug!(
            "🔧 [build_prompt] Putting request_id={} into PromptRequest.meta",
            prompt.request_id
        );
        let mut meta = serde_json::Map::new();
        meta.insert(
            "request_id".to_string(),
            serde_json::Value::String(prompt.request_id.clone()),
        );

        Ok(PromptRequest::new(session_id, content_blocks).meta(meta))
    }

    /// 发送 Prompt 到指定会话（仅文本）
    ///
    /// 使用有界通道，提供背压保护。如果通道已满，会异步等待直到有空间。
    pub async fn send_text_prompt(&self, project_id: &str, prompt: &PromptMessage) -> Result<()> {
        let session = self
            .get_session(project_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", project_id))?;

        let prompt_request = Self::build_text_prompt_request(prompt, session.session_id().clone())?;

        session
            .prompt_tx()
            .send(prompt_request)
            .await
            .map_err(|e| {
                error!("send Prompt requestfailed: {:?}", e);
                anyhow::anyhow!("Failed to send Prompt request: {:?}", e)
            })?;

        info!("Prompt request already sent, project ID: {}", project_id);
        Ok(())
    }

    /// 发送 Prompt 请求到指定会话
    ///
    /// 使用有界通道，提供背压保护。如果通道已满，会异步等待直到有空间。
    pub async fn send_prompt_request(
        &self,
        project_id: &str,
        prompt_request: PromptRequest,
    ) -> Result<()> {
        let session = self
            .get_session(project_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", project_id))?;

        send_prompt_to_entry(&session, prompt_request)
            .await
            .map_err(|e| {
                error!("send Prompt requestfailed: {:?}", e);
                anyhow::anyhow!("Failed to send Prompt request: {:?}", e)
            })?;
        info!("Prompt request already sent, project ID: {}", project_id);
        Ok(())
    }
}

impl<N: SessionNotifier + 'static, R: SessionRegistry> std::fmt::Debug for AcpSessionManager<N, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpSessionManager")
            .field("session_count", &self.registry.count())
            .finish()
    }
}

/// 直接向 get_or_create_session 返回的 entry 发送 Prompt
///
/// 不按 project_id 重取——重取在"会话就绪到发送"之间开了 TOCTOU 窗口
/// （并发重建期间可能取到通道已死的其它 entry，且 prompt_request 内的
/// session_id 与实际通道不一致）。调用方持有刚裁决过的 entry，直发即可。
///
/// 返回裸 [`tokio::sync::mpsc::error::SendError`]：mpsc send 的失败原因
/// 唯一（接收端 drop/通道关闭，即连接任务已退出），调用方以类型本身
/// 判定"通道死亡"触发重建重试，无需 downcast。
pub(crate) async fn send_prompt_to_entry(
    session: &impl SessionEntry,
    prompt_request: PromptRequest,
) -> std::result::Result<(), tokio::sync::mpsc::error::SendError<PromptRequest>> {
    session.prompt_tx().send(prompt_request).await
}
