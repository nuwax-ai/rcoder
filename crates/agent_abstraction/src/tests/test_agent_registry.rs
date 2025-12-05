//! Tests for agent registry module.

use super::*;
use crate::registry::{AgentRegistry, AgentSpec};

#[tokio::test]
async fn test_agent_registry_creation() {
    let registry = AgentRegistry::new();
    assert_eq!(registry.len(), 0);
}

#[tokio::test]
async fn test_register_agent() {
    let registry = AgentRegistry::new();

    let spec = AgentSpec {
        agent_id: "test-agent".to_string(),
        command: "claude-code-acp".to_string(),
        args: vec![],
        env: std::collections::HashMap::new(),
        enabled: true,
        metadata: std::collections::HashMap::new(),
    };

    registry.register(spec.clone()).unwrap();
    assert_eq!(registry.len(), 1);
    assert!(registry.contains(&spec.agent_id));
}

#[tokio::test]
async fn test_register_duplicate_agent() {
    let registry = AgentRegistry::new();

    let spec = AgentSpec {
        agent_id: "test-agent".to_string(),
        command: "claude-code-acp".to_string(),
        args: vec![],
        env: std::collections::HashMap::new(),
        enabled: true,
        metadata: std::collections::HashMap::new(),
    };

    registry.register(spec.clone()).unwrap();
    let result = registry.register(spec.clone());
    assert!(result.is_err());
}

#[tokio::test]
async fn test_unregister_agent() {
    let registry = AgentRegistry::new();

    let spec = AgentSpec {
        agent_id: "test-agent".to_string(),
        command: "claude-code-acp".to_string(),
        args: vec![],
        env: std::collections::HashMap::new(),
        enabled: true,
        metadata: std::collections::HashMap::new(),
    };

    registry.register(spec.clone()).unwrap();
    assert_eq!(registry.len(), 1);

    let result = registry.unregister(&spec.agent_id);
    assert!(result.is_ok());
    assert_eq!(registry.len(), 0);
}

#[tokio::test]
async fn test_get_agent_spec() {
    let registry = AgentRegistry::new();

    let spec = AgentSpec {
        agent_id: "test-agent".to_string(),
        command: "claude-code-acp".to_string(),
        args: vec!["--verbose".to_string()],
        env: std::collections::HashMap::new(),
        enabled: true,
        metadata: std::collections::HashMap::new(),
    };

    registry.register(spec.clone()).unwrap();

    let retrieved = registry.get(&spec.agent_id).unwrap();
    assert_eq!(retrieved.agent_id, spec.agent_id);
    assert_eq!(retrieved.command, spec.command);
    assert_eq!(retrieved.args, spec.args);
}

#[tokio::test]
async fn test_get_nonexistent_agent() {
    let registry = AgentRegistry::new();

    let result = registry.get("nonexistent-agent");
    assert!(result.is_none());
}

#[tokio::test]
async fn test_list_agents() {
    let registry = AgentRegistry::new();

    let spec1 = AgentSpec {
        agent_id: "agent-1".to_string(),
        command: "claude-code-acp".to_string(),
        args: vec![],
        env: std::collections::HashMap::new(),
        enabled: true,
        metadata: std::collections::HashMap::new(),
    };

    registry.register(spec1.clone()).unwrap();

    let agents = registry.list();
    assert_eq!(agents.len(), 1);

    let agent_ids: Vec<_> = agents.iter().map(|spec| spec.agent_id.clone()).collect();
    assert!(agent_ids.contains(&"agent-1".to_string()));
}

#[tokio::test]
async fn test_list_enabled_agents() {
    let registry = AgentRegistry::new();

    let spec1 = AgentSpec {
        agent_id: "agent-1".to_string(),
        command: "claude-code-acp".to_string(),
        args: vec![],
        env: std::collections::HashMap::new(),
        enabled: true,
        metadata: std::collections::HashMap::new(),
    };

    registry.register(spec1.clone()).unwrap();

    let enabled_agents = registry.list_enabled();
    assert_eq!(enabled_agents.len(), 1);
    assert_eq!(enabled_agents[0].agent_id, "agent-1");
}

#[tokio::test]
async fn test_clear_registry() {
    let registry = AgentRegistry::new();

    let spec = AgentSpec {
        agent_id: "test-agent".to_string(),
        command: "claude-code-acp".to_string(),
        args: vec![],
        env: std::collections::HashMap::new(),
        enabled: true,
        metadata: std::collections::HashMap::new(),
    };

    registry.register(spec.clone()).unwrap();
    assert_eq!(registry.len(), 1);

    registry.clear();
    assert_eq!(registry.len(), 0);
}

#[test]
fn test_agent_spec_default() {
    let spec = AgentSpec::default();
    assert_eq!(spec.agent_id, "");
    assert!(spec.args.is_empty());
    assert!(spec.env.is_empty());
    assert!(spec.enabled);
    assert!(spec.metadata.is_empty());
}

#[test]
fn test_agent_spec_with_metadata() {
    let mut metadata = std::collections::HashMap::new();
    metadata.insert("version".to_string(), "1.0.0".to_string());
    metadata.insert("author".to_string(), "test".to_string());

    let spec = AgentSpec {
        agent_id: "test-agent".to_string(),
        command: "test-command".to_string(),
        args: vec!["--flag".to_string()],
        env: std::collections::HashMap::new(),
        enabled: true,
        metadata: metadata.clone(),
    };

    assert_eq!(spec.metadata.get("version").unwrap(), "1.0.0");
    assert_eq!(spec.metadata.get("author").unwrap(), "test");
}

#[test]
fn test_agent_spec_env_variables() {
    let mut env = std::collections::HashMap::new();
    env.insert("API_KEY".to_string(), "test-key".to_string());
    env.insert("ENDPOINT".to_string(), "https://example.com".to_string());

    let spec = AgentSpec {
        agent_id: "test-agent".to_string(),
        command: "test-command".to_string(),
        args: vec![],
        env: env.clone(),
        enabled: true,
        metadata: std::collections::HashMap::new(),
    };

    assert_eq!(spec.env.get("API_KEY").unwrap(), "test-key");
    assert_eq!(spec.env.get("ENDPOINT").unwrap(), "https://example.com");
}
