use agent_client_protocol::{self as acp, StopReason};
use async_trait::async_trait;
use shared_types::{AcpConnectionConfig, AgentType};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::io::{BufRead, BufReader, Write};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{Mutex, Arc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, info, error, warn};
use uuid::Uuid;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize)]
pub struct AcpMessage {
    id: String,
    method: String,
    params: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AcpResponse {
    pub id: String,
    pub result: Option<Value>,
    pub error: Option<AcpError>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AcpError {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

pub mod error;
pub mod session;

pub use error::{AcpClientError, AcpResult};
pub use session::AcpSession;

#[async_trait]
pub trait Session: Send + Sync {
    async fn prompt(
        &self,
        prompt: &str,
        context: Option<serde_json::Value>,
    ) -> AcpResult<acp::PromptResponse>;

    fn session_id(&self) -> &Uuid;
    fn acp_session_id(&self) -> &str;
}

#[async_trait]
pub trait AgentConnection: Send + Sync {
    async fn new_session(
        &self,
        project_path: &PathBuf,
        session_id: Uuid,
    ) -> AcpResult<std::sync::Arc<dyn Session>>;

    async fn authenticate(&self, method_id: &str, credentials: &serde_json::Value) -> AcpResult<()>;

    async fn prompt(
        &self,
        session_id: Uuid,
        prompt: &str,
        context: Option<serde_json::Value>,
    ) -> AcpResult<acp::PromptResponse>;

    async fn cancel(&self, session_id: Uuid) -> AcpResult<()>;

    async fn close(&self, session_id: Uuid) -> AcpResult<()>;
}

pub struct AcpConnectionManager {
    config: AcpConnectionConfig,
}

impl AcpConnectionManager {
    pub fn new(config: AcpConnectionConfig) -> Self {
        Self { config }
    }

    pub async fn create_connection(&self, connection_id: &str) -> AcpResult<std::sync::Arc<dyn AgentConnection>> {
        match self.config.agent_type {
            AgentType::ClaudeCode => {
                let connection = ClaudeCodeConnection::new(connection_id.to_string(), self.config.clone());
                Ok(std::sync::Arc::new(connection))
            }
            AgentType::Gemini => {
                Err(AcpClientError::ProtocolError("Gemini connection not implemented".to_string()))
            }
            AgentType::Custom => {
                Err(AcpClientError::ProtocolError("Custom connection not implemented".to_string()))
            }
        }
    }
}

pub struct ClaudeCodeConnection {
    connection_id: String,
    config: AcpConnectionConfig,
    sessions: Arc<Mutex<HashMap<Uuid, Arc<AcpSession>>>>,
}

impl ClaudeCodeConnection {
    pub fn new(connection_id: String, config: AcpConnectionConfig) -> Self {
        Self {
            connection_id,
            config,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn spawn_claude_process(&self, project_path: &PathBuf) -> Result<(Child, ChildStdin, ChildStdout), Box<dyn std::error::Error + Send + Sync>> {
        info!("Spawning claude-code process for project: {:?}", project_path);

        let mut cmd = Command::new("claude-code");
        cmd.arg("acp")
           .arg("--stdio")
           .current_dir(project_path)
           .stdin(Stdio::piped())
           .stdout(Stdio::piped())
           .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;

        let stdin = child.stdin.take().ok_or("Failed to get stdin")?;
        let stdout = child.stdout.take().ok_or("Failed to get stdout")?;

        Ok((child, stdin, stdout))
    }

    async fn send_acp_message(&self, stdin: &mut ChildStdin, message: &AcpMessage) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let json = serde_json::to_string(message)?;
        debug!("Sending ACP message: {}", json);

        stdin.write_all(json.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;

        Ok(())
    }

    async fn read_acp_response(&self, stdout: &mut ChildStdout) -> Result<AcpResponse, Box<dyn std::error::Error + Send + Sync>> {
        let mut buffer = Vec::new();
        let mut temp_buf = [0u8; 1024];

        loop {
            let n = stdout.read(&mut temp_buf).await?;
            if n == 0 {
                break;
            }

            buffer.extend_from_slice(&temp_buf[..n]);

            // Check if we have a complete JSON message
            if let Ok(response) = self.try_parse_response(&buffer) {
                debug!("Received ACP response: {:?}", response);
                return Ok(response);
            }
        }

        Err("Failed to read complete response".into())
    }

    fn try_parse_response(&self, buffer: &[u8]) -> Result<AcpResponse, serde_json::Error> {
        let response: AcpResponse = serde_json::from_slice(buffer)?;
        Ok(response)
    }
}

#[async_trait]
impl AgentConnection for ClaudeCodeConnection {
    async fn new_session(
        &self,
        project_path: &PathBuf,
        session_id: Uuid,
    ) -> AcpResult<std::sync::Arc<dyn Session>> {
        info!("Creating new Claude Code session: {} for project: {:?}", session_id, project_path);

        // Spawn claude-code process
        let (process, mut stdin, mut stdout) = self.spawn_claude_process(project_path)
            .await
            .map_err(|e| AcpClientError::ConnectionError(format!("Failed to spawn claude-code: {}", e)))?;

        // Send initialization message
        let init_message = AcpMessage {
            id: session_id.to_string(),
            method: "initialize".to_string(),
            params: serde_json::json!({
                "sessionId": session_id.to_string(),
                "capabilities": {
                    "prompts": true,
                    "tools": true,
                    "files": true
                }
            }),
        };

        self.send_acp_message(&mut stdin, &init_message).await
            .map_err(|e| AcpClientError::ProtocolError(format!("Failed to send init message: {}", e)))?;

        // Read initialization response
        let init_response = self.read_acp_response(&mut stdout).await
            .map_err(|e| AcpClientError::ProtocolError(format!("Failed to read init response: {}", e)))?;

        if init_response.error.is_some() {
            return Err(AcpClientError::ConnectionError(
                format!("Initialization failed: {:?}", init_response.error)
            ));
        }

        let session = Arc::new(AcpSession::new(session_id, process, stdin, stdout));

        // Track the session
        self.sessions.lock().await.insert(session_id, session.clone());

        Ok(session)
    }

    async fn authenticate(&self, method_id: &str, credentials: &serde_json::Value) -> AcpResult<()> {
        debug!("Authenticating with method: {}", method_id);

        // For now, assume authentication is handled by claude-code CLI
        // Future implementation could include API key validation
        Ok(())
    }

    async fn prompt(
        &self,
        session_id: Uuid,
        prompt: &str,
        context: Option<serde_json::Value>,
    ) -> AcpResult<acp::PromptResponse> {
        debug!("Sending prompt to Claude Code session {}: {}", session_id, prompt);

        // Find the session and use it to send the prompt
        let sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(&session_id) {
            // Clone the Arc to use the session
            let session_clone = session.clone();
            drop(sessions); // Release the lock

            return session.prompt(prompt, context).await;
        }

        Err(AcpClientError::ConnectionError(
            format!("Session {} not found", session_id)
        ))
    }

    async fn cancel(&self, session_id: Uuid) -> AcpResult<()> {
        info!("Cancelling session: {}", session_id);

        // Find the session and send cancellation
        let sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(&session_id) {
            // Send cancellation message
            let cancel_message = AcpMessage {
                id: uuid::Uuid::new_v4().to_string(),
                method: "cancel".to_string(),
                params: serde_json::json!({
                    "sessionId": session_id.to_string(),
                    "reason": "user_cancelled"
                }),
            };

            // Note: We can't easily send messages here since the session handles its own stdin/stdout
            // This would need a better design in a real implementation
            warn!("Session cancellation not fully implemented - session will be cleaned up on drop");
        }

        Ok(())
    }

    async fn close(&self, session_id: Uuid) -> AcpResult<()> {
        info!("Closing session: {}", session_id);

        // Remove session from tracking - it will be cleaned up when Arc is dropped
        let mut sessions = self.sessions.lock().await;
        sessions.remove(&session_id);

        Ok(())
    }
}