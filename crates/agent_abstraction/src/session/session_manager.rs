//! ACP Session Manager
//!
//! 基于 project_id 管理 ACP 会话的核心模块
//!
//! ## 架构说明
//!
//! 使用依赖注入的 `SessionRegistry` 进行会话存储，消除了之前的重复数据结构：
//! - 之前：`AcpSessionManager.sessions` 和 `AGENT_REGISTRY` 各自维护一份会话数据
//! - 现在：统一使用注入的 `SessionRegistry`（通常是 `AGENT_REGISTRY`）
//!
//! 这样确保了：
//! - 当 Agent 被停止时（通过 `stop_agent` 调用 `AGENT_REGISTRY.remove`），
//!   `AcpSessionManager` 自然就看不到该会话了
//! - 不再需要手动同步两个存储

use std::path::{Component, PathBuf};
use std::sync::Arc;

use dashmap::DashMap;

use agent_client_protocol::{Client, ContentBlock, PromptRequest, SessionId, TextContent};
use agent_config::PromptBuilder;
use anyhow::Result;
use chrono::Utc;
use shared_types::{
    AgentLifecycle, AgentStatus, ModelProviderConfig, ProjectAndAgentInfo, SessionEntry,
};
use tracing::{debug, error, info};

use crate::PromptMessage;
use crate::launcher::ClaudeCodeLauncher;
use crate::traits::{AgentStartConfig, SessionNotifier, SessionRegistry};

/// ACP 会话管理器
///
/// 管理所有活跃的 ACP 会话，提供：
/// - 会话创建和复用
/// - 模型配置变化检测
/// - 会话生命周期管理
///
/// ## 泛型参数
/// - `N`: SessionNotifier 实现，用于推送 SSE 消息
/// - `C`: ACP Client 实现
/// - `R`: SessionRegistry 实现，用于存储会话数据（通常是 AGENT_REGISTRY）
pub struct AcpSessionManager<N: SessionNotifier, C: Client + 'static, R: SessionRegistry> {
    /// 会话注册表（注入的 SessionRegistry）
    registry: Arc<R>,
    /// 会话通知器
    notifier: Arc<N>,
    /// Client 类型标记（phantom data）
    _client_marker: std::marker::PhantomData<C>,
}

impl<N: SessionNotifier, C: Client + Default + 'static, R: SessionRegistry>
    AcpSessionManager<N, C, R>
where
    R::Entry: Into<ProjectAndAgentInfo> + From<ProjectAndAgentInfo>,
{
    /// 创建新的会话管理器
    ///
    /// # 参数
    /// - `notifier`: 会话通知器
    /// - `registry`: 会话注册表（通常注入 AGENT_REGISTRY）
    pub fn new(notifier: Arc<N>, registry: Arc<R>) -> Self {
        Self {
            registry,
            notifier,
            _client_marker: std::marker::PhantomData,
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
    pub fn normalize_path(path: &PathBuf) -> PathBuf {
        let joined_path = if path.is_absolute() {
            path.clone()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        };

        joined_path
            .components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect()
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
            "🔧 [build_prompt] 将 request_id={} 放入 PromptRequest.meta",
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
            "🔧 [build_prompt] 将 request_id={} 放入 PromptRequest.meta",
            prompt.request_id
        );
        let mut meta = serde_json::Map::new();
        meta.insert(
            "request_id".to_string(),
            serde_json::Value::String(prompt.request_id.clone()),
        );

        Ok(PromptRequest::new(session_id, content_blocks).meta(meta))
    }

    /// 创建新的 Agent 会话
    ///
    /// 启动 Agent 进程并建立 ACP 连接
    ///
    /// # 参数
    /// - `shared_api_key_manager`: 共享的 API 密钥管理器（用于自动清理）
    /// - `service_uuid`: 与此 Agent 关联的唯一 UUID
    ///
    /// # 返回值
    /// - `R::Entry`: 会话条目
    pub async fn create_session(
        &self,
        project_id: String,
        project_path: PathBuf,
        _session_id_hint: Option<String>,
        model_provider: Option<ModelProviderConfig>,
        start_config: AgentStartConfig,
        client: C,
        shared_api_key_manager: Option<Arc<DashMap<String, shared_types::ModelProviderConfig>>>,
        service_uuid: Option<String>,
    ) -> Result<R::Entry> {
        info!("开始创建新的 Agent 会话，项目 ID: {}", project_id);

        // 创建启动器
        let launcher = ClaudeCodeLauncher::new(self.notifier.clone());

        // 记录是否使用了 resume（仅用于日志）
        let has_resume = start_config.resume_session_id.is_some();
        if has_resume {
            info!(
                "📌 使用 resume 启动 Agent: session_id={:?}",
                start_config.resume_session_id
            );
        }

        // 启动 Agent，不再内部降级
        // 如果 resume 失败，直接返回错误，让上层（rcoder）决定是否降级重试
        let connection_info = launcher
            .launch(
                project_id.clone(),
                project_path.clone(),
                model_provider.clone(),
                start_config.clone(),
                client,
                self.registry.clone(),
                shared_api_key_manager,
                None,  // project_uuid_map 清理由 agent_runner 层负责
                service_uuid,
            )
            .await?;

        info!(
            "✅ Agent 会话创建成功，会话 ID: {}",
            connection_info.session_id.0
        );

        // 创建 ProjectAndAgentInfo
        let lifecycle_handle =
            Some(connection_info.lifecycle_guard.clone() as Arc<dyn AgentLifecycle>);
        let now = Utc::now();
        let agent_info = ProjectAndAgentInfo {
            project_id: project_id.clone(),
            session_id: connection_info.session_id.clone(),
            prompt_tx: connection_info.prompt_tx,
            cancel_tx: connection_info.cancel_tx,
            model_provider: model_provider.clone(),
            request_id: None,
            status: AgentStatus::Idle,
            last_activity: now,
            created_at: now,
            stop_handle: lifecycle_handle,
        };

        // 存储会话信息到 registry
        let session_id_str = connection_info.session_id.0.to_string();
        self.registry
            .insert(&project_id, &session_id_str, agent_info.clone().into());

        // 注意：降级处理已移至 launch() 的 spawn_local 块内
        // 通过 tokio::select! 在 LocalSet 中直接处理降级，避免跨线程问题
        // 详见 crates/agent_abstraction/src/launcher/claude_code.rs

        // 返回刚插入的条目
        Ok(agent_info.into())
    }

    /// 获取或创建会话
    ///
    /// 如果会话已存在且模型配置未变化，则复用；否则创建新会话
    ///
    /// # 参数
    /// - `shared_api_key_manager`: 共享的 API 密钥管理器（用于自动清理）
    /// - `service_uuid`: 与此 Agent 关联的唯一 UUID
    ///
    /// # 返回值
    /// - `R::Entry`: 会话条目
    /// - `bool`: 是否是新创建的会话
    pub async fn get_or_create_session(
        &self,
        project_id: &str,
        project_path: PathBuf,
        session_id_hint: Option<String>,
        model_provider: Option<ModelProviderConfig>,
        start_config: AgentStartConfig,
        client: C,
        shared_api_key_manager: Option<Arc<DashMap<String, shared_types::ModelProviderConfig>>>,
        service_uuid: Option<String>,
    ) -> Result<(R::Entry, bool)> {
        // 检查是否存在
        if let Some(existing) = self.get_session(project_id) {
            // 🔧 关键检查：检查 channel 是否仍然有效（Agent 进程是否还在运行）
            // 如果 prompt_tx.is_closed() 为 true，说明 rx 端已被 drop（Agent 进程已退出）
            if existing.is_channel_closed() {
                info!(
                    "⚠️ 检测到会话 channel 已关闭（Agent 进程已退出），移除失效会话并重建，项目 ID: {}",
                    project_id
                );
                // 移除失效会话
                self.remove_session(project_id);
                // 创建新会话
                let new_session = self
                    .create_session(
                        project_id.to_string(),
                        project_path,
                        session_id_hint,
                        model_provider,
                        start_config,
                        client,
                        shared_api_key_manager.clone(),
                        service_uuid.clone(),
                    )
                    .await?;
                return Ok((new_session, true)); // true = 新创建
            }

            // 检查模型配置是否变化
            if existing.is_model_config_changed(&model_provider) {
                info!(
                    "检测到模型配置变化，重启 Agent 会话，项目 ID: {}, 旧配置: {:?}, 新配置: {:?}",
                    project_id,
                    existing.model_provider(),
                    model_provider
                );
                // 移除旧会话
                self.remove_session(project_id);
                // 创建新会话
                let new_session = self
                    .create_session(
                        project_id.to_string(),
                        project_path,
                        session_id_hint,
                        model_provider,
                        start_config,
                        client,
                        shared_api_key_manager.clone(),
                        service_uuid.clone(),
                    )
                    .await?;
                return Ok((new_session, true)); // true = 新创建
            }

            info!("复用现有 Agent 会话，项目 ID: {}", project_id);
            return Ok((existing, false)); // false = 复用
        }

        // 创建新会话
        let new_session = self
            .create_session(
                project_id.to_string(),
                project_path,
                session_id_hint,
                model_provider,
                start_config,
                client,
                shared_api_key_manager,
                service_uuid,
            )
            .await?;
        Ok((new_session, true))
    }

    /// 发送 Prompt 到指定会话（仅文本）
    pub fn send_text_prompt(&self, project_id: &str, prompt: &PromptMessage) -> Result<()> {
        let session = self
            .get_session(project_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", project_id))?;

        let prompt_request = Self::build_text_prompt_request(prompt, session.session_id().clone())?;

        session.prompt_tx().send(prompt_request).map_err(|e| {
            error!("发送 Prompt 请求失败: {:?}", e);
            anyhow::anyhow!("发送 Prompt 请求失败: {:?}", e)
        })?;

        info!("✅ Prompt 请求已发送，项目 ID: {}", project_id);
        Ok(())
    }

    /// 发送 Prompt 请求到指定会话
    pub fn send_prompt_request(
        &self,
        project_id: &str,
        prompt_request: PromptRequest,
    ) -> Result<()> {
        let session = self
            .get_session(project_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", project_id))?;

        session.prompt_tx().send(prompt_request).map_err(|e| {
            error!("发送 Prompt 请求失败: {:?}", e);
            anyhow::anyhow!("发送 Prompt 请求失败: {:?}", e)
        })?;

        info!("✅ Prompt 请求已发送，项目 ID: {}", project_id);
        Ok(())
    }
}

impl<N: SessionNotifier, C: Client + Default + 'static, R: SessionRegistry> std::fmt::Debug
    for AcpSessionManager<N, C, R>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpSessionManager")
            .field("session_count", &self.registry.count())
            .finish()
    }
}
