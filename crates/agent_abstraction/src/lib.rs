//! Agent Abstraction Layer
//!
//! This crate provides abstract interfaces for agent management in RCoder.

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
