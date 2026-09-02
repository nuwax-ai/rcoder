//! 代理配置模块
//!
//! 定义了端口反向代理的配置结构体和相关功能。
//
// 注：历史上此结构体带 StructOpt derive，但 from_args() 从未被调用（配置全部
// 经 Default/字面量构造），故随 structopt 退役（unmaintained，RUSTSEC-2022-0104）
// 一并移除；默认值以手写 Default 为唯一来源。

/// 端口反向代理配置
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// 监听端口
    pub listen_port: u16,

    /// 默认后端端口（当 URL 中没有 port 参数时使用）
    pub default_backend_port: u16,

    /// 后端服务主机（默认为 localhost）
    pub backend_host: String,

    /// URL 中端口参数的名称
    pub port_param: String,

    /// Pingora 配置文件路径
    pub config_file: Option<String>,

    /// 启用详细日志
    pub verbose: bool,

    /// 请求超时（秒），None 使用默认 600
    pub request_timeout_seconds: Option<u64>,
    /// 连接建立超时（秒），None 使用默认 10
    pub connect_timeout_seconds: Option<u64>,
    /// 连接池空闲超时（秒），None 使用默认 90
    pub pool_idle_timeout_seconds: Option<u64>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            listen_port: 8080,
            default_backend_port: 3000,
            backend_host: "127.0.0.1".to_string(),
            port_param: "port".to_string(),
            config_file: None,
            verbose: false,
            request_timeout_seconds: None,
            connect_timeout_seconds: None,
            pool_idle_timeout_seconds: None,
        }
    }
}

impl ProxyConfig {
    /// 验证配置的有效性
    pub fn validate(&self) -> Result<(), String> {
        if self.listen_port == 0 {
            return Err("Listen port cannot be 0".to_string());
        }

        if self.default_backend_port == 0 {
            return Err("Default backend port cannot be 0".to_string());
        }

        if self.backend_host.is_empty() {
            return Err("Backend host address cannot be empty".to_string());
        }

        if self.port_param.is_empty() {
            return Err("Port parameter name cannot be empty".to_string());
        }

        Ok(())
    }

    /// 创建默认配置
    pub fn new() -> Self {
        Self::default()
    }

    /// 创建自定义监听端口的配置
    pub fn with_listen_port(port: u16) -> Self {
        Self {
            listen_port: port,
            ..Self::default()
        }
    }

    /// 设置后端主机
    pub fn with_backend_host(mut self, host: impl Into<String>) -> Self {
        self.backend_host = host.into();
        self
    }

    /// 设置端口参数名
    pub fn with_port_param(mut self, param: impl Into<String>) -> Self {
        self.port_param = param.into();
        self
    }
}
