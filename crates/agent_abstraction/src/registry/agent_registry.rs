//! Agent registry implementation.

use std::collections::HashMap;
use std::sync::Arc;

use dashmap::DashMap;

use super::super::Agent;

/// Agent specification structure
#[derive(Debug, Clone)]
pub struct AgentSpec {
    /// Agent unique identifier
    pub agent_id: String,

    /// Command to execute
    pub command: String,

    /// Command arguments
    pub args: Vec<String>,

    /// Environment variables
    pub env: HashMap<String, String>,

    /// Whether enabled
    pub enabled: bool,

    /// Metadata
    pub metadata: HashMap<String, String>,
}

impl AgentSpec {
    /// Create a new AgentSpec
    pub fn new(agent_id: String, command: String) -> Self {
        Self {
            agent_id,
            command,
            args: Vec::new(),
            env: HashMap::new(),
            enabled: true,
            metadata: HashMap::new(),
        }
    }

    /// Add an argument
    pub fn with_arg(mut self, arg: String) -> Self {
        self.args.push(arg);
        self
    }

    /// Add an environment variable
    pub fn with_env<K, V>(mut self, key: K, value: V) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Set enabled status
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Add metadata
    pub fn with_metadata<K, V>(mut self, key: K, value: V) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

impl Default for AgentSpec {
    fn default() -> Self {
        Self {
            agent_id: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            enabled: true,
            metadata: HashMap::new(),
        }
    }
}

/// Agent registry for managing Agent implementations
///
/// 使用 agent_id 作为 key 来注册和管理 Agent
pub struct AgentRegistry {
    /// Registered agents by agent_id
    agents: DashMap<String, Arc<dyn Agent>>,

    /// Agent specifications by agent_id
    specs: DashMap<String, AgentSpec>,
}

impl AgentRegistry {
    /// Create a new registry
    pub fn new() -> Self {
        Self {
            agents: DashMap::new(),
            specs: DashMap::new(),
        }
    }

    /// Register an Agent specification (without implementation)
    pub fn register(
        &self,
        spec: AgentSpec,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let agent_id = spec.agent_id.clone();

        // Check if already registered
        if self.specs.contains_key(&agent_id) {
            return Err(format!("Agent '{}' already registered", agent_id).into());
        }

        // Insert spec
        self.specs.insert(agent_id, spec);

        Ok(())
    }

    /// Register an Agent implementation with specification
    pub fn register_with_implementation(
        &self,
        spec: AgentSpec,
        agent: Arc<dyn Agent>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let agent_id = spec.agent_id.clone();

        // Check if already registered
        if self.agents.contains_key(&agent_id) {
            return Err(format!("Agent '{}' already registered", agent_id).into());
        }

        // Insert into maps
        self.agents.insert(agent_id.clone(), agent);
        self.specs.insert(agent_id, spec);

        Ok(())
    }

    /// Get an Agent implementation by agent_id
    pub fn get_implementation(
        &self,
        agent_id: &str,
    ) -> Result<Arc<dyn Agent>, Box<dyn std::error::Error + Send + Sync>> {
        self.agents
            .get(agent_id)
            .map(|entry| Arc::clone(entry.value()))
            .ok_or_else(|| format!("Agent '{}' not found", agent_id).into())
    }

    /// Get AgentSpec by agent_id
    pub fn get(&self, agent_id: &str) -> Option<AgentSpec> {
        self.specs.get(agent_id).map(|entry| entry.value().clone())
    }

    /// Get AgentSpec by agent_id (Result version)
    pub fn get_spec(
        &self,
        agent_id: &str,
    ) -> Result<AgentSpec, Box<dyn std::error::Error + Send + Sync>> {
        self.specs
            .get(agent_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| format!("Spec for Agent '{}' not found", agent_id).into())
    }

    /// List all registered Agent specs
    pub fn list(&self) -> Vec<AgentSpec> {
        self.specs.iter().map(|entry| entry.value().clone()).collect()
    }

    /// List all enabled Agent specs
    pub fn list_enabled(&self) -> Vec<AgentSpec> {
        self.specs
            .iter()
            .filter(|entry| entry.value().enabled)
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// List all registered agent IDs
    pub fn list_agent_ids(&self) -> Vec<String> {
        self.specs.iter().map(|entry| entry.key().clone()).collect()
    }

    /// Unregister an Agent by agent_id
    pub fn unregister(
        &self,
        agent_id: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Remove from both maps
        let _ = self.agents.remove(agent_id);
        let _ = self.specs.remove(agent_id);

        Ok(())
    }

    /// Get the number of registered agents
    pub fn len(&self) -> usize {
        self.specs.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// Check if an agent_id is registered
    pub fn contains(&self, agent_id: &str) -> bool {
        self.specs.contains_key(agent_id)
    }

    /// Clear all registrations
    pub fn clear(&self) {
        self.agents.clear();
        self.specs.clear();
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}
