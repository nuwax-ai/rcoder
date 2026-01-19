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

use std::path::{Component, PathBuf};
use std::sync::Arc;

// 使用 SACP 类型
use sacp::schema::{ContentBlock, PromptRequest, SessionId, TextContent};
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
        Self {
            registry,
            notifier,
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

    /// 创建新的 Agent 会话 (SACP 版本)
    ///
    /// 启动 Agent 进程并建立 SACP 连接
    ///
    /// # 参数
    /// - `project_id`: 项目 ID
    /// - `project_path`: 项目路径
    /// - `model_provider`: 模型提供者配置
    /// - `start_config`: Agent 启动配置
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
        service_uuid: Option<String>,
    ) -> Result<R::Entry> {
        let agent_info = self
            .create_session_internal(
                project_id.clone(),
                project_path,
                model_provider,
                start_config,
                service_uuid,
            )
            .await?;

        // 存储会话信息到 registry
        let session_id_str = agent_info.session_id().to_string();
        self.registry
            .insert(&project_id, &session_id_str, agent_info.clone());

        Ok(agent_info)
    }

    /// 内部方法：创建 Agent 会话但不插入到 registry
    ///
    /// 用于 entry API 优化，避免重复插入
    async fn create_session_internal(
        &self,
        project_id: String,
        project_path: PathBuf,
        model_provider: Option<ModelProviderConfig>,
        start_config: AgentStartConfig,
        service_uuid: Option<String>,
    ) -> Result<R::Entry> {
        info!("开始创建新的 Agent 会话，项目 ID: {}", project_id);

        // 创建 SACP 启动器
        let launcher = ClaudeCodeLauncher::new(self.notifier.clone());

        // 记录是否使用了 resume（仅用于日志）
        let has_resume = start_config.resume_session_id.is_some();
        if has_resume {
            info!(
                "📌 使用 resume 启动 Agent: session_id={:?}",
                start_config.resume_session_id
            );
        }

        // 启动 Agent (SACP 版本)
        // 如果 resume 失败，直接返回错误，让上层（rcoder）决定是否降级重试
        let connection_info = launcher
            .launch(
                project_id.clone(),
                project_path.clone(),
                model_provider.clone(),
                start_config.clone(),
                self.registry.clone(),
                service_uuid,
            )
            .await?;

        info!(
            "✅ Agent 会话创建成功，会话 ID: {}",
            connection_info.session_id
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

        // 返回 agent_info（不插入 registry，由调用方处理）
        Ok(agent_info.into())
    }

    /// 获取或创建会话 (SACP 版本)
    ///
    /// 如果会话已存在且模型配置未变化，则复用；否则创建新会话
    ///
    /// # 优化说明
    /// 使用 DashMap 的 entry API 进行原子性操作，将 DashMap 访问次数从 3-4 次减少到 1 次
    ///
    /// # 参数
    /// - `project_id`: 项目 ID
    /// - `project_path`: 项目路径
    /// - `session_id_hint`: 会话 ID 提示（用于恢复）
    /// - `model_provider`: 模型提供者配置
    /// - `start_config`: Agent 启动配置
    /// - `service_uuid`: 与此 Agent 关联的唯一 UUID
    ///
    /// # 返回值
    /// - `R::Entry`: 会话条目
    /// - `bool`: 是否是新创建的会话
    pub async fn get_or_create_session(
        &self,
        project_id: &str,
        project_path: PathBuf,
        _session_id_hint: Option<String>, // 保留用于未来扩展（resume 逻辑在上层处理）
        model_provider: Option<ModelProviderConfig>,
        start_config: AgentStartConfig,
        service_uuid: Option<String>,
    ) -> Result<(R::Entry, bool)> {
        use dashmap::mapref::entry::Entry;

        // 🔥 使用 entry API 进行原子性操作（一次 DashMap 访问）
        match self.registry.entry(project_id.to_string()) {
            Entry::Occupied(mut occupied_entry) => {
                let existing = occupied_entry.get();

                // 🔧 关键检查 1：检查 channel 是否仍然有效（Agent 进程是否还在运行）
                let channel_closed = existing.is_channel_closed();
                // 🔧 关键检查 2：检查模型配置是否变化
                let model_changed = existing.is_model_config_changed(&model_provider);

                let needs_rebuild = channel_closed || model_changed;

                if needs_rebuild {
                    if channel_closed {
                        info!(
                            "⚠️ 检测到会话 channel 已关闭（Agent 进程已退出），重建会话，项目 ID: {}",
                            project_id
                        );
                    }
                    if model_changed {
                        info!(
                            "检测到模型配置变化，重启 Agent 会话，项目 ID: {}, 旧配置: {:?}, 新配置: {:?}",
                            project_id,
                            existing.model_provider(),
                            model_provider
                        );
                    }

                    // 创建新会话（不插入 registry）
                    let new_session = self
                        .create_session_internal(
                            project_id.to_string(),
                            project_path,
                            model_provider,
                            start_config,
                            service_uuid,
                        )
                        .await?;

                    // 直接在原地更新（无需额外查找）
                    let session_id_str = new_session.session_id().to_string();
                    occupied_entry.insert(new_session.clone());
                    info!(
                        "✅ 已重建会话，项目 ID: {}, session_id: {}",
                        project_id, session_id_str
                    );

                    Ok((new_session, true)) // true = 新创建
                } else {
                    info!("复用现有 Agent 会话，项目 ID: {}", project_id);
                    Ok((existing.clone(), false)) // false = 复用
                }
            }
            Entry::Vacant(vacant_entry) => {
                info!("会话不存在，创建新会话，项目 ID: {}", project_id);

                // 创建新会话（不插入 registry）
                let new_session = self
                    .create_session_internal(
                        project_id.to_string(),
                        project_path,
                        model_provider,
                        start_config,
                        service_uuid,
                    )
                    .await?;

                // 直接插入到 vacant entry
                let session_id_str = new_session.session_id().to_string();
                vacant_entry.insert(new_session.clone());
                info!(
                    "✅ 新会话创建完成，项目 ID: {}, session_id: {}",
                    project_id, session_id_str
                );

                Ok((new_session, true)) // true = 新创建
            }
        }
    }

    /// 发送 Prompt 到指定会话（仅文本）
    ///
    /// 使用有界通道，提供背压保护。如果通道已满，会异步等待直到有空间。
    pub async fn send_text_prompt(&self, project_id: &str, prompt: &PromptMessage) -> Result<()> {
        let session = self
            .get_session(project_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", project_id))?;

        let prompt_request = Self::build_text_prompt_request(prompt, session.session_id().clone())?;

        session.prompt_tx()
            .send(prompt_request)
            .await
            .map_err(|e| {
                error!("发送 Prompt 请求失败: {:?}", e);
                anyhow::anyhow!("发送 Prompt 请求失败: {:?}", e)
            })?;

        info!("✅ Prompt 请求已发送，项目 ID: {}", project_id);
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

        session.prompt_tx()
            .send(prompt_request)
            .await
            .map_err(|e| {
                error!("发送 Prompt 请求失败: {:?}", e);
                anyhow::anyhow!("发送 Prompt 请求失败: {:?}", e)
            })?;

        info!("✅ Prompt 请求已发送，项目 ID: {}", project_id);
        Ok(())
    }
}

impl<N: SessionNotifier + 'static, R: SessionRegistry> std::fmt::Debug
    for AcpSessionManager<N, R>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpSessionManager")
            .field("session_count", &self.registry.count())
            .finish()
    }
}
