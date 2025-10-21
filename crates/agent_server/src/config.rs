//! Agent Server 配置模块

use anyhow::Result;
use serde::{Deserialize, Serialize};
pub use shared_types::AgentType;
use std::path::PathBuf;
use std::time::Duration;

/// Agent Server 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentServerConfig {
    /// HTTP 服务端口
    pub port: u16,

    /// Agent 类型
    pub agent_type: AgentType,

    /// 项目 ID
    pub project_id: String,

    /// 会话 ID (可选)
    pub session_id: Option<String>,

    /// 工作目录
    pub work_dir: PathBuf,

    /// 额外的启动参数
    pub extra_args: Vec<String>,

    /// 最大并发会话数
    pub max_sessions: usize,

    /// 会话超时时间 (秒)
    pub session_timeout_secs: u64,

    /// 健康检查间隔 (秒)
    pub health_check_interval_secs: u64,

    /// 是否启用调试模式
    pub debug_mode: bool,

    /// 是否启用 CORS
    pub enable_cors: bool,

    /// 请求超时时间 (秒)
    pub request_timeout_secs: u64,

    /// 最大请求大小 (字节)
    pub max_request_size_bytes: usize,
}

impl Default for AgentServerConfig {
    fn default() -> Self {
        Self {
            port: 8086,
            agent_type: AgentType::Claude,
            project_id: "default_project".to_string(),
            session_id: None,
            work_dir: PathBuf::from("/app/workspace"),
            extra_args: vec![],
            max_sessions: 10,
            session_timeout_secs: 3600, // 1 hour
            health_check_interval_secs: 30,
            debug_mode: false,
            enable_cors: true,
            request_timeout_secs: 120, // 2 minutes
            max_request_size_bytes: 10 * 1024 * 1024, // 10MB
        }
    }
}

impl AgentServerConfig {
    /// 从环境变量加载配置
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(port) = std::env::var("AGENT_SERVER_PORT") {
            if let Ok(port) = port.parse() {
                config.port = port;
            }
        }

        if let Ok(agent_type) = std::env::var("AGENT_TYPE") {
            if let Ok(agent_type) = agent_type.parse() {
                config.agent_type = agent_type;
            }
        }

        if let Ok(project_id) = std::env::var("PROJECT_ID") {
            config.project_id = project_id;
        }

        if let Ok(session_id) = std::env::var("SESSION_ID") {
            config.session_id = Some(session_id);
        }

        if let Ok(work_dir) = std::env::var("WORK_DIR") {
            config.work_dir = work_dir.into();
        }

        if let Ok(max_sessions) = std::env::var("MAX_SESSIONS") {
            if let Ok(max_sessions) = max_sessions.parse() {
                config.max_sessions = max_sessions;
            }
        }

        if let Ok(debug_mode) = std::env::var("DEBUG_MODE") {
            config.debug_mode = debug_mode.parse().unwrap_or(false);
        }

        config
    }

    /// 验证配置
    pub fn validate(&self) -> Result<()> {
        if self.port == 0 {
            return Err(anyhow::anyhow!("端口号不能为 0"));
        }

        if self.project_id.is_empty() {
            return Err(anyhow::anyhow!("项目 ID 不能为空"));
        }

        if !self.work_dir.exists() {
            return Err(anyhow::anyhow!("工作目录不存在: {:?}", self.work_dir));
        }

        if self.max_sessions == 0 {
            return Err(anyhow::anyhow!("最大会话数必须大于 0"));
        }

        Ok(())
    }

    /// 获取请求超时时间
    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout_secs)
    }

    /// 获取会话超时时间
    pub fn session_timeout(&self) -> Duration {
        Duration::from_secs(self.session_timeout_secs)
    }

    /// 获取健康检查间隔
    pub fn health_check_interval(&self) -> Duration {
        Duration::from_secs(self.health_check_interval_secs)
    }
}

/// 网络配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// 绑定地址
    pub bind_address: String,

    /// 是否启用 TLS
    pub enable_tls: bool,

    /// TLS 证书文件路径
    pub tls_cert_path: Option<String>,

    /// TLS 私钥文件路径
    pub tls_key_path: Option<String>,

    /// 最大连接数
    pub max_connections: usize,

    /// 连接超时时间 (秒)
    pub connection_timeout_secs: u64,

    /// Keep-alive 超时时间 (秒)
    pub keep_alive_timeout_secs: u64,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            bind_address: "0.0.0.0".to_string(),
            enable_tls: false,
            tls_cert_path: None,
            tls_key_path: None,
            max_connections: 1000,
            connection_timeout_secs: 30,
            keep_alive_timeout_secs: 60,
        }
    }
}

/// 日志配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// 日志级别
    pub level: String,

    /// 日志格式 (json, pretty, compact)
    pub format: String,

    /// 是否启用文件日志
    pub enable_file_logging: bool,

    /// 日志文件路径
    pub log_file_path: Option<String>,

    /// 日志文件最大大小 (MB)
    pub max_file_size_mb: u64,

    /// 日志文件保留数量
    pub max_files: usize,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: "json".to_string(),
            enable_file_logging: false,
            log_file_path: None,
            max_file_size_mb: 100,
            max_files: 5,
        }
    }
}