//! # Agent Abstraction Layer
//!
//! 此 crate 提供 Agent 管理的抽象接口，是 RCoder 架构的中间层。
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
//! | [`session`] | 会话管理和 Worker 抽象 | `AcpSessionManager`, `AcpAgentWorker` |
//! | [`launcher`] | Agent 启动和生命周期管理 | `ClaudeCodeLauncher` |
//! | [`acp`] | ACP 协议连接抽象 | `CancelResult` |
//!
//! ## Trait 实现指南
//!
//! 本 crate 定义的 trait 预期在 `agent_runner` 中实现：
//!
//! - [`SessionRegistry`] → `agent_runner::service::AgentSessionRegistry` (全局 `AGENT_REGISTRY`)
//! - [`SessionNotifier`] → `agent_runner::service::SseSessionNotifier`
//!
//! ### 使用模式
//!
//! ```ignore
//! // 在 agent_runner 中：
//! // 1. 实现 SessionRegistry trait
//! impl SessionRegistry for AgentSessionRegistry { ... }
//!
//! // 2. 通过依赖注入传入 AcpSessionManager
//! let session_manager = AcpSessionManager::new(
//!     Arc::new(notifier),      // SessionNotifier 实现
//!     AGENT_REGISTRY.clone(),  // SessionRegistry 实现
//! );
//! ```
//!
//! ## 设计模式
//!
//! - **依赖反转（DIP）**: Trait 在此定义，实现在上层 crate
//! - **泛型参数**: `AcpSessionManager<N, C, R>` 支持可插拔实现
//! - **RAII**: 通过 `AgentLifecycleGuard` 管理 Agent 资源生命周期
//!
//! ## 与 AGENT_REGISTRY 的关系
//!
//! `AGENT_REGISTRY` 是 `agent_runner` 中的全局单例，实现了 `SessionRegistry` trait。
//! 通过依赖注入传入 `AcpSessionManager`，使得抽象层不依赖具体实现。
//!
//! - **直接访问 AGENT_REGISTRY**: 仅在 `agent_runner` 内部使用（如清理任务、状态查询）
//! - **通过 SessionRegistry trait**: 在 `agent_abstraction` 内部使用（会话管理逻辑）

pub mod acp;
pub mod error;
pub mod launcher;
pub mod session;
pub mod traits;

// Re-export types from submodules
pub use acp::{CancelNotificationRequestWrapper, CancelResult};
pub use launcher::{
    AgentLaunchConfig, AgentLifecycleGuard, AgentStopGuard, AgentStopHandleArc,
    ClaudeCodeLauncher, LauncherConnectionInfo, LauncherConnectionInfoComplete,
    convert_context_servers, get_default_agent_config, load_agent_config,
    spawn_cancel_handler_for_agent, spawn_prompt_handler_for_agent,
};
pub use error::AgentAbstractionError;
pub use session::{
    AcpAgentWorker, AcpSessionManager, AgentWorker, SessionHandles, WorkerRequest,
    WorkerResponse,
};
pub use traits::agent::{AgentStartConfig, PromptMessage};
pub use traits::session_notifier::SessionNotifier;
pub use traits::session_registry::SessionRegistry;
