//! 配置子段定义（从 config.rs 拆出——AppConfig 嵌套子 struct 群）。
//!
//! 子段与私有 serde default fn 整体同档；AgentCleanupConfig/GrpcTimeoutConfig/
//! AgentConcurrencyConfig 的 validate 群与 131 行测试随各自 struct 同档。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckConfig {
    pub enabled: bool,
    pub interval_seconds: u64,
    pub timeout_seconds: u64,
    pub healthy_threshold: u32,
    pub unhealthy_threshold: u32,
}

/// 代理配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// 代理监听端口
    pub listen_port: u16,
    /// 默认后端端口
    pub default_backend_port: u16,
    /// 后端服务主机
    pub backend_host: String,
    /// URL 中端口参数的名称
    pub port_param: String,
    /// 健康检查配置
    pub health_check: HealthCheckConfig,
}

/// Agent cleanup configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCleanupConfig {
    /// Idle timeout (seconds), default 300 (5 minutes)
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    /// Cleanup check interval (seconds), default 30
    #[serde(default = "default_cleanup_interval")]
    pub cleanup_interval_secs: u64,
}

/// Deprecated no-op. Agent process count is no longer limited by agent_runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConcurrencyConfig {
    /// Deprecated no-op.
    #[serde(default = "default_concurrency_limit")]
    pub concurrency_limit: usize,
}

/// Deprecated no-op config validation kept for compatibility.
impl AgentConcurrencyConfig {
    pub const MIN_CONCURRENCY_LIMIT: usize = 1;

    pub fn validate(&self) -> Result<(), String> {
        if self.concurrency_limit < Self::MIN_CONCURRENCY_LIMIT {
            return Err(format!(
                "concurrency_limit must be >= {}, current value: {}",
                Self::MIN_CONCURRENCY_LIMIT,
                self.concurrency_limit
            ));
        }
        Ok(())
    }
}

fn default_concurrency_limit() -> usize {
    10
}

impl Default for AgentConcurrencyConfig {
    fn default() -> Self {
        Self {
            concurrency_limit: default_concurrency_limit(),
        }
    }
}

/// gRPC timeout configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrpcTimeoutConfig {
    /// Cancel session timeout (seconds), default 30
    #[serde(default = "default_cancel_timeout")]
    pub cancel_session_timeout_secs: u64,

    /// ACP session creation timeout (seconds), default 100
    #[serde(default = "default_acp_session_timeout")]
    pub acp_session_create_timeout_secs: u64,

    /// Agent cancel call timeout (seconds), default 10
    #[serde(default = "default_agent_cancel_timeout")]
    pub agent_cancel_timeout_secs: u64,

    /// Port check timeout (milliseconds), default 500
    #[serde(default = "default_port_check_timeout")]
    pub port_check_timeout_millis: u64,
}

/// gRPC timeout configuration constants
impl GrpcTimeoutConfig {
    /// Minimum cancel session timeout (5 seconds)
    pub const MIN_CANCEL_TIMEOUT: u64 = 5;
    /// Maximum cancel session timeout (300 seconds = 5 minutes)
    pub const MAX_CANCEL_TIMEOUT: u64 = 300;
    /// Minimum ACP session creation timeout (10 seconds)
    pub const MIN_ACP_SESSION_TIMEOUT: u64 = 10;
    /// Maximum ACP session creation timeout (300 seconds = 5 minutes)
    pub const MAX_ACP_SESSION_TIMEOUT: u64 = 300;
    /// Minimum Agent cancel call timeout (5 seconds)
    pub const MIN_AGENT_CANCEL_TIMEOUT: u64 = 5;
    /// Maximum Agent cancel call timeout (60 seconds)
    pub const MAX_AGENT_CANCEL_TIMEOUT: u64 = 60;
    /// Minimum port check timeout (100 milliseconds)
    pub const MIN_PORT_CHECK_TIMEOUT: u64 = 100;
    /// Maximum port check timeout (10000 milliseconds = 10 seconds)
    pub const MAX_PORT_CHECK_TIMEOUT: u64 = 10000;

    /// Validate that configuration values are within valid ranges
    pub fn validate(&self) -> Result<(), String> {
        if self.cancel_session_timeout_secs < Self::MIN_CANCEL_TIMEOUT
            || self.cancel_session_timeout_secs > Self::MAX_CANCEL_TIMEOUT
        {
            return Err(format!(
                "cancel_session_timeout_secs must be between {} and {}, current: {}",
                Self::MIN_CANCEL_TIMEOUT,
                Self::MAX_CANCEL_TIMEOUT,
                self.cancel_session_timeout_secs
            ));
        }

        if self.acp_session_create_timeout_secs < Self::MIN_ACP_SESSION_TIMEOUT
            || self.acp_session_create_timeout_secs > Self::MAX_ACP_SESSION_TIMEOUT
        {
            return Err(format!(
                "acp_session_create_timeout_secs must be between {} and {}, current: {}",
                Self::MIN_ACP_SESSION_TIMEOUT,
                Self::MAX_ACP_SESSION_TIMEOUT,
                self.acp_session_create_timeout_secs
            ));
        }

        if self.agent_cancel_timeout_secs < Self::MIN_AGENT_CANCEL_TIMEOUT
            || self.agent_cancel_timeout_secs > Self::MAX_AGENT_CANCEL_TIMEOUT
        {
            return Err(format!(
                "agent_cancel_timeout_secs must be between {} and {}, current: {}",
                Self::MIN_AGENT_CANCEL_TIMEOUT,
                Self::MAX_AGENT_CANCEL_TIMEOUT,
                self.agent_cancel_timeout_secs
            ));
        }

        if self.port_check_timeout_millis < Self::MIN_PORT_CHECK_TIMEOUT
            || self.port_check_timeout_millis > Self::MAX_PORT_CHECK_TIMEOUT
        {
            return Err(format!(
                "port_check_timeout_millis must be between {} and {}, current: {}",
                Self::MIN_PORT_CHECK_TIMEOUT,
                Self::MAX_PORT_CHECK_TIMEOUT,
                self.port_check_timeout_millis
            ));
        }

        Ok(())
    }
}

/// Agent 清理配置常量
impl AgentCleanupConfig {
    /// 最小闲置超时时间（10 秒）
    pub const MIN_IDLE_TIMEOUT: u64 = 10;
    /// 最大闲置超时时间（24 小时）
    pub const MAX_IDLE_TIMEOUT: u64 = 24 * 60 * 60;
    /// 最小清理检查间隔（5 秒）
    pub const MIN_CLEANUP_INTERVAL: u64 = 5;
    /// 最大清理检查间隔（1 小时）
    pub const MAX_CLEANUP_INTERVAL: u64 = 60 * 60;

    /// 验证配置值是否在有效范围内
    pub fn validate(&self) -> Result<(), String> {
        if self.idle_timeout_secs < Self::MIN_IDLE_TIMEOUT
            || self.idle_timeout_secs > Self::MAX_IDLE_TIMEOUT
        {
            return Err(format!(
                "idle_timeout_secs must be between {} and {}, current value: {}",
                Self::MIN_IDLE_TIMEOUT,
                Self::MAX_IDLE_TIMEOUT,
                self.idle_timeout_secs
            ));
        }

        if self.cleanup_interval_secs < Self::MIN_CLEANUP_INTERVAL
            || self.cleanup_interval_secs > Self::MAX_CLEANUP_INTERVAL
        {
            return Err(format!(
                "cleanup_interval_secs must be between {} and {}, current value: {}",
                Self::MIN_CLEANUP_INTERVAL,
                Self::MAX_CLEANUP_INTERVAL,
                self.cleanup_interval_secs
            ));
        }

        Ok(())
    }
}

#[cfg(feature = "http-server")]
fn default_idle_timeout() -> u64 {
    24 * 60 * 60 // 24 小时（Tauri 客户端模式）
}

#[cfg(not(feature = "http-server"))]
fn default_idle_timeout() -> u64 {
    300 // 5 分钟（CLI 模式）
}

fn default_cleanup_interval() -> u64 {
    30 // 30 秒
}

fn default_cancel_timeout() -> u64 {
    30 // 30 秒
}

fn default_acp_session_timeout() -> u64 {
    100 // 100 秒
}

fn default_agent_cancel_timeout() -> u64 {
    10 // 10 秒
}

fn default_port_check_timeout() -> u64 {
    500 // 500 毫秒
}

impl Default for AgentCleanupConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: default_idle_timeout(),
            cleanup_interval_secs: default_cleanup_interval(),
        }
    }
}

impl Default for GrpcTimeoutConfig {
    fn default() -> Self {
        Self {
            cancel_session_timeout_secs: default_cancel_timeout(),
            acp_session_create_timeout_secs: default_acp_session_timeout(),
            agent_cancel_timeout_secs: default_agent_cancel_timeout(),
            port_check_timeout_millis: default_port_check_timeout(),
        }
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            listen_port: 8088,
            default_backend_port: 8086,
            backend_host: "127.0.0.1".to_string(),
            port_param: "port".to_string(),
            health_check: HealthCheckConfig {
                enabled: true,
                interval_seconds: 5,
                timeout_seconds: 1,
                healthy_threshold: 2,
                unhealthy_threshold: 3,
            },
        }
    }
}

/// 加载配置
#[cfg(test)]
mod tests {
    use super::super::{CliArgs, load_config_with_args};
    use super::*;

    #[test]
    fn test_agent_cleanup_default_values() {
        let config = AgentCleanupConfig::default();
        #[cfg(feature = "http-server")]
        assert_eq!(config.idle_timeout_secs, 86400); // 24 hours in http-server mode
        #[cfg(not(feature = "http-server"))]
        assert_eq!(config.idle_timeout_secs, 300); // 5 minutes in CLI mode
        assert_eq!(config.cleanup_interval_secs, 30);
    }

    #[test]
    fn test_agent_cleanup_validate_valid_range() {
        let config = AgentCleanupConfig {
            idle_timeout_secs: 600,    // 10 分钟
            cleanup_interval_secs: 60, // 1 分钟
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_agent_cleanup_validate_min_boundaries() {
        // 测试最小边界值
        let config = AgentCleanupConfig {
            idle_timeout_secs: AgentCleanupConfig::MIN_IDLE_TIMEOUT,
            cleanup_interval_secs: AgentCleanupConfig::MIN_CLEANUP_INTERVAL,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_agent_cleanup_validate_max_boundaries() {
        // 测试最大边界值
        let config = AgentCleanupConfig {
            idle_timeout_secs: AgentCleanupConfig::MAX_IDLE_TIMEOUT,
            cleanup_interval_secs: AgentCleanupConfig::MAX_CLEANUP_INTERVAL,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_agent_cleanup_validate_idle_timeout_too_small() {
        let config = AgentCleanupConfig {
            idle_timeout_secs: 5, // 小于最小值 10
            cleanup_interval_secs: 30,
        };
        assert!(config.validate().is_err());
        let err = config.validate().unwrap_err();
        assert!(err.contains("idle_timeout_secs"));
    }

    #[test]
    fn test_agent_cleanup_validate_idle_timeout_too_large() {
        let config = AgentCleanupConfig {
            idle_timeout_secs: 100000, // 大于最大值 86400
            cleanup_interval_secs: 30,
        };
        assert!(config.validate().is_err());
        let err = config.validate().unwrap_err();
        assert!(err.contains("idle_timeout_secs"));
    }

    #[test]
    fn test_agent_cleanup_validate_cleanup_interval_too_small() {
        let config = AgentCleanupConfig {
            idle_timeout_secs: 180,
            cleanup_interval_secs: 2, // 小于最小值 5
        };
        assert!(config.validate().is_err());
        let err = config.validate().unwrap_err();
        assert!(err.contains("cleanup_interval_secs"));
    }

    #[test]
    fn test_agent_cleanup_validate_cleanup_interval_too_large() {
        let config = AgentCleanupConfig {
            idle_timeout_secs: 180,
            cleanup_interval_secs: 5000, // 大于最大值 3600
        };
        assert!(config.validate().is_err());
        let err = config.validate().unwrap_err();
        assert!(err.contains("cleanup_interval_secs"));
    }

    #[test]
    fn test_agent_cleanup_validate_both_invalid() {
        let config = AgentCleanupConfig {
            idle_timeout_secs: 0,
            cleanup_interval_secs: 0,
        };
        assert!(config.validate().is_err());
        // 应该先检测到 idle_timeout_secs 的错误
        let err = config.validate().unwrap_err();
        assert!(err.contains("idle_timeout_secs"));
    }

    #[test]
    fn proxy_default_backend_port_uses_final_cli_port() {
        let config = load_config_with_args(CliArgs {
            port: Some(8286),
            projects_dir: None,
            enable_proxy: true,
            proxy_port: Some(8089),
            default_backend_port: None,
        });

        let proxy_config = config.proxy_config.expect("proxy config");
        assert_eq!(config.port, 8286);
        assert_eq!(proxy_config.listen_port, 8089);
        assert_eq!(proxy_config.default_backend_port, 8286);
    }

    #[test]
    fn proxy_default_backend_port_keeps_explicit_cli_value() {
        let config = load_config_with_args(CliArgs {
            port: Some(8286),
            projects_dir: None,
            enable_proxy: true,
            proxy_port: Some(8089),
            default_backend_port: Some(9000),
        });

        let proxy_config = config.proxy_config.expect("proxy config");
        assert_eq!(config.port, 8286);
        assert_eq!(proxy_config.listen_port, 8089);
        assert_eq!(proxy_config.default_backend_port, 9000);
    }
}
