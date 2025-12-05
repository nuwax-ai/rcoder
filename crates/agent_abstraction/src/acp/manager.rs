//! ACP connection pool manager.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;

use crate::acp::AgentConnection;

/// Connection statistics
#[derive(Debug, Clone)]
pub struct ConnectionStats {
    pub total_connections: usize,
    pub max_connections: usize,
    pub cleanup_interval: Duration,
    pub max_idle_time: Duration,
}

/// ACP connection pool manager
#[derive(Debug)]
pub struct AcpConnectionManager {
    /// Connection pool
    connections: DashMap<String, Arc<AgentConnection>>,

    /// Maximum number of connections
    max_connections: usize,

    /// Cleanup interval
    cleanup_interval: Duration,

    /// Maximum idle time
    max_idle_time: Duration,
}

impl AcpConnectionManager {
    /// Create a new connection manager
    pub fn new(
        max_connections: usize,
        cleanup_interval: Duration,
        max_idle_time: Duration,
    ) -> Self {
        Self {
            connections: DashMap::new(),
            max_connections,
            cleanup_interval,
            max_idle_time,
        }
    }

    /// Get a connection by agent_id
    ///
    /// Returns an existing connection from the pool if available.
    /// Returns an error if the connection is not found.
    pub fn get_connection(
        &self,
        agent_id: &str,
    ) -> Result<Arc<AgentConnection>, Box<dyn std::error::Error + Send + Sync>> {
        self.connections
            .get(agent_id)
            .map(|conn| Arc::clone(&conn))
            .ok_or_else(|| format!("Connection not found for agent_id: {}", agent_id).into())
    }

    /// Try to get a connection, returns None if not found
    pub fn try_get_connection(&self, agent_id: &str) -> Option<Arc<AgentConnection>> {
        self.connections.get(agent_id).map(|conn| Arc::clone(&conn))
    }

    /// Check if a connection exists
    pub fn has_connection(&self, agent_id: &str) -> bool {
        self.connections.contains_key(agent_id)
    }

    /// Add a connection to the pool
    ///
    /// Returns error if the pool is at maximum capacity.
    pub fn add_connection(
        &self,
        agent_id: String,
        connection: Arc<AgentConnection>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.connections.len() >= self.max_connections {
            return Err(format!(
                "Connection pool at maximum capacity ({})",
                self.max_connections
            )
            .into());
        }
        self.connections.insert(agent_id, connection);
        Ok(())
    }

    /// Remove a connection from the pool
    pub fn remove_connection(&self, agent_id: &str) -> Option<Arc<AgentConnection>> {
        self.connections.remove(agent_id).map(|(_, conn)| conn)
    }

    /// Get all connection IDs
    pub fn get_connection_ids(&self) -> Vec<String> {
        self.connections.iter().map(|r| r.key().clone()).collect()
    }

    /// Get the number of active connections
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Clear all connections
    pub fn clear(&self) {
        self.connections.clear();
    }

    /// Get statistics
    pub fn get_stats(&self) -> ConnectionStats {
        ConnectionStats {
            total_connections: self.connections.len(),
            max_connections: self.max_connections,
            cleanup_interval: self.cleanup_interval,
            max_idle_time: self.max_idle_time,
        }
    }
}

impl Default for AcpConnectionManager {
    fn default() -> Self {
        Self::new(10000, Duration::from_secs(60), Duration::from_secs(300))
    }
}
