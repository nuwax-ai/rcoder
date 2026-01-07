//! Session notification trait for SSE message pushing.
//!
//! 定义会话通知的抽象接口，用于推送 SSE 消息到前端。
//! agent_runner 模块实现此 trait 来完成实际的消息推送。

use async_trait::async_trait;
use shared_types::SessionNotify;

/// 会话通知器 trait
///
/// 提供会话消息推送的抽象接口，解耦 agent_abstraction 和具体的 SSE 实现。
///
/// # 设计说明
/// - agent_abstraction 只依赖此 trait，不依赖具体的 SSE 实现
/// - agent_runner 实现此 trait，完成实际的消息推送
/// - 通过依赖注入的方式，在启动 prompt handler 时传入 notifier
#[async_trait]
pub trait SessionNotifier: Send + Sync + 'static {
    /// 推送会话开始通知
    async fn notify_prompt_start(
        &self,
        project_id: &str,
        session_id: &str,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// 推送会话结束通知
    async fn notify_prompt_end(
        &self,
        project_id: &str,
        session_id: &str,
        stop_reason: agent_client_protocol::StopReason,
        error_message: Option<String>,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// 推送会话错误通知
    async fn notify_prompt_error(
        &self,
        project_id: &str,
        session_id: &str,
        error: agent_client_protocol::Error,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// 推送 Agent 会话更新通知
    async fn notify_session_update(
        &self,
        project_id: &str,
        session_id: &str,
        session_update: agent_client_protocol::SessionUpdate,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// 推送通用会话通知
    async fn notify(
        &self,
        project_id: &str,
        session_id: &str,
        notify: SessionNotify,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}
