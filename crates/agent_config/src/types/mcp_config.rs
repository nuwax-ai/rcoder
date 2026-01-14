//! MCP server configuration structures.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// MCP server source type
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpServerSource {
    /// Custom command (npm, uvx, bun, etc.)
    Custom,
    /// Local executable file
    Local,
}

/// MCP server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Server name
    pub name: String,

    /// Server source type
    pub source: McpServerSource,

    /// Whether enabled
    pub enabled: bool,

    /// Command to execute (for custom/local sources)
    pub command: Option<String>,

    /// Command arguments
    pub args: Option<Vec<String>>,

    /// Environment variables
    pub env: Option<HashMap<String, String>>,

    /// Connection timeout
    pub timeout: Option<Duration>,
}

/// Context server configuration (simplified format for config files)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextServerConfig {
    /// Server source type
    pub source: String,

    /// Whether enabled
    pub enabled: bool,

    /// Command to execute
    pub command: Option<String>,

    /// Command arguments
    pub args: Option<Vec<String>>,

    /// Environment variables
    pub env: Option<HashMap<String, String>>,
}

impl McpServerConfig {
    /// Create a new MCP server config
    pub fn new(name: String, source: McpServerSource) -> Self {
        Self {
            name,
            source,
            enabled: true,
            command: None,
            args: None,
            env: None,
            timeout: Some(Duration::from_secs(30)),
        }
    }

    /// Create a custom MCP server
    pub fn custom(name: String, command: String) -> Self {
        Self {
            name,
            source: McpServerSource::Custom,
            enabled: true,
            command: Some(command),
            args: None,
            env: None,
            timeout: Some(Duration::from_secs(30)),
        }
    }

    /// Create a local MCP server
    pub fn local(name: String, command: String) -> Self {
        Self {
            name,
            source: McpServerSource::Local,
            enabled: true,
            command: Some(command),
            args: None,
            env: None,
            timeout: Some(Duration::from_secs(30)),
        }
    }

    /// Check if server is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

impl ContextServerConfig {
    /// Create a new context server config
    pub fn new(source: String) -> Self {
        Self {
            source,
            enabled: true,
            command: None,
            args: None,
            env: None,
        }
    }

    /// Check if server is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}
