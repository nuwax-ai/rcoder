//! ACP connection configuration.

use std::time::Duration;

/// Configuration for ACP connection pool
#[derive(Debug, Clone)]
pub struct AcpConnectionConfig {
    /// Maximum idle time for a connection (default: 300 seconds)
    pub max_idle_time: Duration,

    /// Interval for cleaning up idle connections (default: 60 seconds)
    pub cleanup_interval: Duration,

    /// Minimum protection duration for newly created containers/connections (default: 300 seconds)
    /// Containers created within this time window will not be cleaned up, even if they appear idle.
    /// This prevents premature cleanup of recently created containers in concurrent scenarios.
    pub min_protection_duration: Duration,

    /// Timeout for establishing connections (default: 30 seconds)
    pub connection_timeout: Duration,

    /// Maximum number of connections in the pool (default: 100)
    pub max_connections: usize,
}

impl AcpConnectionConfig {
    /// Create a new configuration with default values
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a custom configuration
    pub fn with_max_idle_time(mut self, max_idle_time: Duration) -> Self {
        self.max_idle_time = max_idle_time;
        self
    }

    /// Set cleanup interval
    pub fn with_cleanup_interval(mut self, cleanup_interval: Duration) -> Self {
        self.cleanup_interval = cleanup_interval;
        self
    }

    /// Set minimum protection duration for newly created containers
    pub fn with_min_protection_duration(mut self, min_protection_duration: Duration) -> Self {
        self.min_protection_duration = min_protection_duration;
        self
    }

    /// Set connection timeout
    pub fn with_connection_timeout(mut self, connection_timeout: Duration) -> Self {
        self.connection_timeout = connection_timeout;
        self
    }

    /// Set max connections
    pub fn with_max_connections(mut self, max_connections: usize) -> Self {
        self.max_connections = max_connections;
        self
    }

    /// Load configuration from environment variables
    pub fn from_env() -> Self {
        let mut config = Self::default();

        // Parse max_idle_time from environment
        if let Ok(val) = std::env::var("ACP_MAX_IDLE_TIME") {
            if let Ok(seconds) = val.parse::<u64>() {
                config.max_idle_time = Duration::from_secs(seconds);
            }
        }

        // Parse cleanup_interval from environment
        if let Ok(val) = std::env::var("ACP_CLEANUP_INTERVAL") {
            if let Ok(seconds) = val.parse::<u64>() {
                config.cleanup_interval = Duration::from_secs(seconds);
            }
        }

        // Parse min_protection_duration from environment
        if let Ok(val) = std::env::var("ACP_MIN_PROTECTION_DURATION") {
            if let Ok(seconds) = val.parse::<u64>() {
                config.min_protection_duration = Duration::from_secs(seconds);
            }
        }

        // Parse connection_timeout from environment
        if let Ok(val) = std::env::var("ACP_CONNECTION_TIMEOUT") {
            if let Ok(seconds) = val.parse::<u64>() {
                config.connection_timeout = Duration::from_secs(seconds);
            }
        }

        // Parse max_connections from environment
        if let Ok(val) = std::env::var("ACP_MAX_CONNECTIONS") {
            if let Ok(connections) = val.parse::<usize>() {
                config.max_connections = connections;
            }
        }

        config
    }
}

impl Default for AcpConnectionConfig {
    fn default() -> Self {
        Self {
            max_idle_time: Duration::from_secs(300),   // 5 minutes
            cleanup_interval: Duration::from_secs(60), // 1 minute
            min_protection_duration: Duration::from_secs(300), // 5 minutes
            connection_timeout: Duration::from_secs(30), // 30 seconds
            max_connections: 100,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AcpConnectionConfig::default();
        assert_eq!(config.max_idle_time, Duration::from_secs(300));
        assert_eq!(config.cleanup_interval, Duration::from_secs(60));
        assert_eq!(config.min_protection_duration, Duration::from_secs(300));
        assert_eq!(config.connection_timeout, Duration::from_secs(30));
        assert_eq!(config.max_connections, 100);
    }

    #[test]
    fn test_custom_config() {
        let config = AcpConnectionConfig::default()
            .with_max_idle_time(Duration::from_secs(600))
            .with_cleanup_interval(Duration::from_secs(120))
            .with_min_protection_duration(Duration::from_secs(180))
            .with_connection_timeout(Duration::from_secs(60))
            .with_max_connections(200);

        assert_eq!(config.max_idle_time, Duration::from_secs(600));
        assert_eq!(config.cleanup_interval, Duration::from_secs(120));
        assert_eq!(config.min_protection_duration, Duration::from_secs(180));
        assert_eq!(config.connection_timeout, Duration::from_secs(60));
        assert_eq!(config.max_connections, 200);
    }

    #[test]
    fn test_config_builder() {
        let config = AcpConnectionConfig::new()
            .with_max_idle_time(Duration::from_secs(900))
            .with_max_connections(50);

        assert_eq!(config.max_idle_time, Duration::from_secs(900));
        assert_eq!(config.cleanup_interval, Duration::from_secs(60)); // default
        assert_eq!(config.min_protection_duration, Duration::from_secs(300)); // default
        assert_eq!(config.max_connections, 50);
    }

    #[test]
    fn test_min_protection_duration() {
        let config =
            AcpConnectionConfig::default().with_min_protection_duration(Duration::from_secs(600));

        assert_eq!(config.min_protection_duration, Duration::from_secs(600));
    }
}
