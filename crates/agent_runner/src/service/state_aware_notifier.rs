//! StateAwareNotifier 实现
//!
//! 状态感知的 SessionNotifier 包装器，在推送 SSE 消息的同时同步更新 Agent 状态。
//!
//! ## 核心职责
//! 1. 委托给 SseSessionNotifier 推送 SSE 消息
//! 2. 通过 SessionRegistry trait 同步更新 Agent 状态（R-1 重构：不再直接依赖 AGENT_REGISTRY）
//! 3. 保持状态转换的原子性和一致性

use agent_abstraction::traits::SessionRegistry;
use agent_abstraction::SessionNotifier;
use async_trait::async_trait;
use std::sync::Arc;
use tracing::{debug, error, info};

use super::session_notifier::SseSessionNotifier;
use shared_types::{AgentStatus, SessionNotify};

/// 状态感知的 SessionNotifier 包装器
///
/// 通过委托模式包装 SseSessionNotifier，在推送 SSE 消息的同时通过
/// `SessionRegistry` trait 同步更新 Agent 状态。
///
/// # 设计特点
/// - **依赖反转**：通过 `R: SessionRegistry` trait 引用更新状态，不直接依赖具体全局变量
/// - **委托模式**：所有 SSE 推送操作委托给内部的 SseSessionNotifier
/// - **原子性状态更新**：通过 registry trait 确保状态更新的原子性
/// - **状态同步顺序**：
///   - PromptStart: 先更新状态为 Active，再推送 SSE
///   - PromptEnd: 先推送 SSE，再恢复状态为 Idle
///   - PromptError: 先推送错误消息，再恢复状态为 Idle
#[derive(Debug, Clone)]
pub struct StateAwareNotifier<R: SessionRegistry> {
    /// 内部 SSE 推送器
    inner: Arc<SseSessionNotifier>,
    /// 通过 trait 引用操作注册表，不直接依赖 AGENT_REGISTRY
    registry: Arc<R>,
}

impl<R: SessionRegistry> StateAwareNotifier<R> {
    /// 创建新的 StateAwareNotifier 实例
    ///
    /// # Arguments
    /// * `registry` - SessionRegistry trait 引用，用于更新 agent 状态
    pub fn new(registry: Arc<R>) -> Self {
        Self {
            inner: Arc::new(SseSessionNotifier::new()),
            registry,
        }
    }

    /// 更新 Agent 状态（通过 SessionRegistry trait）
    ///
    /// 使用 trait 方法实现状态更新，避免直接依赖具体的全局变量。
    /// 实际的状态更新逻辑由 `SessionRegistry` 实现方（如 `AgentSessionRegistry`）提供。
    ///
    /// # 参数
    /// - `project_id`: 项目 ID
    /// - `status`: 新的 Agent 状态
    fn update_agent_status(&self, project_id: &str, status: AgentStatus) {
        self.registry.update_agent_status(project_id, status);
        debug!(
            "🔄 [StateAwareNotifier] Requested status update for Project[{}] to {:?}",
            project_id, status
        );
    }
}

#[async_trait]
impl<R: SessionRegistry + 'static> SessionNotifier for StateAwareNotifier<R> {
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
            "📨 Project[{}] sending SessionPromptStart notification, session_id={}, request_id={:?}",
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
        stop_reason: agent_client_protocol::schema::StopReason,
        error_message: Option<String>,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!(
            "✅ Project[{}] sending SessionPromptEnd notification, session_id={}, stop_reason={:?}, request_id={:?}",
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
        error: agent_client_protocol::schema::Error,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        error!(
            "❌ Project[{}] sending SessionPromptError notification, session_id={}, error_code={}, error_message={}, request_id={:?}",
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
        session_update: agent_client_protocol::schema::SessionUpdate,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        debug!(
            "🔄 Project[{}] sending SessionUpdate notification, session_id={}",
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
            "📢 Project[{}] sending generic session notification, session_id={}",
            project_id, session_id
        );

        // 委托给内部 notifier
        self.inner.notify(project_id, session_id, notify).await
    }
}
