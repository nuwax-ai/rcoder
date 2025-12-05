//! ACP Connection Builder
//!
//! This module provides utilities for establishing ACP connections
//! from process stdio handles.
//!
//! Note: The actual ACP connection logic is implemented in `agent_runner::proxy_agent::claude_code_agent`.
//! This module provides the abstraction layer that can be used for future extensions.

use std::path::PathBuf;

use agent_client_protocol::{McpServer, PromptRequest, SessionId};
use shared_types::CancelNotificationRequest;
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::launcher::LaunchedProcess;

/// Established ACP connection result
#[derive(Debug)]
pub struct EstablishedConnection {
    /// Session ID from newSession
    pub session_id: SessionId,
    /// Prompt sender channel
    pub prompt_tx: mpsc::UnboundedSender<PromptRequest>,
    /// Cancel sender channel
    pub cancel_tx: mpsc::UnboundedSender<CancelNotificationRequest>,
    /// Stderr monitoring task handle
    pub stderr_task: JoinHandle<()>,
    /// Cancellation token
    pub cancel_token: CancellationToken,
}

/// ACP Connection Builder
///
/// Establishes ACP connections from process stdio handles.
/// This builder encapsulates the connection configuration.
///
/// Note: For the actual connection implementation, see `agent_runner::proxy_agent::claude_code_agent`.
pub struct AcpConnectionBuilder {
    /// Project path for the session
    project_path: PathBuf,
    /// Optional existing session ID to load
    existing_session_id: Option<String>,
    /// MCP servers to configure
    mcp_servers: Vec<McpServer>,
    /// Client name for identification
    client_name: String,
    /// Client version
    client_version: String,
}

impl AcpConnectionBuilder {
    /// Create a new connection builder
    pub fn new(project_path: PathBuf) -> Self {
        Self {
            project_path,
            existing_session_id: None,
            mcp_servers: Vec::new(),
            client_name: "rcoder-agent".to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Set existing session ID to load
    pub fn with_session_id(mut self, session_id: Option<String>) -> Self {
        self.existing_session_id = session_id;
        self
    }

    /// Set MCP servers
    pub fn with_mcp_servers(mut self, servers: Vec<McpServer>) -> Self {
        self.mcp_servers = servers;
        self
    }

    /// Set client name
    pub fn with_client_name(mut self, name: String) -> Self {
        self.client_name = name;
        self
    }

    /// Set client version
    pub fn with_client_version(mut self, version: String) -> Self {
        self.client_version = version;
        self
    }

    /// Get project path
    pub fn project_path(&self) -> &PathBuf {
        &self.project_path
    }

    /// Get existing session ID
    pub fn existing_session_id(&self) -> Option<&String> {
        self.existing_session_id.as_ref()
    }

    /// Get MCP servers
    pub fn mcp_servers(&self) -> &[McpServer] {
        &self.mcp_servers
    }

    /// Get client name
    pub fn client_name(&self) -> &str {
        &self.client_name
    }

    /// Get client version
    pub fn client_version(&self) -> &str {
        &self.client_version
    }
}

/// ACP Agent Client marker type
///
/// This is a marker type for ACP client implementations.
/// The actual implementation should implement the `agent_client_protocol::Client` trait.
#[derive(Debug, Clone)]
pub struct AcpAgentClient;
