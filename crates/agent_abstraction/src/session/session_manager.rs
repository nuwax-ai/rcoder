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
use agent_config::PromptBuilder;
use anyhow::Result;
use chrono::Utc;
use sacp::schema::{ContentBlock, PromptRequest, SessionId, TextContent};
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
        Self { registry, notifier }
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
 info!("startingcreated message Agent session, project ID: {}", project_id);

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
    /// 获取或创建会话
    ///
    /// # 参数
    /// - `project_id`: 项目 ID
    /// - `project_path`: 项目路径
    /// - `session_id_hint`: 会话 ID 提示（用于恢复现有会话）
    /// - `model_provider`: 模型提供者配置
    /// - `start_config`: Agent 启动配置
    /// - `service_uuid`: 与此 Agent 关联的唯一 UUID
    ///
    /// # 返回值
    /// - `R::Entry`: 会话条目
    /// - `bool`: 是否是新创建的会话
    ///
    /// # 并发安全性
    ///
    /// 使用"检查-创建-插入"三阶段模式避免在持有 entry 期间调用 `.await`：
    ///
    /// 1. **快速检查**：检查会话是否已存在且有效
    /// 2. **创建会话**：如果需要创建，在**不持有锁**的情况下创建会话（.await）
    /// 3. **原子性插入**：使用 entry API 原子性插入，如果其他线程已创建则使用已存在的
    ///
    /// 这样确保：
    /// - 不会在持有 DashMap entry 期间跨越 await 点
    /// - 同一 project_id 最多只会创建一个会话
    /// - 高并发下不会阻塞其他 project_id 的访问（DashMap 分段锁特性）
    pub async fn get_or_create_session(
        &self,
        project_id: &str,
        project_path: PathBuf,
        session_id_hint: Option<String>, // 🔥 修复：使用此参数查找现有会话
        model_provider: Option<ModelProviderConfig>,
        start_config: AgentStartConfig,
        service_uuid: Option<String>,
    ) -> Result<(R::Entry, bool)> {
        use dashmap::mapref::entry::Entry;

        let project_id_key = project_id.to_string();

        // 🆕 第一阶段：如果提供了 session_id_hint，先尝试通过 session_id 查找现有会话
        // 🔥 优化：使用 get_entry_by_session() 避免两次调用之间的竞态窗口
        if let Some(ref hint_sid) = session_id_hint {
            // 一次性查询：通过 session_id 直接获取 agent_info
            if let Some(existing) = self.registry.get_entry_by_session(hint_sid) {
                // 验证 project_id 是否匹配（防御性编程）
                if existing.project_id() == project_id {
                    let channel_closed = existing.is_channel_closed();
                    let model_changed = existing.is_model_config_changed(&model_provider);

                    if !channel_closed && !model_changed {
                        info!(
                            "[SESSION] 通过 session_id_hint 复用现有会话: project_id={}, session_id={}",
                            project_id, hint_sid
                        );
                        return Ok((existing, false));
                    }

                    if channel_closed {
                        info!(
                            "⚠️ [SESSION] session_id_hint 对应的会话 channel 已关闭，需要重建: project_id={}, session_id={}",
                            project_id, hint_sid
                        );
                    }
                    if model_changed {
                        info!(
                            "🔄 [SESSION] session_id_hint 对应的会话模型配置变化，需要重建: project_id={}, session_id={}",
                            project_id, hint_sid
                        );
                    }
                    // 会话无效，继续后续逻辑（可能需要重建）
                } else {
                    info!(
                        "⚠️ [SESSION] session_id_hint 属于不同的 project: hint_project={}, current_project={}, session_id={}",
                        existing.project_id(),
                        project_id,
                        hint_sid
                    );
                    // session_id 属于其他 project，不能复用，继续后续逻辑
                }
            } else {
                info!(
                    "🔍 [SESSION] session_id_hint 不存在于 registry: session_id={}",
                    hint_sid
                );
                // session_id 不存在，继续后续逻辑
            }
        }

        // 第一阶段：快速检查现有会话
        if let Entry::Occupied(occupied_entry) = self.registry.entry(project_id_key.clone()) {
            let existing = occupied_entry.get();

            // 🔥 显式检查 Pending 状态 - PendingGuard 创建的占位符需要被替换
            if *existing.status() == AgentStatus::Pending {
                info!(
                    "🔄 [SESSION] 检测到 Pending 占位符，准备替换: project_id={}",
                    project_id
                );
                // 显式释放 entry 锁
                drop(occupied_entry);

                // 第二阶段：创建新会话（不持有锁）
                let new_session = self
                    .create_session_internal(
                        project_id_key.clone(),
                        project_path,
                        model_provider,
                        start_config,
                        service_uuid,
                    )
                    .await?;

                // 第三阶段：原子性替换（插入新会话，覆盖 pending 占位符）
                match self.registry.entry(project_id_key.clone()) {
                    Entry::Vacant(entry) => {
                        let new_session_id = new_session.session_id().to_string();
                        entry.insert(new_session.clone());
                        info!(
                            "✅ [SESSION] 插入新会话替换 pending: project_id={}, session_id={}",
                            project_id, new_session_id
                        );
                        return Ok((new_session, true));
                    }
                    Entry::Occupied(mut entry) => {
                        // 检查是 Pending 占位符还是其他线程创建的真实会话
                        let current_session = entry.get();
                        let current_status = current_session.status();

                        if *current_status == AgentStatus::Pending {
                            // 仍然是 Pending 占位符，替换它
                            let old_session_id = current_session.session_id().to_string();
                            let new_session_id = new_session.session_id().to_string();
                            entry.insert(new_session.clone());
                            info!(
                                "✅ [SESSION] 替换 pending 占位符: project_id={}, {} → {}",
                                project_id, old_session_id, new_session_id
                            );
                            return Ok((new_session, true));
                        } else {
                            // 其他线程已经创建了真实会话，使用已存在的（丢弃我们创建的）
                            let existing_session = current_session.clone();
                            let created_session_id = new_session.session_id().to_string();
                            let existing_session_id = existing_session.session_id().to_string();
                            info!(
                                "🔄 [SESSION] 检测到并发创建（其他线程已创建真实会话）: project_id={}, 丢弃 session_id={}, 使用 session_id={}",
                                project_id, created_session_id, existing_session_id
                            );
                            // new_session 会被 drop，AgentLifecycleGuard 会清理 Agent 进程
                            return Ok((existing_session, false));
                        }
                    }
                }
            }

            let channel_closed = existing.is_channel_closed();
            let model_changed = existing.is_model_config_changed(&model_provider);

            if !channel_closed && !model_changed {
 info!("reuse message Agent session, project ID: {}", project_id);
                return Ok((existing.clone(), false));
            }

            // 需要重建会话，先克隆必要数据并释放锁
            let session_id_str = existing.session_id().to_string();
            drop(occupied_entry); // 显式释放 entry 锁

            if channel_closed {
                info!(
                    "⚠️ 检测到会话 channel 已关闭（Agent 进程已退出），重建会话，项目 ID: {}, 旧 session_id: {}",
                    project_id, session_id_str
                );
            }
            if model_changed {
                info!(
                    "检测到模型配置变化，重启 Agent 会话，项目 ID: {}, 旧 session_id: {}",
                    project_id, session_id_str
                );
            }

            // 第二阶段：在不持有锁的情况下创建新会话
            let new_session = self
                .create_session_internal(
                    project_id_key.clone(),
                    project_path,
                    model_provider,
                    start_config,
                    service_uuid,
                )
                .await?;

            // 第三阶段：原子性插入（使用 entry API 防止并发创建）
            match self.registry.entry(project_id_key.clone()) {
                Entry::Vacant(entry) => {
                    // 其他线程还没有创建，使用我们创建的会话
                    let new_session_id = new_session.session_id().to_string();
                    entry.insert(new_session.clone());
                    info!(
                        "✅ 已重建会话，项目 ID: {}, 新 session_id: {}",
                        project_id, new_session_id
                    );
                    return Ok((new_session, true));
                }
                Entry::Occupied(entry) => {
                    // 其他线程已经创建了会话，使用已存在的（丢弃我们创建的）
                    let existing_session = entry.get().clone();
                    let created_session_id = new_session.session_id().to_string();
                    let existing_session_id = existing_session.session_id().to_string();
                    info!(
                        "🔄 [SESSION] 检测到并发创建，使用其他线程创建的会话: project_id={}, 丢弃 session_id={}, 使用 session_id={}",
                        project_id, created_session_id, existing_session_id
                    );
                    // new_session 会被 drop，Session 的 Drop 实现会清理 Agent 进程
                    return Ok((existing_session, false));
                }
            }
        }

        // 会话不存在，需要创建新会话
 info!("sessionnot found, created message session, project ID: {}", project_id);

        // 第二阶段：在不持有锁的情况下创建新会话
        let new_session = self
            .create_session_internal(
                project_id_key.clone(),
                project_path,
                model_provider,
                start_config,
                service_uuid,
            )
            .await?;

        // 第三阶段：原子性插入
        match self.registry.entry(project_id_key.clone()) {
            Entry::Vacant(entry) => {
                // 其他线程还没有创建，使用我们创建的会话
                let session_id_str = new_session.session_id().to_string();
                entry.insert(new_session.clone());
                info!(
                    "✅ 新会话创建完成，项目 ID: {}, session_id: {}",
                    project_id, session_id_str
                );
                Ok((new_session, true))
            }
            Entry::Occupied(entry) => {
                // 其他线程已经创建了会话，使用已存在的（丢弃我们创建的）
                let existing_session = entry.get().clone();
                let created_session_id = new_session.session_id().to_string();
                let existing_session_id = existing_session.session_id().to_string();
                info!(
                    "🔄 [SESSION] 检测到并发创建，使用其他线程创建的会话: project_id={}, 丢弃 session_id={}, 使用 session_id={}",
                    project_id, created_session_id, existing_session_id
                );
                // new_session 会被 drop，Session 的 Drop 实现会清理 Agent 进程
                Ok((existing_session, false))
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

        session
            .prompt_tx()
            .send(prompt_request)
            .await
            .map_err(|e| {
 error!("send Prompt requestfailed: {:?}", e);
                anyhow::anyhow!("发送 Prompt 请求失败: {:?}", e)
            })?;

 info!("Prompt requestalreadysend, project ID: {}", project_id);
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

        session
            .prompt_tx()
            .send(prompt_request)
            .await
            .map_err(|e| {
 error!("send Prompt requestfailed: {:?}", e);
                anyhow::anyhow!("发送 Prompt 请求失败: {:?}", e)
            })?;

 info!("Prompt requestalreadysend, project ID: {}", project_id);
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
