//! Tests for lifecycle manager module.

use super::*;
use crate::lifecycle::{AgentLifecycleManager, AgentStatusInfo};
use agent_client_protocol::SessionId;
use std::time::Duration;

#[tokio::test]
async fn test_lifecycle_manager_creation() {
    let manager = AgentLifecycleManager::new();
    assert_eq!(manager.agent_count(), 0);
}

#[tokio::test]
async fn test_spawn_agent() {
    let manager = AgentLifecycleManager::new();

    let info = crate::traits::agent::ProcessLaunchInfo {
        id: "test-agent".to_string(),
        name: "test".to_string(),
        command: "echo".to_string(),
        args: vec!["hello".to_string()],
        working_dir: std::path::PathBuf::new(),
        env: std::collections::HashMap::new(),
    };

    let result = manager.spawn_agent(info).await;
    assert!(result.is_ok());

    let process = result.unwrap();
    assert_eq!(process.id(), "test-agent");
}

#[tokio::test]
async fn test_register_agent() {
    let manager = Arc::new(AgentLifecycleManager::new());

    let info = crate::traits::agent::ProcessLaunchInfo {
        id: "test-agent-2".to_string(),
        name: "test".to_string(),
        command: "echo".to_string(),
        args: vec!["hello".to_string()],
        working_dir: std::path::PathBuf::new(),
        env: std::collections::HashMap::new(),
    };

    let process = manager.spawn_agent(info).await.unwrap();

    let launched_agent = crate::LaunchedAgent {
        process,
        client_conn: std::sync::Arc::new(agent_client_protocol::ClientSideConnection::new()),
        session_id: SessionId::new("test-session"),
        cancel_token: tokio_util::sync::CancellationToken::new(),
        stderr_task: None,
    };

    let result = manager
        .register_agent("test-agent-2".to_string(), Arc::new(launched_agent))
        .await;
    assert!(result.is_ok());

    assert_eq!(manager.agent_count(), 1);
}

#[tokio::test]
async fn test_stop_agent() {
    let manager = Arc::new(AgentLifecycleManager::new());

    let info = crate::traits::agent::ProcessLaunchInfo {
        id: "test-agent-3".to_string(),
        name: "test".to_string(),
        command: "echo".to_string(),
        args: vec!["hello".to_string()],
        working_dir: std::path::PathBuf::new(),
        env: std::collections::HashMap::new(),
    };

    let process = manager.spawn_agent(info).await.unwrap();

    let launched_agent = crate::LaunchedAgent {
        process,
        client_conn: std::sync::Arc::new(agent_client_protocol::ClientSideConnection::new()),
        session_id: SessionId::new("test-session"),
        cancel_token: tokio_util::sync::CancellationToken::new(),
        stderr_task: None,
    };

    manager
        .register_agent("test-agent-3".to_string(), Arc::new(launched_agent))
        .await
        .unwrap();

    assert_eq!(manager.agent_count(), 1);

    let result = manager.stop_agent("test-agent-3").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_get_agent_status() {
    let manager = Arc::new(AgentLifecycleManager::new());

    let info = crate::traits::agent::ProcessLaunchInfo {
        id: "test-agent-4".to_string(),
        name: "test".to_string(),
        command: "echo".to_string(),
        args: vec!["hello".to_string()],
        working_dir: std::path::PathBuf::new(),
        env: std::collections::HashMap::new(),
    };

    let process = manager.spawn_agent(info).await.unwrap();
    let session_id = SessionId::new("test-session");

    let launched_agent = crate::LaunchedAgent {
        process,
        client_conn: std::sync::Arc::new(agent_client_protocol::ClientSideConnection::new()),
        session_id: session_id.clone(),
        cancel_token: tokio_util::sync::CancellationToken::new(),
        stderr_task: None,
    };

    manager
        .register_agent("test-agent-4".to_string(), Arc::new(launched_agent))
        .await
        .unwrap();

    let status = manager.get_agent_status("test-agent-4", &session_id).await;
    assert!(status.is_ok());
    assert_eq!(status.unwrap(), shared_types::AgentStatus::Active);
}

#[tokio::test]
async fn test_get_nonexistent_agent_status() {
    let manager = Arc::new(AgentLifecycleManager::new());
    let session_id = SessionId::new("nonexistent");

    let status = manager.get_agent_status("nonexistent", &session_id).await;
    assert!(status.is_err());
}

#[test]
fn test_agent_status_info() {
    let status_info = AgentStatusInfo::new(
        shared_types::AgentStatus::Active,
        Some("test-session".to_string()),
        Some("test-request".to_string()),
    );

    assert_eq!(status_info.status, shared_types::AgentStatus::Active);
    assert_eq!(status_info.session_id, Some("test-session".to_string()));
    assert_eq!(status_info.request_id, Some("test-request".to_string()));

    // Test update_activity
    let initial_time = status_info.last_activity;
    std::thread::sleep(Duration::from_millis(10));
    let mut status_info = status_info;
    status_info.update_activity();
    assert!(status_info.last_activity > initial_time);
}

#[test]
fn test_agent_status_info_update_status() {
    let mut status_info = AgentStatusInfo::new(shared_types::AgentStatus::Idle, None, None);

    assert_eq!(status_info.status, shared_types::AgentStatus::Idle);

    status_info.update_status(shared_types::AgentStatus::Active);
    assert_eq!(status_info.status, shared_types::AgentStatus::Active);
}

#[test]
fn test_agent_status_info_set_session_id() {
    let mut status_info = AgentStatusInfo::new(shared_types::AgentStatus::Active, None, None);

    status_info.set_session_id(Some("new-session".to_string()));
    assert_eq!(status_info.session_id, Some("new-session".to_string()));
}

#[test]
fn test_agent_status_info_set_request_id() {
    let mut status_info = AgentStatusInfo::new(shared_types::AgentStatus::Active, None, None);

    status_info.set_request_id(Some("new-request".to_string()));
    assert_eq!(status_info.request_id, Some("new-request".to_string()));
}

#[tokio::test]
async fn test_get_running_agents() {
    let manager = Arc::new(AgentLifecycleManager::new());

    // Initially empty
    assert!(manager.get_running_agents().is_empty());

    // Add an agent
    let info = crate::traits::agent::ProcessLaunchInfo {
        id: "test-agent-5".to_string(),
        name: "test".to_string(),
        command: "echo".to_string(),
        args: vec!["hello".to_string()],
        working_dir: std::path::PathBuf::new(),
        env: std::collections::HashMap::new(),
    };

    let process = manager.spawn_agent(info).await.unwrap();

    let launched_agent = crate::LaunchedAgent {
        process,
        client_conn: std::sync::Arc::new(agent_client_protocol::ClientSideConnection::new()),
        session_id: SessionId::new("test-session"),
        cancel_token: tokio_util::sync::CancellationToken::new(),
        stderr_task: None,
    };

    manager
        .register_agent("test-agent-5".to_string(), Arc::new(launched_agent))
        .await
        .unwrap();

    let running_agents = manager.get_running_agents();
    assert_eq!(running_agents.len(), 1);
    assert_eq!(running_agents[0], "test-agent-5");
}

#[tokio::test]
async fn test_agent_count() {
    let manager = Arc::new(AgentLifecycleManager::new());
    assert_eq!(manager.agent_count(), 0);

    // Add first agent
    let info1 = crate::traits::agent::ProcessLaunchInfo {
        id: "test-agent-6a".to_string(),
        name: "test".to_string(),
        command: "echo".to_string(),
        args: vec!["hello".to_string()],
        working_dir: std::path::PathBuf::new(),
        env: std::collections::HashMap::new(),
    };

    let process1 = manager.spawn_agent(info1).await.unwrap();
    let launched_agent1 = crate::LaunchedAgent {
        process: process1,
        client_conn: std::sync::Arc::new(agent_client_protocol::ClientSideConnection::new()),
        session_id: SessionId::new("test-session-1"),
        cancel_token: tokio_util::sync::CancellationToken::new(),
        stderr_task: None,
    };
    manager
        .register_agent("test-agent-6a".to_string(), Arc::new(launched_agent1))
        .await
        .unwrap();

    assert_eq!(manager.agent_count(), 1);

    // Add second agent
    let info2 = crate::traits::agent::ProcessLaunchInfo {
        id: "test-agent-6b".to_string(),
        name: "test".to_string(),
        command: "echo".to_string(),
        args: vec!["world".to_string()],
        working_dir: std::path::PathBuf::new(),
        env: std::collections::HashMap::new(),
    };

    let process2 = manager.spawn_agent(info2).await.unwrap();
    let launched_agent2 = crate::LaunchedAgent {
        process: process2,
        client_conn: std::sync::Arc::new(agent_client_protocol::ClientSideConnection::new()),
        session_id: SessionId::new("test-session-2"),
        cancel_token: tokio_util::sync::CancellationToken::new(),
        stderr_task: None,
    };
    manager
        .register_agent("test-agent-6b".to_string(), Arc::new(launched_agent2))
        .await
        .unwrap();

    assert_eq!(manager.agent_count(), 2);
}
