//! ACP connection management module.

mod connection;
mod connection_builder;
mod manager;

pub use connection::{
    AgentConnection, AgentStatus, CancelNotificationRequestWrapper, CancelResult, ConnectionStats,
    ConnectionStatus,
};
pub use connection_builder::{AcpAgentClient, AcpConnectionBuilder, EstablishedConnection};
pub use manager::AcpConnectionManager;

/// Legacy type alias for backward compatibility
pub type Connection = AgentConnection;

/// Placeholder error type
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("Connection error: {0}")]
    Connection(String),
    #[error("Other error: {0}")]
    Other(String),
}
