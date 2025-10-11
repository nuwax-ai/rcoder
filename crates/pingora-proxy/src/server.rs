//! 代理服务器模块
//!
//! 提供代理服务器的启动、管理和请求处理功能。

use std::sync::Arc;
use anyhow::Result;
use tracing::{info, error};
use axum::{
    extract::Request,
    http::StatusCode,
    response::Response,
    body::Body,
    routing::any,
};

use crate::config::ProxyConfig;
use crate::service::PortProxyService;

/// 代理服务器管理器
#[derive(Clone)]
pub struct ProxyServer {
    config: ProxyConfig,
    service: Arc<PortProxyService>,
}

impl ProxyServer {
    /// 创建新的代理服务器
    pub fn new(config: ProxyConfig) -> Self {
        let service = Arc::new(PortProxyService::new(config.clone()));
        Self { config, service }
    }

    /// 启动代理服务器
    pub async fn start(self) -> Result<()> {
        // 验证配置
        self.config.validate().map_err(|e| {
            anyhow::anyhow!("配置验证失败: {}", e)
        })?;

        info!("启动端口代理服务器，监听端口: {}", self.config.listen_port);

        // 构建 Axum 应用
        let app = axum::Router::new()
            .fallback(any(Self::proxy_handler))
            .with_state(self.service.clone());

        // 启动服务器
        let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", self.config.listen_port))
            .await?;

        self.log_startup_info();

        axum::serve(listener, app).await?;
        Ok(())
    }

    /// 记录启动信息
    fn log_startup_info(&self) {
        info!("代理服务器已启动，监听端口: {}", self.config.listen_port);
        info!("配置信息:");
        info!("  默认后端端口: {}", self.config.default_backend_port);
        info!("  后端主机: {}", self.config.backend_host);
        info!("  端口参数名: {}", self.config.port_param);
        info!("代理使用方式:");
        info!("  ?port=3000 - 访问端口 3000 的服务");
        info!("  /proxy/3000/path - 访问端口 3000 的服务");
    }

    /// 代理处理器
    async fn proxy_handler(
        axum::extract::State(service): axum::extract::State<Arc<PortProxyService>>,
        req: Request<Body>,
    ) -> Result<Response<Body>, StatusCode> {
        service.proxy_request(req).await.map_err(|e| {
            error!("代理处理失败: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })
    }

    /// 获取服务实例
    pub fn service(&self) -> Arc<PortProxyService> {
        self.service.clone()
    }

    /// 获取配置的只读引用
    pub fn config(&self) -> &ProxyConfig {
        &self.config
    }

    /// 获取监听端口
    pub fn listen_port(&self) -> u16 {
        self.config.listen_port
    }

    /// 获取默认后端端口
    pub fn default_backend_port(&self) -> u16 {
        self.config.default_backend_port
    }

    /// 检查服务器是否可以启动
    pub fn can_start(&self) -> Result<(), String> {
        self.config.validate()
    }

    /// 预启动检查（不实际启动服务器）
    pub async fn pre_start_check(&self) -> Result<()> {
        // 检查配置
        self.config.validate().map_err(|e| {
            anyhow::anyhow!("配置验证失败: {}", e)
        })?;

        // 检查端口是否可用
        if let Err(e) = self.check_port_available().await {
            return Err(anyhow::anyhow!("端口检查失败: {}", e));
        }

        info!("预启动检查通过，可以启动代理服务器");
        Ok(())
    }

    /// 检查端口是否可用
    async fn check_port_available(&self) -> Result<()> {
        use tokio::net::TcpListener;

        match TcpListener::bind(format!("0.0.0.0:{}", self.config.listen_port)).await {
            Ok(_) => {
                info!("端口 {} 可用", self.config.listen_port);
                Ok(())
            }
            Err(e) => {
                Err(anyhow::anyhow!("端口 {} 不可用: {}", self.config.listen_port, e))
            }
        }
    }

    /// 创建带有自定义配置的代理服务器
    pub fn with_config(config: ProxyConfig) -> Self {
        Self::new(config)
    }

    /// 使用默认配置创建代理服务器
    pub fn default() -> Self {
        Self::new(ProxyConfig::default())
    }

    /// 使用指定监听端口创建代理服务器
    pub fn with_listen_port(port: u16) -> Self {
        Self::new(ProxyConfig::with_listen_port(port))
    }

    /// 设置后端主机
    pub fn with_backend_host(mut self, host: impl Into<String>) -> Self {
        self.config.backend_host = host.into();
        self
    }

    /// 设置端口参数名
    pub fn with_port_param(mut self, param: impl Into<String>) -> Self {
        self.config.port_param = param.into();
        self
    }

    /// 设置默认后端端口
    pub fn with_default_backend_port(mut self, port: u16) -> Self {
        self.config.default_backend_port = port;
        self
    }
}

/// 代理服务器构建器
pub struct ProxyServerBuilder {
    config: ProxyConfig,
}

impl ProxyServerBuilder {
    /// 创建新的构建器
    pub fn new() -> Self {
        Self {
            config: ProxyConfig::default(),
        }
    }

    /// 设置监听端口
    pub fn listen_port(mut self, port: u16) -> Self {
        self.config.listen_port = port;
        self
    }

    /// 设置默认后端端口
    pub fn default_backend_port(mut self, port: u16) -> Self {
        self.config.default_backend_port = port;
        self
    }

    /// 设置后端主机
    pub fn backend_host(mut self, host: impl Into<String>) -> Self {
        self.config.backend_host = host.into();
        self
    }

    /// 设置端口参数名
    pub fn port_param(mut self, param: impl Into<String>) -> Self {
        self.config.port_param = param.into();
        self
    }

    /// 设置配置文件路径
    pub fn config_file(mut self, path: impl Into<String>) -> Self {
        self.config.config_file = Some(path.into());
        self
    }

    /// 启用详细日志
    pub fn verbose(mut self, verbose: bool) -> Self {
        self.config.verbose = verbose;
        self
    }

    /// 构建代理服务器
    pub fn build(self) -> ProxyServer {
        ProxyServer::new(self.config)
    }
}

impl Default for ProxyServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProxyConfig;

    #[test]
    fn test_server_creation() {
        let config = ProxyConfig {
            listen_port: 8080,
            default_backend_port: 3000,
            backend_host: "127.0.0.1".to_string(),
            port_param: "port".to_string(),
            config_file: None,
            verbose: false,
        };

        let server = ProxyServer::new(config);

        assert_eq!(server.listen_port(), 8080);
        assert_eq!(server.default_backend_port(), 3000);
    }

    #[test]
    fn test_server_builder() {
        let server = ProxyServerBuilder::new()
            .listen_port(9090)
            .default_backend_port(3001)
            .backend_host("localhost")
            .port_param("target_port")
            .verbose(true)
            .build();

        assert_eq!(server.listen_port(), 9090);
        assert_eq!(server.default_backend_port(), 3001);
        assert_eq!(server.config().backend_host, "localhost");
        assert_eq!(server.config().port_param, "target_port");
        assert!(server.config().verbose);
    }

    #[test]
    fn test_server_convenience_methods() {
        let server = ProxyServer::with_listen_port(8080)
            .with_backend_host("example.com")
            .with_port_param("service_port")
            .with_default_backend_port(80);

        assert_eq!(server.listen_port(), 8080);
        assert_eq!(server.config().backend_host, "example.com");
        assert_eq!(server.config().port_param, "service_port");
        assert_eq!(server.config().default_backend_port, 80);
    }

    #[tokio::test]
    async fn test_pre_start_check() {
        let server = ProxyServer::default();

        // 默认配置应该通过预启动检查
        let result = server.pre_start_check().await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_config_validation() {
        // 测试有效配置
        let valid_config = ProxyConfig::default();
        let server = ProxyServer::new(valid_config);
        assert!(server.can_start().is_ok());

        // 测试无效配置（端口为0）
        let mut invalid_config = ProxyConfig::default();
        invalid_config.listen_port = 0;
        let server = ProxyServer::new(invalid_config);
        assert!(server.can_start().is_err());
    }

    #[test]
    fn test_service_access() {
        let server = ProxyServer::default();
        let service = server.service();

        // 测试可以通过服务器访问服务
        assert_eq!(service.config().default_backend_port, 3000);
        assert_eq!(service.config().listen_port, 8080);
    }
}