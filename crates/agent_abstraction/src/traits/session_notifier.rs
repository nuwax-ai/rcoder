//! # Session Notifier Trait
//!
//! 定义会话通知的抽象接口，用于推送 SSE 消息到前端。
//! `agent_runner` 模块实现此 trait 来完成实际的消息推送。
//!
//! ## 预期实现
//!
//! 此 trait 预期在 `agent_runner` 中实现：
//!
//! - **实现类**: `agent_runner::service::SseSessionNotifier`
//! - **包装类**: `agent_runner::service::StateAwareNotifier`（注入 project_id）
//!
//! ## 架构说明
//!
//! ```text
//! agent_abstraction                    agent_runner
//! ┌─────────────────┐                 ┌─────────────────────┐
//! │ SessionNotifier │◄────────────────│ SseSessionNotifier  │
//! │ (trait)         │   implements    │ (struct)            │
//! └────────┬────────┘                 └──────────┬──────────┘
//!          │                                     │
//!          │                          ┌──────────▼──────────┐
//! ┌────────▼────────┐                 │ StateAwareNotifier  │
//! │AcpSessionManager│◄────────────────│ (wrapper, 注入     │
//! │ notifier: N     │   injects       │  project_id)        │
//! └─────────────────┘                 └─────────────────────┘
//! ```
//!
//! ## 方法使用场景
//!
//! | 方法 | 触发时机 | 推送内容 |
//! |------|---------|---------|
//! | `notify_prompt_start` | Agent 开始处理请求 | 会话开始通知 |
//! | `notify_prompt_end` | Agent 完成处理 | 会话结束通知（含停止原因）|
//! | `notify_prompt_error` | Agent 处理出错 | 错误通知 |
//! | `notify_session_update` | Agent 输出内容 | 内容块更新（文本、工具调用等）|
//! | `notify` | 通用通知 | 任意 SessionNotify 类型 |
//!
//! ## 使用示例
//!
//! ```ignore
//! // 在 agent_runner 中定义实现
//! pub struct SseSessionNotifier;
//!
//! #[async_trait]
//! impl SessionNotifier for SseSessionNotifier {
//!     async fn notify_prompt_start(...) -> Result<...> {
//!         // 推送到 SESSION_CACHE，由 SSE handler 分发
//!     }
//! }
//!
//! // 使用 StateAwareNotifier 包装，自动注入 project_id
//! let notifier = StateAwareNotifier::new(project_id.clone());
//! let session_manager = AcpSessionManager::new(Arc::new(notifier), registry);
//! ```

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
