//! Agent connection wrapper for ACP protocol.

use std::sync::Arc;

use agent_client_protocol::schema::{CancelNotification, PromptRequest, SessionId};
use tokio::sync::{mpsc, oneshot};

// 重新导出 shared_types 中的统一类型
pub use shared_types::{CancelNotificationRequestWrapper, CancelResult};

/// Agent connection wrapper
#[derive(Debug)]
pub struct AgentConnection {
    /// Project ID (业务主键)
    /// project_id 是业务的唯一标识符，用于关联项目和对应的 agent 实例
    /// 可以根据 project_id 找到对应的 session_id
    pub project_id: String,

    /// Service Type (服务类型)
    /// 用于区分不同类型的服务，对应不同的 Docker 镜像和运行环境
    /// 当前所有业务都使用 ServiceType::RCoder
    pub service_type: shared_types::ServiceType,

    /// Session ID (可空)
    /// session_id 是 ACP 协议中的会话标识符，在 agent 的 newSession 成功后由 agent 返回
    /// 创建 agent 时可能没有 session_id，需要在会话建立后更新
    pub session_id: Option<SessionId>,

    /// Prompt sender channel
    pub prompt_tx: Arc<mpsc::UnboundedSender<PromptRequest>>,
    /// Cancel sender channel - wrapped in Arc to avoid Debug requirement
    pub cancel_tx: Arc<mpsc::UnboundedSender<CancelNotificationRequestWrapper>>,
}

impl AgentConnection {
    /// Create a new agent connection
    ///
    /// # Arguments
    /// * `project_id` - 业务主键，唯一标识项目
    /// * `service_type` - 服务类型，对应不同的 Docker 镜像和运行环境
    /// * `session_id` - ACP 会话 ID（可空，创建时可能没有）
    /// * `prompt_tx` - 提示消息发送通道
    /// * `cancel_tx` - 取消消息发送通道
    pub fn new(
        project_id: String,
        service_type: shared_types::ServiceType,
        session_id: Option<SessionId>,
        prompt_tx: Arc<mpsc::UnboundedSender<PromptRequest>>,
        cancel_tx: Arc<mpsc::UnboundedSender<CancelNotificationRequestWrapper>>,
    ) -> Self {
        Self {
            project_id,
            service_type,
            session_id,
            prompt_tx,
            cancel_tx,
        }
    }

    /// Get project ID (业务主键)
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// Get session ID (可空)
    pub fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }

    /// Update session ID after successful newSession
    /// 在 agent 的 newSession 成功后调用，更新 session_id
    pub fn set_session_id(&mut self, session_id: SessionId) {
        self.session_id = Some(session_id);
    }

    /// Check if session is ready (has session_id)
    pub fn has_session(&self) -> bool {
        self.session_id.is_some()
    }

    /// Send prompt
    ///
    /// 通过 channel 发送 prompt 到 LocalSet 中运行的 agent
    /// session_id 由服务端自动管理，调用方无需关心
    ///
    /// 设计说明：
    /// - SACP 版本支持 Send trait，可以在 tokio::spawn 中运行
    /// - 使用 MPSC channel 解耦调用方和 agent 运行环境
    /// - newSession 在 agent 启动后自动执行
    /// - prompt handler 会自动处理 session_id 的覆盖
    pub async fn send_prompt(
        &self,
        prompt: PromptRequest,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.prompt_tx.send(prompt).map_err(|e| {
            Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>
        })
    }

    /// Send cancel
    ///
    /// 通过 channel 发送取消请求到 LocalSet 中运行的 agent
    /// 返回一个 receiver，调用方可以通过它等待取消结果
    ///
    /// # Returns
    /// - `Ok(receiver)`: 请求已发送，通过 receiver.await 获取取消结果
    /// - `Err`: 发送请求失败（channel 已关闭）
    pub async fn send_cancel(
        &self,
        cancel_notification: CancelNotification,
    ) -> Result<oneshot::Receiver<CancelResult>, Box<dyn std::error::Error + Send + Sync>> {
        let (result_tx, result_rx) = oneshot::channel();

        self.cancel_tx
            .send(CancelNotificationRequestWrapper {
                cancel_notification,
                result_tx,
            })
            .map_err(|e| {
                Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>
            })?;

        Ok(result_rx)
    }

    /// Send cancel and wait for result
    ///
    /// 发送取消请求并等待结果
    pub async fn send_cancel_and_wait(
        &self,
        cancel_notification: CancelNotification,
    ) -> Result<CancelResult, Box<dyn std::error::Error + Send + Sync>> {
        let result_rx = self.send_cancel(cancel_notification).await?;

        result_rx.await.map_err(|e| {
            Box::new(std::io::Error::other(format!(
                "Failed to receive cancel result: {}",
                e
            ))) as Box<dyn std::error::Error + Send + Sync>
        })
    }
}

/// Agent status type (re-exported from shared_types)
pub type AgentStatus = shared_types::AgentStatus;

/// Connection status enum
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConnectionStatus {
    /// Connection is being established
    Connecting = 1,
    /// Connection is active
    Connected = 2,
    /// Connection is idle
    Idle = 3,
    /// Connection has an error
    Error = 4,
    /// Connection is closed
    Closed = 5,
}

impl ConnectionStatus {
    /// Convert from u8
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => ConnectionStatus::Connecting,
            2 => ConnectionStatus::Connected,
            3 => ConnectionStatus::Idle,
            4 => ConnectionStatus::Error,
            5 => ConnectionStatus::Closed,
            _ => ConnectionStatus::Closed,
        }
    }

    /// Convert to u8
    pub fn to_u8(self) -> u8 {
        self as u8
    }
}

/// Connection statistics
#[derive(Debug, Clone)]
pub struct ConnectionStats {
    pub total_connections: u64,
    pub active_connections: u64,
    pub idle_connections: u64,
    pub error_connections: u64,
}
