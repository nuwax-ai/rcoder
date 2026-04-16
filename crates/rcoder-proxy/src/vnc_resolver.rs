//! VNC backend resolver module
//!
//! Provides dynamic resolution of VNC backend container IP for transparent proxy.
//! Uses trait abstraction to decouple pingora-proxy and docker_manager.

use async_trait::async_trait;
use std::sync::Arc;

/// VNC backend resolve error
#[derive(Debug, thiserror::Error)]
pub enum VncResolveError {
    /// Container not found
    #[error("container not found: user_id={0}")]
    ContainerNotFound(String),

    /// Container not running
    #[error("container not running: user_id={0}")]
    ContainerNotRunning(String),

    /// Unable to get container IP
    #[error("unable to get container IP: {0}")]
    IpNotAvailable(String),

    /// Query failed
    #[error("query failed: {0}")]
    QueryFailed(String),
}

/// VNC backend info
#[derive(Debug, Clone)]
pub struct VncBackendInfo {
    /// Container IP
    pub container_ip: String,
    /// VNC port (usually 6080)
    pub vnc_port: u16,
    /// Container is running
    pub is_running: bool,
}

impl VncBackendInfo {
    /// Create new VNC backend info
    pub fn new(container_ip: String, vnc_port: u16, is_running: bool) -> Self {
        Self {
            container_ip,
            vnc_port,
            is_running,
        }
    }

    /// Get full backend address (IP:port)
    pub fn backend_addr(&self) -> String {
        format!("{}:{}", self.container_ip, self.vnc_port)
    }
}

/// VNC backend resolver trait
///
/// Used to decouple pingora-proxy and docker_manager,
/// allows different implementation strategies (direct Docker query, cached query, etc.)
#[async_trait]
pub trait VncBackendResolver: Send + Sync {
    /// Resolve VNC backend info by user_id
    ///
    /// # Arguments
    /// * `user_id` - User ID (container identifier in ComputerAgentRunner mode)
    ///
    /// # Returns
    /// * `Ok(VncBackendInfo)` - Successfully resolved VNC backend
    /// * `Err(VncResolveError)` - Resolution failed
    async fn resolve(&self, user_id: &str) -> Result<VncBackendInfo, VncResolveError>;

    /// Check if user has corresponding container (fast check, no detailed info)
    ///
    /// Used to quickly determine if 404 should be returned
    async fn exists(&self, user_id: &str) -> bool;
}

/// Arc wrapper for type erasure
pub type DynVncBackendResolver = Arc<dyn VncBackendResolver>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock resolver for testing
    struct MockResolver {
        users: std::collections::HashMap<String, VncBackendInfo>,
    }

    #[async_trait]
    impl VncBackendResolver for MockResolver {
        async fn resolve(&self, user_id: &str) -> Result<VncBackendInfo, VncResolveError> {
            self.users
                .get(user_id)
                .cloned()
                .ok_or_else(|| VncResolveError::ContainerNotFound(user_id.to_string()))
        }

        async fn exists(&self, user_id: &str) -> bool {
            self.users.contains_key(user_id)
        }
    }

    #[tokio::test]
    async fn test_mock_resolver() {
        let mut users = std::collections::HashMap::new();
        users.insert(
            "user_123".to_string(),
            VncBackendInfo::new("172.17.0.5".to_string(), 6080, true),
        );

        let resolver = MockResolver { users };

        // Test successful resolution
        let info = resolver.resolve("user_123").await.unwrap();
        assert_eq!(info.container_ip, "172.17.0.5");
        assert_eq!(info.vnc_port, 6080);
        assert!(info.is_running);

        // Test resolution failure
        let err = resolver.resolve("nonexistent").await.unwrap_err();
        assert!(matches!(err, VncResolveError::ContainerNotFound(_)));

        // Test existence check
        assert!(resolver.exists("user_123").await);
        assert!(!resolver.exists("nonexistent").await);
    }

    #[test]
    fn test_backend_addr() {
        let info = VncBackendInfo::new("192.168.1.100".to_string(), 6080, true);
        assert_eq!(info.backend_addr(), "192.168.1.100:6080");
    }
}
