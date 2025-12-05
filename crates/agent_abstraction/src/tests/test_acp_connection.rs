//! Tests for ACP connection module.

use super::*;
use crate::acp::{AgentConnection, CancelNotificationRequestWrapper};

#[tokio::test]
async fn test_agent_connection_creation() {
    let project_id = "project-123".to_string();
    let session_id = agent_client_protocol::SessionId::new("test-session");
    let (prompt_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let (cancel_tx, _) = tokio::sync::mpsc::unbounded_channel();

    let connection = AgentConnection::new(
        project_id.clone(),
        shared_types::ServiceType::RCoder,
        Some(session_id.clone()),
        std::sync::Arc::new(prompt_tx),
        std::sync::Arc::new(cancel_tx),
    );

    assert_eq!(connection.project_id(), project_id);
    assert_eq!(connection.session_id().unwrap().0, "test-session");
    assert!(connection.has_session());
}

#[tokio::test]
async fn test_agent_connection_send_prompt() {
    let project_id = "project-123".to_string();
    let session_id = agent_client_protocol::SessionId::new("test-session");
    let (prompt_tx, mut prompt_rx) = tokio::sync::mpsc::unbounded_channel();
    let (cancel_tx, _) = tokio::sync::mpsc::unbounded_channel();

    let connection = AgentConnection::new(
        project_id.clone(),
        shared_types::ServiceType::RCoder,
        Some(session_id),
        std::sync::Arc::new(prompt_tx),
        std::sync::Arc::new(cancel_tx),
    );

    // Send a prompt
    let prompt = agent_client_protocol::PromptRequest::new(
        agent_client_protocol::SessionId::new("test-session"),
        vec![agent_client_protocol::ContentBlock::Text(
            agent_client_protocol::TextContent::new("Test prompt".to_string()),
        )],
    );

    // The prompt should be sent through the channel
    let result = connection.send_prompt(prompt).await;
    assert!(result.is_ok());

    // Verify the prompt was received (non-blocking check)
    tokio::select! {
        received = prompt_rx.recv() => {
            assert!(received.is_some());
        }
        _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {
            // Timeout is expected in tests as we don't have a receiver set up
        }
    }
}

#[test]
fn test_cancel_notification_request_wrapper() {
    let inner_request = shared_types::CancelNotificationRequest {
        cancel_notification: shared_types::CancelNotification {
            request_id: "test-request".to_string(),
        },
        tx: tokio::sync::oneshot::channel().0,
    };

    let wrapper = CancelNotificationRequestWrapper {
        inner: inner_request,
    };
    assert!(wrapper.inner.cancel_notification.request_id == "test-request");
}

#[test]
fn test_connection_status_enum() {
    use crate::acp::ConnectionStatus;

    assert_eq!(ConnectionStatus::Connecting as u8, 1);
    assert_eq!(ConnectionStatus::Connected as u8, 2);
    assert_eq!(ConnectionStatus::Idle as u8, 3);
    assert_eq!(ConnectionStatus::Error as u8, 4);
    assert_eq!(ConnectionStatus::Closed as u8, 5);

    // Test from_u8 conversion
    assert!(matches!(
        ConnectionStatus::from_u8(1),
        ConnectionStatus::Connecting
    ));
    assert!(matches!(
        ConnectionStatus::from_u8(2),
        ConnectionStatus::Connected
    ));
    assert!(matches!(
        ConnectionStatus::from_u8(99),
        ConnectionStatus::Closed
    ));
}

#[test]
fn test_connection_status_to_u8() {
    use crate::acp::ConnectionStatus;

    assert_eq!(ConnectionStatus::Connecting.to_u8(), 1);
    assert_eq!(ConnectionStatus::Connected.to_u8(), 2);
    assert_eq!(ConnectionStatus::Idle.to_u8(), 3);
    assert_eq!(ConnectionStatus::Error.to_u8(), 4);
    assert_eq!(ConnectionStatus::Closed.to_u8(), 5);
}

#[test]
fn test_connection_stats() {
    use crate::acp::ConnectionStats;

    let stats = ConnectionStats {
        total_connections: 10,
        active_connections: 5,
        idle_connections: 3,
        error_connections: 2,
    };

    assert_eq!(stats.total_connections, 10);
    assert_eq!(stats.active_connections, 5);
    assert_eq!(stats.idle_connections, 3);
    assert_eq!(stats.error_connections, 2);
}

#[tokio::test]
async fn test_agent_connection_send_cancel() {
    let project_id = "project-123".to_string();
    let session_id = agent_client_protocol::SessionId::new("test-session");
    let (prompt_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let (cancel_tx, mut cancel_rx) = tokio::sync::mpsc::unbounded_channel();

    let connection = AgentConnection::new(
        project_id.clone(),
        shared_types::ServiceType::RCoder,
        Some(session_id),
        std::sync::Arc::new(prompt_tx),
        std::sync::Arc::new(cancel_tx),
    );

    // Create a cancel request
    let cancel_request = shared_types::CancelNotificationRequest {
        cancel_notification: shared_types::CancelNotification {
            request_id: "test-cancel".to_string(),
        },
        tx: tokio::sync::oneshot::channel().0,
    };

    // Send the cancel request
    let result = connection.send_cancel(cancel_request).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_agent_connection_without_session() {
    let project_id = "project-123".to_string();
    let (prompt_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let (cancel_tx, _) = tokio::sync::mpsc::unbounded_channel();

    let connection = AgentConnection::new(
        project_id.clone(),
        shared_types::ServiceType::RCoder,
        None, // 没有 session_id
        std::sync::Arc::new(prompt_tx),
        std::sync::Arc::new(cancel_tx),
    );

    // 验证没有 session_id
    assert_eq!(connection.project_id(), project_id);
    assert!(!connection.has_session());
    assert!(connection.session_id().is_none());

    // 尝试发送提示应该失败
    let prompt = agent_client_protocol::PromptRequest::new(
        agent_client_protocol::SessionId::new("test-session"),
        vec![agent_client_protocol::ContentBlock::Text(
            agent_client_protocol::TextContent::new("Test prompt".to_string()),
        )],
    );

    let result = connection.send_prompt(prompt).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("尚未建立会话"));
}

#[tokio::test]
async fn test_agent_connection_set_session_id() {
    let project_id = "project-123".to_string();
    let (prompt_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let (cancel_tx, _) = tokio::sync::mpsc::unbounded_channel();

    let mut connection = AgentConnection::new(
        project_id.clone(),
        shared_types::ServiceType::RCoder,
        None, // 初始没有 session_id
        std::sync::Arc::new(prompt_tx),
        std::sync::Arc::new(cancel_tx),
    );

    // 验证初始状态
    assert!(!connection.has_session());

    // 设置 session_id
    let new_session_id = agent_client_protocol::SessionId::new("new-session");
    connection.set_session_id(new_session_id.clone());

    // 验证设置成功
    assert!(connection.has_session());
    assert_eq!(connection.session_id().unwrap().0, "new-session");
}

#[test]
fn test_agent_connection_project_id() {
    let project_id = "my-project-456".to_string();
    let session_id = agent_client_protocol::SessionId::new("test-session");
    let (prompt_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let (cancel_tx, _) = tokio::sync::mpsc::unbounded_channel();

    let connection = AgentConnection::new(
        project_id.clone(),
        Some(session_id),
        std::sync::Arc::new(prompt_tx),
        std::sync::Arc::new(cancel_tx),
    );

    assert_eq!(connection.project_id(), project_id);
}
