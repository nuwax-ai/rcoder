//! Pingora 反向代理库
//!
//! 这个库提供了一个基于端口参数的反向代理服务，可以通过 URL 参数中的端口信息
//! 将请求代理到对应的后端服务。主要用于 Docker 容器中统一端口访问多个前端应用。
//!
//! # 特性
//!
//! - **端口路由**: 支持通过查询参数 (`?port=3000`) 或路径 (`/proxy/3000/path`) 指定目标端口
//! - **动态后端管理**: 运行时添加、移除和管理后端服务
//! - **高性能**: 基于 Axum 和 Reqwest 的异步处理
//! - **灵活配置**: 支持命令行参数和配置文件
//! - **完整日志**: 详细的操作日志和错误处理
//!
//! # 快速开始
//!
//! ```rust,ignore
//! use rcoder_proxy::{ProxyConfig, ProxyServer};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let config = ProxyConfig {
//!         listen_port: 8080,
//!         default_backend_port: 3000,
//!         backend_host: "127.0.0.1".to_string(),
//!         port_param: "port".to_string(),
//!         config_file: None,
//!         verbose: false,
//!     };
//!
//!     let server = ProxyServer::new(config);
//!     server.start().await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! # 使用方式
//!
//! ## 查询参数方式
//!
//! ```bash
//! curl "http://localhost:8080?port=3000"
//! curl "http://localhost:8080/api/data?port=3001&format=json"
//! ```
//!
//! ## 路径方式
//!
//! ```bash
//! curl "http://localhost:8080/proxy/3000/"
//! curl "http://localhost:8080/proxy/3001/api/users"
//! ```
//!
//! ## 默认端口
//!
//! ```bash
//! curl "http://localhost:8080/"  # 自动路由到默认端口 3000
//! ```

// 导入模块
pub mod config;
pub mod pingora_server;
pub mod protocol_convert;
pub mod router;
pub mod server;
pub mod service;
pub mod vnc_resolver;

// 重新导出公共接口
pub use config::ProxyConfig;
pub use pingora_server::PingoraServerManager;
pub use router::{RouteType, create_router, get_routes_documentation};
pub use server::{PingoraServerRunner, ProxyServer, ProxyServerBuilder};
pub use service::{PingoraProxyService, PortProxyService}; // PortProxyService 是别名
pub use vnc_resolver::{
    DynVncBackendResolver, VncBackendInfo, VncBackendResolver, VncResolveError,
};

// 库级别的常量和类型
pub const DEFAULT_PORT: u16 = 8080;
pub const DEFAULT_BACKEND_PORT: u16 = 3000;
pub const DEFAULT_BACKEND_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT_PARAM: &str = "port";

/// 库版本信息
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 库的主要特性
pub mod features {
    /// 支持查询参数端口提取
    pub const QUERY_PARAM_ROUTING: bool = true;

    /// 支持路径端口提取
    pub const PATH_ROUTING: bool = true;

    /// 支持动态后端管理
    pub const DYNAMIC_BACKENDS: bool = true;

    /// 支持配置文件
    pub const CONFIG_FILE_SUPPORT: bool = true;

    /// 支持详细日志
    pub const VERBOSE_LOGGING: bool = true;
}

/// 便捷函数：创建默认配置
pub fn default_config() -> ProxyConfig {
    ProxyConfig::default()
}

/// 便捷函数：创建指定端口的配置
pub fn config_with_port(port: u16) -> ProxyConfig {
    ProxyConfig::with_listen_port(port)
}

/// 便捷函数：创建默认代理服务器
pub fn default_server() -> ProxyServer {
    ProxyServer::default()
}

/// 便捷函数：创建指定端口的代理服务器
pub fn server_with_port(port: u16) -> ProxyServer {
    ProxyServer::with_listen_port(port)
}

/// 便捷函数：创建自定义配置的代理服务器
pub fn server_with_config<F>(config_fn: F) -> ProxyServer
where
    F: FnOnce(ProxyConfig) -> ProxyConfig,
{
    let config = config_fn(default_config());
    ProxyServer::new(config)
}

/// 快速启动代理服务器
///
/// # 参数
///
/// * `listen_port` - 监听端口
/// * `default_backend_port` - 默认后端端口
/// * `backend_host` - 后端主机地址
///
/// # 示例
///
/// ```rust,ignore
/// use rcoder_proxy::quick_start;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     quick_start(8080, 3000, "127.0.0.1").await?;
///     Ok(())
/// }
/// ```
pub async fn quick_start(
    listen_port: u16,
    default_backend_port: u16,
    backend_host: &str,
) -> anyhow::Result<()> {
    let config = ProxyConfig {
        listen_port,
        default_backend_port,
        backend_host: backend_host.to_string(),
        port_param: DEFAULT_PORT_PARAM.to_string(),
        config_file: None,
        verbose: false,
    };

    let server = ProxyServer::new(config);
    server.start().await
}

/// Proxy error type
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("config error: {0}")]
    Config(String),

    #[error("network error: {0}")]
    Network(String),

    #[error("backend error: {0}")]
    Backend(String),

    #[error("port extraction error: {0}")]
    PortExtraction(String),

    #[error("request handling error: {0}")]
    RequestHandling(String),
}

impl From<anyhow::Error> for ProxyError {
    fn from(err: anyhow::Error) -> Self {
        ProxyError::RequestHandling(err.to_string())
    }
}

/// 代理结果类型
pub type ProxyResult<T> = Result<T, ProxyError>;

#[cfg(test)]
mod integration_tests {
    use super::*;

    #[test]
    fn test_library_constants() {
        assert_eq!(DEFAULT_PORT, 8080);
        assert_eq!(DEFAULT_BACKEND_PORT, 3000);
        assert_eq!(DEFAULT_BACKEND_HOST, "127.0.0.1");
        assert_eq!(DEFAULT_PORT_PARAM, "port");
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn test_convenience_functions() {
        // 测试默认配置
        let config = default_config();
        assert_eq!(config.listen_port, 8080);

        // 测试指定端口配置
        let config = config_with_port(9090);
        assert_eq!(config.listen_port, 9090);

        // 测试默认服务器
        let server = default_server();
        assert_eq!(server.listen_port(), 8080);

        // 测试指定端口服务器
        let server = server_with_port(9090);
        assert_eq!(server.listen_port(), 9090);

        // 测试自定义配置服务器
        let server = server_with_config(|mut config| {
            config.listen_port = 8080;
            config.backend_host = "localhost".to_string();
            config
        });
        assert_eq!(server.config().backend_host, "localhost");
    }

    #[test]
    fn test_features() {
        assert!(features::QUERY_PARAM_ROUTING);
        assert!(features::PATH_ROUTING);
        assert!(features::DYNAMIC_BACKENDS);
        assert!(features::CONFIG_FILE_SUPPORT);
        assert!(features::VERBOSE_LOGGING);
    }

    #[test]
    fn test_proxy_error() {
        let error = ProxyError::Config("Invalid port".to_string());
        assert!(error.to_string().contains("配置错误"));

        let error = ProxyError::Network("Connection failed".to_string());
        assert!(error.to_string().contains("网络错误"));
    }
}
