//! 代理配置模块
//!
//! 定义了端口反向代理的配置结构体和相关功能。

use structopt::StructOpt;

/// 端口反向代理配置
#[derive(Debug, Clone, StructOpt)]
pub struct ProxyConfig {
    /// 监听端口
    #[structopt(long, default_value = "8080")]
    pub listen_port: u16,

    /// 默认后端端口（当 URL 中没有 port 参数时使用）
    #[structopt(long, default_value = "3000")]
    pub default_backend_port: u16,

    /// 后端服务主机（默认为 localhost）
    #[structopt(long, default_value = "127.0.0.1")]
    pub backend_host: String,

    /// URL 中端口参数的名称
    #[structopt(long, default_value = "port")]
    pub port_param: String,

    /// Pingora 配置文件路径
    #[structopt(long)]
    pub config_file: Option<String>,

    /// 启用详细日志
    #[structopt(long)]
    pub verbose: bool,
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
        }
    }
}

impl ProxyConfig {
    /// 验证配置的有效性
    pub fn validate(&self) -> Result<(), String> {
        if self.listen_port == 0 {
            return Err("监听端口不能为 0".to_string());
        }

        if self.default_backend_port == 0 {
            return Err("默认后端端口不能为 0".to_string());
        }

        if self.backend_host.is_empty() {
            return Err("后端主机地址不能为空".to_string());
        }

        if self.port_param.is_empty() {
            return Err("端口参数名不能为空".to_string());
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
