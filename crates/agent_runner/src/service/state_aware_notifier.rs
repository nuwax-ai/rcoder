//! StateAwareNotifier 实现
//!
//! 状态感知的 SessionNotifier 包装器，在推送 SSE 消息的同时同步更新 Agent 状态。
//!
//! ## 核心职责
//! 1. 委托给 SseSessionNotifier 推送 SSE 消息
//! 2. 同步更新 PROJECT_AND_AGENT_INFO_MAP 状态
//! 3. 保持状态转换的原子性和一致性

use agent_abstraction::SessionNotifier;
use async_trait::async_trait;
use std::sync::Arc;
use tracing::{debug, error, info};

use crate::proxy_agent::PROJECT_AND_AGENT_INFO_MAP;
use shared_types::{AgentStatus, SessionNotify};
use super::session_notifier::SseSessionNotifier;

/// 状态感知的 SessionNotifier 包装器
///
/// 通过委托模式包装 SseSessionNotifier，在推送 SSE 消息的同时同步更新
/// PROJECT_AND_AGENT_INFO_MAP 中的 Agent 状态。
///
/// # 设计特点
/// - **委托模式**：所有 SSE 推送操作委托给内部的 SseSessionNotifier
/// - **原子性状态更新**：使用 DashMap 的 get_mut 确保状态更新的原子性
/// - **状态同步顺序**：
///   - PromptStart: 先更新状态为 Active，再推送 SSE
///   - PromptEnd: 先推送 SSE，再恢复状态为 Idle
///   - PromptError: 先推送错误消息，再恢复状态为 Idle
///
/// # 示例
/// ```rust
/// use agent_runner::service::StateAwareNotifier;
/// use std::sync::Arc;
///
/// let notifier = Arc::new(StateAwareNotifier::new());
///
/// // 在启动 prompt handler 时注入 notifier
/// agent_abstraction::compat::channel_utils::spawn_prompt_handler_for_agent(
///     client_conn,
///     prompt_rx,
///     session_id,
///     &project_id,
///     notifier,
/// );
/// ```
#[derive(Debug, Clone)]
pub struct StateAwareNotifier {
    /// 内部 SSE 推送器
    inner: Arc<SseSessionNotifier>,
}

impl StateAwareNotifier {
    /// 创建新的 StateAwareNotifier 实例
    pub fn new() -> Self {
        Self {
            inner: Arc::new(SseSessionNotifier::new()),
        }
    }

    /// 更新 Agent 状态（原子操作）
    ///
    /// 使用 DashMap 的 get_mut 方法确保状态更新的原子性。
    ///
    /// # 参数
    /// - `project_id`: 项目 ID
    /// - `status`: 新的 Agent 状态
    fn update_agent_status(&self, project_id: &str, status: AgentStatus) {
        if let Some(mut info) = PROJECT_AND_AGENT_INFO_MAP.get_mut(project_id) {
            info.status = status;
            info.last_activity = chrono::Utc::now();
            debug!(
                "项目[{}]状态更新为 {:?}, last_activity={}",
                project_id,
                status,
                info.last_activity.format("%Y-%m-%d %H:%M:%S")
            );
        } else {
            error!(
                "项目[{}]不存在于 PROJECT_AND_AGENT_INFO_MAP 中，无法更新状态",
                project_id
            );
        }
    }
}

impl Default for StateAwareNotifier {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SessionNotifier for StateAwareNotifier {
    /// 推送会话开始通知
    ///
    /// 顺序：
    /// 1. 更新 Agent 状态为 Active
    /// 2. 推送 SessionPromptStart 到 SSE
    async fn notify_prompt_start(
        &self,
        project_id: &str,
        session_id: &str,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!(
            "📨 项目[{}]发送 SessionPromptStart 通知, session_id={}, request_id={:?}",
            project_id, session_id, request_id
        );

        // 1. 更新状态为 Active（在推送 SSE 之前）
        self.update_agent_status(project_id, AgentStatus::Active);

        // 2. 推送 SSE 消息
        self.inner
            .notify_prompt_start(project_id, session_id, request_id)
            .await
    }

    /// 推送会话结束通知
    ///
    /// 顺序：
    /// 1. 推送 SessionPromptEnd 到 SSE
    /// 2. 恢复 Agent 状态为 Idle
    async fn notify_prompt_end(
        &self,
        project_id: &str,
        session_id: &str,
        stop_reason: agent_client_protocol::StopReason,
        error_message: Option<String>,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!(
            "✅ 项目[{}]发送 SessionPromptEnd 通知, session_id={}, stop_reason={:?}, request_id={:?}",
            project_id, session_id, stop_reason, request_id
        );

        // 1. 推送 SSE 消息（在恢复状态之前）
        let result = self
            .inner
            .notify_prompt_end(
                project_id,
                session_id,
                stop_reason,
                error_message,
                request_id,
            )
            .await;

        // 2. 恢复状态为 Idle
        self.update_agent_status(project_id, AgentStatus::Idle);

        result
    }

    /// 推送会话错误通知
    ///
    /// 顺序：
    /// 1. 推送 SessionPromptError 到 SSE
    /// 2. 恢复 Agent 状态为 Idle
    async fn notify_prompt_error(
        &self,
        project_id: &str,
        session_id: &str,
        error: agent_client_protocol::Error,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        error!(
            "❌ 项目[{}]发送 SessionPromptError 通知, session_id={}, error_code={}, error_message={}, request_id={:?}",
            project_id, session_id, error.code, error.message, request_id
        );

        // 1. 推送错误消息（在恢复状态之前）
        let result = self
            .inner
            .notify_prompt_error(project_id, session_id, error, request_id)
            .await;

        // 2. 恢复状态为 Idle
        self.update_agent_status(project_id, AgentStatus::Idle);

        result
    }

    /// 推送 Agent 会话更新通知
    ///
    /// 不更新 Agent 状态，仅推送 SSE 消息。
    async fn notify_session_update(
        &self,
        project_id: &str,
        session_id: &str,
        session_update: agent_client_protocol::SessionUpdate,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        debug!(
            "🔄 项目[{}]发送 SessionUpdate 通知, session_id={}",
            project_id, session_id
        );

        // 委托给内部 notifier，不更新状态
        self.inner
            .notify_session_update(project_id, session_id, session_update, request_id)
            .await
    }

    /// 推送通用会话通知
    ///
    /// 不更新 Agent 状态，仅推送 SSE 消息。
    async fn notify(
        &self,
        project_id: &str,
        session_id: &str,
        notify: SessionNotify,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        debug!(
            "📢 项目[{}]发送通用会话通知, session_id={}",
            project_id, session_id
        );

        // 委托给内部 notifier
        self.inner.notify(project_id, session_id, notify).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use shared_types::ProjectAndAgentInfo;
    use tokio::sync::mpsc;

    /// 辅助函数：初始化测试环境
    fn setup_test_agent(project_id: &str) {
        let (prompt_tx, _prompt_rx) = mpsc::unbounded_channel();
        let (cancel_tx, _cancel_rx) = mpsc::unbounded_channel();

        let agent_info = ProjectAndAgentInfo {
            project_id: project_id.to_string(),
            session_id: agent_client_protocol::SessionId::new("test-session".to_string()),
            prompt_tx,
            cancel_tx,
            model_provider: None,
            request_id: None,
            status: AgentStatus::Idle,
            last_activity: Utc::now(),
            created_at: Utc::now(),
            stop_handle: None,
        };

        PROJECT_AND_AGENT_INFO_MAP.insert(project_id.to_string(), agent_info);
    }

    /// 辅助函数：清理测试环境
    fn cleanup_test_agent(project_id: &str) {
        PROJECT_AND_AGENT_INFO_MAP.remove(project_id);
    }

    #[tokio::test]
    async fn test_notify_prompt_start_updates_status() {
        // Given: 初始化 Agent 状态为 Idle
        let project_id = "test-project-start";
        setup_test_agent(project_id);

        let notifier = StateAwareNotifier::new();

        // When: 发送 PromptStart 通知
        let _result = notifier
            .notify_prompt_start(project_id, "session-1", None)
            .await;

        // Then: 状态应更新为 Active
        let info = PROJECT_AND_AGENT_INFO_MAP.get(project_id).unwrap();
        assert_eq!(info.status, AgentStatus::Active);

        // Cleanup
        cleanup_test_agent(project_id);
    }

    #[tokio::test]
    async fn test_notify_prompt_end_restores_idle() {
        // Given: Agent 状态为 Active
        let project_id = "test-project-end";
        setup_test_agent(project_id);

        // 手动设置为 Active
        if let Some(mut info) = PROJECT_AND_AGENT_INFO_MAP.get_mut(project_id) {
            info.status = AgentStatus::Active;
        }

        let notifier = StateAwareNotifier::new();

        // When: 发送 PromptEnd 通知
        let _result = notifier
            .notify_prompt_end(
                project_id,
                "session-1",
                agent_client_protocol::StopReason::EndTurn,
                None,
                None,
            )
            .await;

        // Then: 状态应恢复为 Idle
        let info = PROJECT_AND_AGENT_INFO_MAP.get(project_id).unwrap();
        assert_eq!(info.status, AgentStatus::Idle);

        // Cleanup
        cleanup_test_agent(project_id);
    }

    #[tokio::test]
    async fn test_notify_prompt_error_restores_idle() {
        // Given: Agent 状态为 Active
        let project_id = "test-project-error";
        setup_test_agent(project_id);

        // 手动设置为 Active
        if let Some(mut info) = PROJECT_AND_AGENT_INFO_MAP.get_mut(project_id) {
            info.status = AgentStatus::Active;
        }

        let notifier = StateAwareNotifier::new();

        // When: 发送 PromptError 通知
        let error = agent_client_protocol::Error::internal_error();

        let _result = notifier
            .notify_prompt_error(project_id, "session-1", error, None)
            .await;

        // Then: 状态应恢复为 Idle
        let info = PROJECT_AND_AGENT_INFO_MAP.get(project_id).unwrap();
        assert_eq!(info.status, AgentStatus::Idle);

        // Cleanup
        cleanup_test_agent(project_id);
    }

    #[tokio::test]
    async fn test_notify_session_update_does_not_change_status() {
        // Given: Agent 状态为 Active
        let project_id = "test-project-update";
        setup_test_agent(project_id);

        // 手动设置为 Active
        if let Some(mut info) = PROJECT_AND_AGENT_INFO_MAP.get_mut(project_id) {
            info.status = AgentStatus::Active;
        }

        let notifier = StateAwareNotifier::new();

        // When: 发送 SessionUpdate 通知
        let session_update = agent_client_protocol::SessionUpdate::AgentMessageChunk(
            agent_client_protocol::ContentChunk::new(agent_client_protocol::ContentBlock::Text(
                agent_client_protocol::TextContent::new("test message".to_string()),
            )),
        );

        let _result = notifier
            .notify_session_update(project_id, "session-1", session_update, None)
            .await;

        // Then: 状态应保持 Active（不变）
        let info = PROJECT_AND_AGENT_INFO_MAP.get(project_id).unwrap();
        assert_eq!(info.status, AgentStatus::Active);

        // Cleanup
        cleanup_test_agent(project_id);
    }
}
