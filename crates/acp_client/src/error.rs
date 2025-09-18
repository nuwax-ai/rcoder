use thiserror::Error;

pub type AcpResult<T> = Result<T, AcpClientError>;

#[derive(Error, Debug)]
pub enum AcpClientError {
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    #[error("Authentication failed: {0}")]
    AuthenticationFailed(String),

    #[error("Protocol error: {0}")]
    ProtocolError(String),

    #[error("Process error: {0}")]
    ProcessError(String),

    #[error("Connection error: {0}")]
    ConnectionError(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),

    #[error("Other error: {0}")]
    Other(#[from] anyhow::Error),
}

impl From<agent_client_protocol::Error> for AcpClientError {
    fn from(err: agent_client_protocol::Error) -> Self {
        AcpClientError::ProtocolError(err.to_string())
    }
}