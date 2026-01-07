//! # Agent Abstraction Layer
//!
//! 此 crate 提供 Agent 管理的抽象接口，是 RCoder 架构的中间层。
//!
//! 使用 SACP (symposium-acp) 协议，支持标准 tokio::spawn（无需 LocalSet）。
//!
//! ## 架构位置
//!
//! ```text
//! shared_types (基础类型: ProjectAndAgentInfo, AgentLifecycleGuard)
//!       ↓
//! agent_abstraction (抽象接口) ← 你在这里
//!       ↓
//! agent_runner (具体实现: AgentSessionRegistry, SseSessionNotifier)
//! ```
//!
//! ## 模块职责
//!
//! | 模块 | 职责 | 关键类型 |
//! |------|------|---------|
//! | [`traits`] | 核心抽象接口定义 | `SessionRegistry`, `SessionNotifier` |
//! | [`session`] | 会话管理和 Worker 抽象 | `SacpSessionManager`, `SacpAgentWorker` |
//! | [`launcher`] | Agent 启动和生命周期管理 | `SacpClaudeCodeLauncher` |
//! | [`acp`] | ACP 协议连接抽象 | `CancelResult` |
//! | [`proxy`] | Proxy Chain 支持 | `RCoderProxy` |
//!
//! ## Trait 实现指南
//!
//! 本 crate 定义的 trait 预期在 `agent_runner` 中实现：
//!
//! - [`SessionRegistry`] → `agent_runner::service::AgentSessionRegistry`
//! - [`SessionNotifier`] → `agent_runner::service::StateAwareNotifier`
//!
//! ### 使用模式
//!
//! ```ignore
//! // 在 agent_runner 中：
//! // 1. 实现 SessionRegistry trait
//! impl SessionRegistry for AgentSessionRegistry { ... }
//!
//! // 2. 通过依赖注入传入 SacpSessionManager
//! let session_manager = SacpSessionManager::new(
//!     Arc::new(notifier),      // SessionNotifier 实现
//!     AGENT_REGISTRY.clone(),  // SessionRegistry 实现
//! );
//! ```
//!
//! ## 设计模式
//!
//! - **依赖反转（DIP）**: Trait 在此定义，实现在上层 crate
//! - **泛型参数**: `SacpSessionManager<N, R>` 支持可插拔实现
//! - **RAII**: 通过 `AgentLifecycleGuard` 管理 Agent 资源生命周期

pub mod acp;
pub mod error;
pub mod launcher;
pub mod session;
pub mod traits;
pub mod proxy;

// Re-export types from submodules
pub use acp::{CancelNotificationRequestWrapper, CancelResult};
pub use launcher::{
    AgentLifecycleGuard, AgentStopGuard, AgentStopHandleArc,
    SacpAgentLaunchConfig, SacpClaudeCodeLauncher, SacpLauncherConnectionInfo,
    convert_context_servers_sacp, get_default_sacp_agent_config, load_sacp_agent_config,
};
pub use error::AgentAbstractionError;
pub use session::{
    AgentWorker, SacpAgentWorker, SacpSessionManager, SessionHandles, WorkerRequest,
    WorkerResponse,
};
pub use traits::agent::{AgentStartConfig, PromptMessage};
pub use traits::session_notifier::SessionNotifier;
pub use traits::session_registry::SessionRegistry;

// Proxy Chain 类型导出
pub use proxy::{
    McpServerConfig, McpTransportType, ProxyConfig, RCoderProxy, RCoderProxyBuilder,
};
