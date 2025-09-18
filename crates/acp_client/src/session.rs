use agent_client_protocol::{self as acp, StopReason};
use async_trait::async_trait;
use std::process::Child;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Arc, Mutex};
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{AcpResult, Session, AcpClientError};

// Re-use ACP message types from lib.rs
use crate::{AcpMessage, AcpResponse, AcpError};

pub struct AcpSession {
    session_id: Uuid,
    process: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<ChildStdin>>,
    stdout: Arc<Mutex<ChildStdout>>,
    is_active: bool,
}

impl AcpSession {
    pub fn new(
        session_id: Uuid,
        process: Child,
        stdin: ChildStdin,
        stdout: ChildStdout,
    ) -> Self {
        Self {
            session_id,
            process: Arc::new(Mutex::new(process)),
            stdin: Arc::new(Mutex::new(stdin)),
            stdout: Arc::new(Mutex::new(stdout)),
            is_active: true,
        }
    }

    async fn send_message(&self, message: &AcpMessage) -> AcpResult<()> {
        let mut stdin = self.stdin.lock().await;
        let json = serde_json::to_string(message)
            .map_err(|e| AcpClientError::ProtocolError(format!("Failed to serialize message: {}", e)))?;

        debug!("Sending ACP message: {}", json);

        stdin.write_all(json.as_bytes()).await
            .map_err(|e| AcpClientError::ConnectionError(format!("Failed to write message: {}", e)))?;

        stdin.write_all(b"\n").await
            .map_err(|e| AcpClientError::ConnectionError(format!("Failed to write newline: {}", e)))?;

        stdin.flush().await
            .map_err(|e| AcpClientError::ConnectionError(format!("Failed to flush stdin: {}", e)))?;

        Ok(())
    }

    async fn read_response(&self) -> AcpResult<AcpResponse> {
        let mut stdout = self.stdout.lock().await;
        let mut buffer = String::new();
        let mut temp_buf = [0u8; 1024];

        loop {
            match stdout.read(&mut temp_buf).await {
                Ok(0) => {
                    return Err(AcpClientError::ConnectionError("Connection closed".to_string()));
                }
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&temp_buf[..n]);
                    buffer.push_str(&chunk);

                    // Check for complete JSON messages (separated by newlines)
                    if let Some(newline_pos) = buffer.find('\n') {
                        let json_str = &buffer[..newline_pos];
                        match self.try_parse_response(json_str.as_bytes()) {
                            Ok(response) => {
                                debug!("Received ACP response: {:?}", response);
                                return Ok(response);
                            }
                            Err(e) => {
                                // If parsing fails, continue reading
                                buffer.remove(0); // Remove processed character
                                continue;
                            }
                        }
                    }
                }
                Err(e) => {
                    return Err(AcpClientError::ConnectionError(format!("Failed to read response: {}", e)));
                }
            }
        }
    }

    fn try_parse_response(&self, buffer: &[u8]) -> Result<AcpResponse, serde_json::Error> {
        serde_json::from_slice(buffer)
    }

    async fn send_prompt(&self, prompt: &str, context: Option<Value>) -> AcpResult<acp::PromptResponse> {
        let message_id = uuid::Uuid::new_v4().to_string();

        let message = AcpMessage {
            id: message_id.clone(),
            method: "prompt".to_string(),
            params: serde_json::json!({
                "prompt": prompt,
                "context": context.unwrap_or_default(),
                "stream": false
            }),
        };

        self.send_message(&message).await?;

        let response = self.read_response().await?;

        if let Some(error) = response.error {
            return Err(AcpClientError::ProtocolError(format!("Prompt failed: {}", error.message)));
        }

        // Convert ACP response to PromptResponse
        self.convert_to_prompt_response(response.result.unwrap_or_default())
    }

    fn convert_to_prompt_response(&self, result: Value) -> AcpResult<acp::PromptResponse> {
        // This is a simplified conversion - in practice, you'd need to handle
        // the actual ACP protocol response format
        Ok(acp::PromptResponse {
            stop_reason: StopReason::EndTurn,
            meta: None,
        })
    }
}

#[async_trait]
impl Session for AcpSession {
    async fn prompt(
        &self,
        prompt: &str,
        context: Option<serde_json::Value>,
    ) -> AcpResult<acp::PromptResponse> {
        debug!("Sending prompt through session: {}", prompt);
        self.send_prompt(prompt, context).await
    }

    fn session_id(&self) -> &Uuid {
        &self.session_id
    }

    fn acp_session_id(&self) -> &str {
        &self.session_id.to_string()
    }
}

impl Drop for AcpSession {
    fn drop(&mut self) {
        if self.is_active {
            info!("Cleaning up ACP session: {}", self.session_id);
            // Process will be cleaned up when Arc<Mutex<Child>> is dropped
        }
    }
}