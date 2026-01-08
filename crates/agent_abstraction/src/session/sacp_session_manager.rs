//! # SACP Session Manager
//!
//! 基于 SACP (symposium-acp) 的会话管理器。
//!
//! 与 `AcpSessionManager` 的主要区别：
//! - 使用 `SacpClaudeCodeLauncher` 替代 `ClaudeCodeLauncher`
//! - 支持标准 `tokio::spawn`（无需 LocalSet）
//! - 所有类型实现 `Send`
//!
//! ## Feature Flag
//!
//! 此模块通过 `sacp` feature 启用。

use std::path::{Component, PathBuf};
use std::sync::Arc;

use agent_config::PromptBuilder;
use anyhow::Result;
use chrono::Utc;
use sacp::schema::{ContentBlock, PromptRequest, SessionId, TextContent};
use shared_types::{
    AgentLifecycle, AgentStatus, ModelProviderConfig, ProjectAndAgentInfo, SessionEntry,
};
use tracing::{debug, error, info};

use crate::PromptMessage;
use crate::launcher::SacpClaudeCodeLauncher;
use crate::traits::{AgentStartConfig, SessionNotifier, SessionRegistry};

/// SACP 会话管理器
///
/// 使用 SACP 库的会话管理器，支持标准 `tokio::spawn`。
///
/// ## 泛型参数
/// - `N`: SessionNotifier 实现，用于推送 SSE 消息
/// - `R`: SessionRegistry 实现，用于存储会话数据
pub struct SacpSessionManager<N: SessionNotifier, R: SessionRegistry> {
    /// 会话注册表（注入的 SessionRegistry）
    registry: Arc<R>,
    /// 会话通知器
    notifier: Arc<N>,
}

impl<N: SessionNotifier + 'static, R: SessionRegistry> SacpSessionManager<N, R>
where
    R::Entry: Into<ProjectAndAgentInfo> + From<ProjectAndAgentInfo>,
{
    /// 创建新的会话管理器
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
            info!("[SACP] Project path does not exist, creating: {:?}", path);
            tokio::fs::create_dir_all(path).await?;
        }
        Ok(())
    }

    /// 构建基础 Prompt 请求（仅文本内容）
    pub fn build_text_prompt_request(
        prompt: &PromptMessage,
        session_id: SessionId,
    ) -> Result<PromptRequest> {
        let final_prompt = if prompt.data_source_attachments.is_empty() {
            PromptBuilder::build_user_prompt(&prompt.content)
        } else {
            PromptBuilder::build_user_prompt_with_data_sources(
                &prompt.content,
                &prompt.data_source_attachments,
            )
        };

        let text_block = ContentBlock::Text(TextContent::new(final_prompt));
        let content_blocks = vec![text_block];

        debug!(
            "[SACP] 将 request_id={} 放入 PromptRequest.meta",
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
    pub fn build_prompt_request_with_attachments(
        prompt: &PromptMessage,
        session_id: SessionId,
        attachment_blocks: Vec<ContentBlock>,
    ) -> Result<PromptRequest> {
        let final_prompt = if prompt.data_source_attachments.is_empty() {
            PromptBuilder::build_user_prompt(&prompt.content)
        } else {
            PromptBuilder::build_user_prompt_with_data_sources(
                &prompt.content,
                &prompt.data_source_attachments,
            )
        };

        let text_block = ContentBlock::Text(TextContent::new(final_prompt));
        let mut content_blocks = vec![text_block];
        content_blocks.extend(attachment_blocks);

        debug!(
            "[SACP] 将 request_id={} 放入 PromptRequest.meta",
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
    /// 使用 SACP 启动器，支持标准 tokio::spawn
    pub async fn create_session(
        &self,
        project_id: String,
        project_path: PathBuf,
        _session_id_hint: Option<String>,
        model_provider: Option<ModelProviderConfig>,
        start_config: AgentStartConfig,
        service_uuid: Option<String>,
    ) -> Result<R::Entry> {
        info!("[SACP] 开始创建新的 Agent 会话，项目 ID: {}", project_id);

        // 创建 SACP 启动器
        let launcher = SacpClaudeCodeLauncher::new(self.notifier.clone());

        // 记录是否使用了 resume
        if start_config.resume_session_id.is_some() {
            info!(
                "[SACP] 使用 resume 启动 Agent: session_id={:?}",
                start_config.resume_session_id
            );
        }

        // 🔥 使用 SACP 启动器（支持标准 tokio::spawn）
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
            "[SACP] Agent 会话创建成功，会话 ID: {}",
            connection_info.session_id
        );

        // 创建 ProjectAndAgentInfo
        let lifecycle_handle =
            Some(connection_info.lifecycle_guard.clone() as Arc<dyn AgentLifecycle>);
        let now = Utc::now();

        // 使用 sacp::schema::SessionId
        let sacp_session_id =
            SessionId::new(Arc::from(connection_info.session_id.to_string().as_str()));

        let agent_info = ProjectAndAgentInfo {
            project_id: project_id.clone(),
            session_id: sacp_session_id,
            prompt_tx: connection_info.prompt_tx,
            cancel_tx: connection_info.cancel_tx,
            model_provider: model_provider.clone(),
            request_id: None,
            status: AgentStatus::Idle,
            last_activity: now,
            created_at: now,
            stop_handle: lifecycle_handle,
            agent_server_config: start_config.agent_server_override.clone(),
        };

        // 存储会话信息到 registry
        let session_id_str = connection_info.session_id.to_string();
        self.registry
            .insert(&project_id, &session_id_str, agent_info.clone().into());

        Ok(agent_info.into())
    }

    /// 获取或创建会话
    pub async fn get_or_create_session(
        &self,
        project_id: &str,
        project_path: PathBuf,
        session_id_hint: Option<String>,
        model_provider: Option<ModelProviderConfig>,
        start_config: AgentStartConfig,
        service_uuid: Option<String>,
    ) -> Result<(R::Entry, bool)> {
        // 检查是否存在
        if let Some(existing) = self.get_session(project_id) {
            // 检查 channel 是否仍然有效
            if existing.is_channel_closed() {
                info!(
                    "[SACP] 检测到会话 channel 已关闭，移除失效会话并重建，项目 ID: {}",
                    project_id
                );
                self.remove_session(project_id);
                let new_session = self
                    .create_session(
                        project_id.to_string(),
                        project_path,
                        session_id_hint,
                        model_provider,
                        start_config,
                        service_uuid,
                    )
                    .await?;
                return Ok((new_session, true));
            }

            // 检查模型配置是否变化
            if existing.is_model_config_changed(&model_provider) {
                info!(
                    "[SACP] 检测到模型配置变化，重启 Agent 会话，项目 ID: {}",
                    project_id
                );
                self.remove_session(project_id);
                let new_session = self
                    .create_session(
                        project_id.to_string(),
                        project_path,
                        session_id_hint,
                        model_provider,
                        start_config,
                        service_uuid,
                    )
                    .await?;
                return Ok((new_session, true));
            }

            // 🎯 检查是否有自定义 agent_server 配置且配置发生变化
            if existing.is_agent_server_config_changed(&start_config.agent_server_override) {
                info!(
                    "[SACP] 检测到 Agent 配置变化，重启会话以使用新 Agent，项目 ID: {}, 旧配置: {:?}, 新配置: {:?}",
                    project_id,
                    existing.agent_server_config().map(|c| c.get_agent_id()),
                    start_config
                        .agent_server_override
                        .as_ref()
                        .map(|c| c.get_agent_id())
                );
                self.remove_session(project_id);
                let new_session = self
                    .create_session(
                        project_id.to_string(),
                        project_path,
                        session_id_hint,
                        model_provider,
                        start_config,
                        service_uuid,
                    )
                    .await?;
                return Ok((new_session, true));
            }

            info!("[SACP] 复用现有 Agent 会话，项目 ID: {}", project_id);
            return Ok((existing, false));
        }

        // 创建新会话
        let new_session = self
            .create_session(
                project_id.to_string(),
                project_path,
                session_id_hint,
                model_provider,
                start_config,
                service_uuid,
            )
            .await?;
        Ok((new_session, true))
    }

    /// 发送 Prompt 请求到指定会话
    pub fn send_prompt_request(
        &self,
        project_id: &str,
        prompt_request: PromptRequest,
    ) -> Result<()> {
        let session = self
            .get_session(project_id)
            .ok_or_else(|| anyhow::anyhow!("[SACP] Session not found: {}", project_id))?;

        session.prompt_tx().send(prompt_request).map_err(|e| {
            error!("[SACP] 发送 Prompt 请求失败: {:?}", e);
            anyhow::anyhow!("[SACP] 发送 Prompt 请求失败: {:?}", e)
        })?;

        info!("[SACP] Prompt 请求已发送，项目 ID: {}", project_id);
        Ok(())
    }
}

impl<N: SessionNotifier + 'static, R: SessionRegistry> std::fmt::Debug
    for SacpSessionManager<N, R>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SacpSessionManager")
            .field("session_count", &self.registry.count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use sacp::schema::SessionId;
    use shared_types::{AgentStatus, ProjectAndAgentInfo, SessionEntry, SessionNotify};
    use tokio::sync::mpsc;

    // ========== Mock SessionNotifier ==========
    struct MockNotifier;

    #[async_trait]
    impl SessionNotifier for MockNotifier {
        async fn notify_prompt_start(
            &self,
            _project_id: &str,
            _session_id: &str,
            _request_id: Option<String>,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }

        async fn notify_prompt_end(
            &self,
            _project_id: &str,
            _session_id: &str,
            _stop_reason: sacp::schema::StopReason,
            _error_message: Option<String>,
            _request_id: Option<String>,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }

        async fn notify_prompt_error(
            &self,
            _project_id: &str,
            _session_id: &str,
            _error: sacp::Error,
            _request_id: Option<String>,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }

        async fn notify_session_update(
            &self,
            _project_id: &str,
            _session_id: &str,
            _session_update: sacp::schema::SessionUpdate,
            _request_id: Option<String>,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }

        async fn notify(
            &self,
            _project_id: &str,
            _session_id: &str,
            _notify: SessionNotify,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
    }

    // ========== Mock SessionRegistry ==========
    struct MockRegistry {
        entries: Mutex<HashMap<String, ProjectAndAgentInfo>>,
        project_to_session: Mutex<HashMap<String, String>>,
    }

    impl MockRegistry {
        fn new() -> Self {
            Self {
                entries: Mutex::new(HashMap::new()),
                project_to_session: Mutex::new(HashMap::new()),
            }
        }
    }

    impl SessionRegistry for MockRegistry {
        type Entry = ProjectAndAgentInfo;

        fn get(&self, project_id: &str) -> Option<Self::Entry> {
            self.entries.lock().unwrap().get(project_id).cloned()
        }

        fn insert(&self, project_id: &str, session_id: &str, entry: Self::Entry) {
            self.entries
                .lock()
                .unwrap()
                .insert(project_id.to_string(), entry);
            self.project_to_session
                .lock()
                .unwrap()
                .insert(project_id.to_string(), session_id.to_string());
        }

        fn remove(&self, project_id: &str) -> Option<Self::Entry> {
            self.project_to_session.lock().unwrap().remove(project_id);
            self.entries.lock().unwrap().remove(project_id)
        }

        fn contains(&self, project_id: &str) -> bool {
            self.entries.lock().unwrap().contains_key(project_id)
        }

        fn list_project_ids(&self) -> Vec<String> {
            self.entries.lock().unwrap().keys().cloned().collect()
        }

        fn count(&self) -> usize {
            self.entries.lock().unwrap().len()
        }
    }

    // ========== Helper functions ==========
    fn create_mock_agent_info(project_id: &str, session_id: &str) -> ProjectAndAgentInfo {
        let (prompt_tx, _prompt_rx) = mpsc::unbounded_channel();
        let (cancel_tx, _cancel_rx) = mpsc::unbounded_channel();

        ProjectAndAgentInfo {
            project_id: project_id.to_string(),
            session_id: SessionId::new(Arc::from(session_id)),
            prompt_tx,
            cancel_tx,
            model_provider: None,
            request_id: None,
            status: AgentStatus::Idle,
            last_activity: chrono::Utc::now(),
            created_at: chrono::Utc::now(),
            stop_handle: None,
            agent_server_config: None,
        }
    }

    // ========== Tests ==========

    #[test]
    fn test_normalize_path_absolute() {
        let path = PathBuf::from("/home/user/project");
        let normalized = SacpSessionManager::<MockNotifier, MockRegistry>::normalize_path(&path);
        assert_eq!(normalized, PathBuf::from("/home/user/project"));
    }

    #[test]
    fn test_normalize_path_removes_current_dir() {
        let path = PathBuf::from("/home/./user/./project");
        let normalized = SacpSessionManager::<MockNotifier, MockRegistry>::normalize_path(&path);
        // Should remove '.' components
        assert!(!normalized.to_string_lossy().contains("/./"));
    }

    #[test]
    fn test_session_manager_new() {
        let notifier = Arc::new(MockNotifier);
        let registry = Arc::new(MockRegistry::new());

        let manager = SacpSessionManager::new(notifier.clone(), registry.clone());

        assert_eq!(manager.session_count(), 0);
        assert!(manager.list_sessions().is_empty());
    }

    #[test]
    fn test_session_manager_registry_operations() {
        let notifier = Arc::new(MockNotifier);
        let registry = Arc::new(MockRegistry::new());
        let manager = SacpSessionManager::new(notifier, registry.clone());

        // Insert a session
        let agent_info = create_mock_agent_info("project-1", "session-1");
        registry.insert("project-1", "session-1", agent_info);

        // Verify operations
        assert!(manager.contains_session("project-1"));
        assert!(!manager.contains_session("project-2"));
        assert_eq!(manager.session_count(), 1);

        let sessions = manager.list_sessions();
        assert_eq!(sessions.len(), 1);
        assert!(sessions.contains(&"project-1".to_string()));

        // Get session
        let session = manager.get_session("project-1");
        assert!(session.is_some());
        assert_eq!(session.unwrap().project_id(), "project-1");

        // Remove session
        let removed = manager.remove_session("project-1");
        assert!(removed.is_some());
        assert_eq!(manager.session_count(), 0);
    }

    #[test]
    fn test_build_text_prompt_request() {
        use crate::PromptMessage;

        let prompt = PromptMessage::new(
            "Hello, world!".to_string(),
            "project-1".to_string(),
            PathBuf::from("/tmp/project"),
            "req-123".to_string(),
            shared_types::ServiceType::RCoder,
        );

        let session_id = SessionId::new(Arc::from("session-1"));

        let result = SacpSessionManager::<MockNotifier, MockRegistry>::build_text_prompt_request(
            &prompt, session_id,
        );

        assert!(result.is_ok());
        let request = result.unwrap();

        // Verify meta contains request_id
        assert!(request.meta.is_some());
        let meta = request.meta.unwrap();
        assert_eq!(
            meta.get("request_id").and_then(|v| v.as_str()),
            Some("req-123")
        );
    }

    #[test]
    fn test_build_prompt_request_with_attachments() {
        use crate::PromptMessage;
        use sacp::schema::{ContentBlock, TextContent};

        let prompt = PromptMessage::new(
            "Analyze this image".to_string(),
            "project-1".to_string(),
            PathBuf::from("/tmp/project"),
            "req-456".to_string(),
            shared_types::ServiceType::RCoder,
        );

        let session_id = SessionId::new(Arc::from("session-1"));
        let attachment_blocks = vec![ContentBlock::Text(TextContent::new(
            "Attachment content".to_string(),
        ))];

        let result =
            SacpSessionManager::<MockNotifier, MockRegistry>::build_prompt_request_with_attachments(
                &prompt,
                session_id,
                attachment_blocks,
            );

        assert!(result.is_ok());
        let request = result.unwrap();

        // Verify meta contains request_id
        assert!(request.meta.is_some());
        let meta = request.meta.unwrap();
        assert_eq!(
            meta.get("request_id").and_then(|v| v.as_str()),
            Some("req-456")
        );

        // Verify prompt contains content blocks (1 text + 1 attachment)
        assert_eq!(request.prompt.len(), 2);
    }

    #[test]
    fn test_registry_and_notifier_accessors() {
        let notifier = Arc::new(MockNotifier);
        let registry = Arc::new(MockRegistry::new());
        let manager = SacpSessionManager::new(notifier.clone(), registry.clone());

        // Verify we can access registry and notifier
        let registry_ref = manager.registry();
        assert_eq!(registry_ref.count(), 0);

        let _notifier_ref = manager.notifier();
        // MockNotifier doesn't have methods to verify, but we ensure no panic
    }

    #[test]
    fn test_debug_impl() {
        let notifier = Arc::new(MockNotifier);
        let registry = Arc::new(MockRegistry::new());
        let manager = SacpSessionManager::new(notifier, registry);

        let debug_str = format!("{:?}", manager);
        assert!(debug_str.contains("SacpSessionManager"));
        assert!(debug_str.contains("session_count"));
    }

    #[tokio::test]
    async fn test_ensure_project_dir() {
        use tempfile::tempdir;

        let temp = tempdir().unwrap();
        let new_dir = temp.path().join("new_project_dir");

        assert!(!new_dir.exists());

        let result =
            SacpSessionManager::<MockNotifier, MockRegistry>::ensure_project_dir(&new_dir).await;

        assert!(result.is_ok());
        assert!(new_dir.exists());
    }
}
