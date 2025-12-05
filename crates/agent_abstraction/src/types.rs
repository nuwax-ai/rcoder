//! Re-export types from agent_config and shared_types.

pub use agent_config::*;
pub use shared_types::*;

// Agent abstraction specific types
pub use acp::config::AcpConnectionConfig;
pub use acp::connection_manager::AcpConnectionManager;
pub use context::AgentContext;
pub use factory::agent_factory::AgentFactory;
pub use instance::AgentInstance;
pub use manager::agent_manager::AgentManager;
pub use mcp::manager::McpServerManager;
pub use process::AgentProcess;
pub use registry::agent_registry::AgentRegistry;

// Error types
pub use agent_config::types::error::AgentError as Error;
pub use agent_config::types::error::Result;
