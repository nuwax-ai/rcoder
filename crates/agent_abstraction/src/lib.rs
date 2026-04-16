//! # Agent Abstraction Layer (SACP 版本)
//!
//! 此 crate 提供 Agent 管理的抽象接口，是 RCoder 架构的中间层。
//!
//! ## SACP 迁移说明
//!
//! 本模块已迁移至 SACP (Symposium ACP) 实现：
//! - **移除 Client trait**: `AcpSessionManager<N, R>` 不再需要 `Client` 泛型参数
//! - **支持 Send trait**: 可使用标准 `tokio::spawn`，无需 `LocalSet`
//! - **简化架构**: 移除了 channel handler，SACP 内部使用回调处理
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
//! | [`launcher`] | Agent 启动和生命周期管理 | `SacpClaudeCodeLauncher` |
//! | [`acp`] | ACP 协议连接抽象 | `CancelResult` |
//!
//! ## Trait 实现指南
//!
//! 本 crate 定义的 trait 预期在 `agent_runner` 中实现：
//!
//! - [`SessionRegistry`] → `agent_runner::service::AgentSessionRegistry` (全局 `AGENT_REGISTRY`)
//! - [`SessionNotifier`] → `agent_runner::service::SseSessionNotifier`
//!
//! ### 使用模式 (SACP)
//!
//! ```ignore
//! // 在 agent_runner 中：
//! // 1. 实现 SessionRegistry trait
//! impl SessionRegistry for AgentSessionRegistry { ... }
//!
//! // 2. 通过依赖注入传入 AcpSessionManager（不再需要 Client 类型参数）
//! let session_manager = AcpSessionManager::new(
//!     Arc::new(notifier),      // SessionNotifier 实现
//!     AGENT_REGISTRY.clone(),  // SessionRegistry 实现
//! );
//! ```
//!
//! ## 设计模式
//!
//! - **依赖反转（DIP）**: Trait 在此定义，实现在上层 crate
//! - **泛型参数**: `AcpSessionManager<N, R>` 支持可插拔实现
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
pub mod mirror_env;
pub mod path_env;
pub mod session;
pub mod traits;

// Re-export types from submodules
pub use acp::{CancelNotificationRequestWrapper, CancelResult};
pub use error::AgentAbstractionError;
pub use launcher::{
    AgentLaunchConfig, AgentLifecycleGuard, ClaudeCodeLauncher, LauncherConnectionInfoComplete,
    convert_context_servers, get_default_agent_config, load_agent_config,
};
pub use session::{
    AcpAgentWorker, AcpSessionManager, AgentWorker, SessionHandles, WorkerRequest, WorkerResponse,
};
pub use traits::agent::{AgentStartConfig, PromptMessage};
pub use traits::session_notifier::SessionNotifier;
pub use traits::session_registry::SessionRegistry;
