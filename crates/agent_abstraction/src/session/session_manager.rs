//! ACP Session Manager
//!
//! 基于 project_id 管理 ACP 会话的核心模块

use std::{
    path::{Component, PathBuf},
    sync::Arc,
};

use agent_client_protocol::{Client, ContentBlock, PromptRequest, SessionId, TextContent};
use agent_config::PromptBuilder;
use anyhow::Result;
use chrono::Utc;
use dashmap::DashMap;
use shared_types::{AgentLifecycle, AgentStatus, ModelProviderConfig};
use tracing::{debug, error, info};

use super::SessionInfo;
use crate::PromptMessage;
use crate::compat::ClaudeCodeLauncher;
use crate::traits::{AgentStartConfig, SessionNotifier};

/// ACP 会话管理器
///
/// 管理所有活跃的 ACP 会话，提供：
/// - 会话创建和复用
/// - 模型配置变化检测
/// - 会话生命周期管理
pub struct AcpSessionManager<N: SessionNotifier, C: Client + 'static> {
    /// project_id -> SessionInfo 映射
    sessions: DashMap<String, Arc<SessionInfo>>,
    /// 会话通知器
    notifier: Arc<N>,
    /// Client 类型标记（phantom data）
    _client_marker: std::marker::PhantomData<C>,
}

impl<N: SessionNotifier, C: Client + Default + 'static> AcpSessionManager<N, C> {
    /// 创建新的会话管理器
    pub fn new(notifier: Arc<N>) -> Self {
        Self {
            sessions: DashMap::new(),
            notifier,
            _client_marker: std::marker::PhantomData,
        }
    }

    /// 获取会话信息
    pub fn get_session(&self, project_id: &str) -> Option<Arc<SessionInfo>> {
        self.sessions.get(project_id).map(|r| r.clone())
    }

    /// 检查会话是否存在
    pub fn contains_session(&self, project_id: &str) -> bool {
        self.sessions.contains_key(project_id)
    }

    /// 移除会话
    pub fn remove_session(&self, project_id: &str) -> Option<Arc<SessionInfo>> {
        self.sessions.remove(project_id).map(|(_, v)| v)
    }

    /// 插入会话信息
    pub fn insert_session(&self, project_id: String, session_info: Arc<SessionInfo>) {
        self.sessions.insert(project_id, session_info);
    }

    /// 获取所有会话的 project_id 列表
    pub fn list_sessions(&self) -> Vec<String> {
        self.sessions.iter().map(|r| r.key().clone()).collect()
    }

    /// 获取会话数量
    pub fn session_count(&self) -> usize {
        self.sessions.len()
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
    pub async fn create_session(
        &self,
        project_id: String,
        project_path: PathBuf,
        session_id_hint: Option<String>,
        model_provider: Option<ModelProviderConfig>,
        start_config: AgentStartConfig,
        client: C,
    ) -> Result<Arc<SessionInfo>> {
        info!("开始创建新的 Agent 会话，项目 ID: {}", project_id);

        // 创建启动器
        let launcher = ClaudeCodeLauncher::new(self.notifier.clone());

        // 记录是否使用了 resume
        let has_resume = start_config.resume_session_id.is_some();

        // 第一次尝试：使用原始配置（可能包含 --resume）
        let result = launcher
            .launch(
                project_id.clone(),
                project_path.clone(),
                session_id_hint.clone(),
                model_provider.clone(),
                start_config.clone(),
                client,
            )
            .await;

        // 简单重试机制：如果启动失败且使用了 --resume，去掉 --resume 重试
        let connection_info = match result {
            Ok(info) => info,
            Err(e) => {
                let error_msg = format!("{:?}", e);

                // 检查是否因为 resume 导致的失败
                if has_resume
                    && (error_msg.contains("No conversation found")
                        || error_msg.contains("session")
                        || error_msg.contains("exited with code 1"))
                {
                    tracing::warn!(
                        "⚠️ Agent 启动失败（可能因 --resume），重试不使用 --resume: error={}",
                        error_msg
                    );

                    // 创建新的 config，不包含 resume_session_id
                    let retry_config = AgentStartConfig {
                        system_prompt: start_config.system_prompt,
                        mcp_servers: start_config.mcp_servers,
                        extra_meta: start_config.extra_meta,
                        service_type: start_config.service_type,
                        resume_session_id: None, // ✅ 去掉 resume
                    };

                    tracing::info!("🔄 重试启动 Agent（不使用 --resume）");

                    // 重试启动（创建新的 client 实例）
                    launcher
                        .launch(
                            project_id.clone(),
                            project_path,
                            session_id_hint,
                            model_provider.clone(),
                            retry_config,
                            C::default(), // 创建新的 client 实例
                        )
                        .await?
                } else {
                    // 其他错误，直接返回
                    return Err(e);
                }
            }
        };

        info!(
            "✅ Agent 会话创建成功，会话 ID: {}",
            connection_info.session_id.0
        );

        // 创建 SessionInfo
        let lifecycle_handle =
            Some(connection_info.lifecycle_guard.clone() as Arc<dyn AgentLifecycle>);
        let session_info = Arc::new(SessionInfo::new(
            project_id.clone(),
            connection_info.session_id,
            connection_info.prompt_tx,
            connection_info.cancel_tx,
            model_provider,
            lifecycle_handle,
        ));

        // 存储会话信息
        self.sessions.insert(project_id, session_info.clone());

        Ok(session_info)
    }

    /// 获取或创建会话
    ///
    /// 如果会话已存在且模型配置未变化，则复用；否则创建新会话
    pub async fn get_or_create_session(
        &self,
        project_id: &str,
        project_path: PathBuf,
        session_id_hint: Option<String>,
        model_provider: Option<ModelProviderConfig>,
        start_config: AgentStartConfig,
        client: C,
    ) -> Result<(Arc<SessionInfo>, bool)> {
        // 检查是否存在
        if let Some(existing) = self.get_session(project_id) {
            // 检查模型配置是否变化
            if existing.is_model_config_changed(&model_provider) {
                info!(
                    "检测到模型配置变化，重启 Agent 会话，项目 ID: {}, 旧配置: {:?}, 新配置: {:?}",
                    project_id, existing.model_provider, model_provider
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
            )
            .await?;
        Ok((new_session, true))
    }

    /// 发送 Prompt 到指定会话（仅文本）
    pub fn send_text_prompt(&self, project_id: &str, prompt: &PromptMessage) -> Result<()> {
        let session = self
            .get_session(project_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", project_id))?;

        let prompt_request = Self::build_text_prompt_request(prompt, session.session_id.clone())?;

        session.prompt_tx.send(prompt_request).map_err(|e| {
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

        session.prompt_tx.send(prompt_request).map_err(|e| {
            error!("发送 Prompt 请求失败: {:?}", e);
            anyhow::anyhow!("发送 Prompt 请求失败: {:?}", e)
        })?;

        info!("✅ Prompt 请求已发送，项目 ID: {}", project_id);
        Ok(())
    }
}

impl<N: SessionNotifier, C: Client + Default + 'static> std::fmt::Debug
    for AcpSessionManager<N, C>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpSessionManager")
            .field("session_count", &self.sessions.len())
            .finish()
    }
}
