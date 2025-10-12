//! 基于 Pingora 的代理服务器模块
//!
//! 提供使用 Cloudflare Pingora 库的高性能代理服务器启动、管理和请求处理功能。

use anyhow::Result;
use std::sync::Arc;
use tracing::info;

use crate::config::ProxyConfig;
use crate::service::PingoraProxyService;

/// 基于 Pingora 的代理服务器管理器
#[derive(Clone)]
pub struct ProxyServer {
    config: ProxyConfig,
    service: Arc<PingoraProxyService>,
}

impl ProxyServer {
    /// 创建新的代理服务器
    pub fn new(config: ProxyConfig) -> Self {
        let service = Arc::new(PingoraProxyService::new(config.clone()));
        Self { config, service }
    }

    /// 启动代理服务器
    pub async fn start(self) -> Result<()> {
        // 验证配置
        self.config
            .validate()
            .map_err(|e| anyhow::anyhow!("配置验证失败: {}", e))?;

        info!(
            "启动基于 Pingora 的端口代理服务器，监听端口: {}",
            self.config.listen_port
        );

        self.log_startup_info();

        // 注意：这是一个库，实际的 Pingora 服务器需要由调用者启动
        // 这里只是准备服务实例
        let pingora_proxy = self.service.create_pingora_proxy();
        info!("Pingora 代理服务已准备就绪");
        info!(
            "负载均衡算法: {}",
            if pingora_proxy.use_round_robin {
                "Round Robin"
            } else {
                "Ketama Consistent"
            }
        );

        Ok(())
    }

    /// 记录启动信息
    fn log_startup_info(&self) {
        info!("基于 Pingora 的代理服务器配置:");
        info!("  监听端口: {}", self.config.listen_port);
        info!("  默认后端端口: {}", self.config.default_backend_port);
        info!("  后端主机: {}", self.config.backend_host);
        info!("  端口参数名: {}", self.config.port_param);
        info!("代理使用方式:");
        info!("  /proxy/3000/path - 访问端口 3000 的服务");
        info!("  ?port=3000 - 通过查询参数访问端口 3000 的服务（向后兼容）");
        info!("Pingora 高级特性:");
        info!("  - 负载均衡 (Round Robin/Ketama)");
        info!("  - 健康检查");
        info!("  - 连接池和连接复用");
        info!("  - HTTP/1.1 和 HTTP/2 支持");
        info!("  - 高性能异步 I/O");
    }

    /// 获取服务实例
    pub fn service(&self) -> Arc<PingoraProxyService> {
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
        self.config
            .validate()
            .map_err(|e| anyhow::anyhow!("配置验证失败: {}", e))?;

        info!("Pingora 代理服务器预启动检查通过");
        Ok(())
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

    /// 设置负载均衡算法
    pub fn with_load_balancing(mut self, use_round_robin: bool) -> Self {
        // 创建新的服务实例
        let service = Arc::new(
            self.service
                .as_ref()
                .clone()
                .with_load_balancing(use_round_robin),
        );
        self.service = service;
        self
    }
}

/// 代理服务器构建器
pub struct ProxyServerBuilder {
    config: ProxyConfig,
    use_round_robin: bool,
}

impl ProxyServerBuilder {
    /// 创建新的构建器
    pub fn new() -> Self {
        Self {
            config: ProxyConfig::default(),
            use_round_robin: true,
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

    /// 设置负载均衡算法
    pub fn load_balancing(mut self, use_round_robin: bool) -> Self {
        self.use_round_robin = use_round_robin;
        self
    }

    /// 构建代理服务器
    pub fn build(self) -> ProxyServer {
        let mut server = ProxyServer::new(self.config);
        server.service = Arc::new(
            server
                .service
                .as_ref()
                .clone()
                .with_load_balancing(self.use_round_robin),
        );
        server
    }
}

impl Default for ProxyServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Pingora 代理服务器运行器
///
/// 提供更直接的 Pingora 服务器控制方式
pub struct PingoraServerRunner {
    service: PingoraProxyService,
}

impl PingoraServerRunner {
    /// 创建新的运行器
    pub fn new(config: ProxyConfig) -> Self {
        Self {
            service: PingoraProxyService::new(config),
        }
    }

    /// 创建带负载均衡的运行器
    pub fn with_load_balancing(config: ProxyConfig, use_round_robin: bool) -> Self {
        Self {
            service: PingoraProxyService::new(config).with_load_balancing(use_round_robin),
        }
    }

    /// 获取服务引用
    pub fn service(&self) -> &PingoraProxyService {
        &self.service
    }

    /// 获取 Pingora 代理实例
    pub fn create_pingora_proxy(&self) -> crate::service::PortProxy {
        self.service.create_pingora_proxy()
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
            .load_balancing(false)
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
            .with_default_backend_port(80)
            .with_load_balancing(false);

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

    #[test]
    fn test_pingora_server_runner() {
        let config = ProxyConfig::default();
        let runner = PingoraServerRunner::new(config);

        // 测试创建运行器
        assert_eq!(runner.service().config().listen_port, 8080);
        assert_eq!(runner.service().config().default_backend_port, 3000);
    }

    #[test]
    fn test_pingora_server_runner_with_lb() {
        let config = ProxyConfig::default();
        let runner = PingoraServerRunner::with_load_balancing(config, false);

        assert!(!runner.service().use_round_robin);
    }
}
