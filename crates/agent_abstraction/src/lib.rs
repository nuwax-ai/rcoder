//! Agent Abstraction Layer
//!
//! This crate provides abstract interfaces for agent management in RCoder.

pub mod acp;
pub mod compat;
pub mod error;
pub mod factory;
pub mod launcher;
pub mod lifecycle;
pub mod manager;
pub mod mcp;
pub mod process;
pub mod registry;
pub mod session;
pub mod traits;

// Re-export types from submodules
pub use acp::{
    AcpAgentClient, AcpConnectionBuilder, AgentConnection, AgentStatus, CancelResult,
    ConnectionStats, EstablishedConnection,
};
pub use compat::{
    AgentLaunchConfig, AgentLifecycleGuard, AgentStopGuard, AgentStopHandleArc,
    ClaudeCodeLauncher, LauncherConnectionInfo, LauncherConnectionInfoComplete,
    convert_context_servers, get_default_agent_config, load_agent_config,
    spawn_cancel_handler_for_agent, spawn_prompt_handler_for_agent,
};
pub use error::AgentAbstractionError;
pub use factory::{AgentFactory, AgentInstance, AgentInstanceStatus, ValidationError, ValidationResult};
pub use launcher::{
    AgentLauncher, ConnectedAgent, LaunchedAgent, LaunchedProcess, ProcessStatus,
    SubprocessLauncher, TerminationResult,
};
pub use lifecycle::{AgentIdleStatus, AgentLifecycleManager, AgentStatusInfo};
pub use manager::{AgentIdleStatistics, AgentManager, AgentServersConfig, AgentStatus as ManagerAgentStatus};
pub use mcp::{
    McpError, McpResult, McpServerInfo, McpServerInstance, McpServerManager, McpServerStatus,
    McpTool, ToolCallRequest, ToolCallResponse,
};
pub use process::AgentProcess;
pub use registry::{AgentRegistry, AgentSpec};
pub use session::{
    AcpAgentWorker, AcpSessionManager, AgentWorker, SessionHandles, WorkerRequest,
    WorkerResponse,
};
pub use traits::agent::{Agent, AgentStartConfig, ProcessLaunchInfo, PromptMessage};
pub use traits::session_notifier::{NoOpSessionNotifier, SessionNotifier};
pub use traits::session_registry::SessionRegistry;
