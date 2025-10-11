//! 代理服务模块
//!
//! 提供核心的端口代理服务功能，包括后端管理、端口提取和请求代理。

use std::sync::Arc;
use std::collections::HashMap;
use anyhow::{Result, anyhow};
use tracing::{info, error, debug};
use axum::{
    extract::Request,
    http::{StatusCode, Uri},
    response::Response,
    body::Body,
};
use reqwest::Client;

use crate::config::ProxyConfig;

/// 端口反向代理服务
pub struct PortProxyService {
    config: ProxyConfig,
    backends: Arc<RwLock<HashMap<u16, String>>>,
    client: Client,
}

use tokio::sync::RwLock;

impl PortProxyService {
    /// 创建新的端口代理服务
    pub fn new(config: ProxyConfig) -> Self {
        let mut backends = HashMap::new();
        // 添加默认后端
        backends.insert(config.default_backend_port, config.backend_host.clone());

        Self {
            config,
            backends: Arc::new(RwLock::new(backends)),
            client: Client::new(),
        }
    }

    /// 添加或更新后端服务
    pub async fn add_backend(&self, port: u16, host: String) {
        let mut backends = self.backends.write().await;
        backends.insert(port, host.clone());
        info!("添加后端服务: {} -> {}", port, host);
    }

    /// 移除后端服务
    pub async fn remove_backend(&self, port: u16) {
        let mut backends = self.backends.write().await;
        if backends.remove(&port).is_some() {
            info!("移除后端服务: {}", port);
        }
    }

    /// 获取所有后端服务列表
    pub async fn list_backends(&self) -> HashMap<u16, String> {
        let backends = self.backends.read().await;
        backends.clone()
    }

    /// 检查后端服务是否存在
    pub async fn has_backend(&self, port: u16) -> bool {
        let backends = self.backends.read().await;
        backends.contains_key(&port)
    }

    /// 获取后端服务数量
    pub async fn backend_count(&self) -> usize {
        let backends = self.backends.read().await;
        backends.len()
    }

    /// 从请求中提取目标端口
    pub fn extract_target_port(&self, req: &Request) -> Result<u16> {
        // 1. 首先尝试从 URL 查询参数中获取端口
        if let Some(query) = req.uri().query() {
            for param in query.split('&') {
                if let Some((key, value)) = param.split_once('=') {
                    if key == self.config.port_param {
                        if let Ok(port) = value.parse::<u16>() {
                            debug!("从 URL 参数中提取端口: {}", port);
                            return Ok(port);
                        }
                    }
                }
            }
        }

        // 2. 尝试从 Path 中提取端口 (例如 /proxy/8080/path)
        let path = req.uri().path();
        if path.starts_with("/proxy/") {
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() >= 3 {
                if let Ok(port) = parts[2].parse::<u16>() {
                    debug!("从路径中提取端口: {}", port);
                    return Ok(port);
                }
            }
        }

        // 3. 使用默认端口
        debug!("使用默认端口: {}", self.config.default_backend_port);
        Ok(self.config.default_backend_port)
    }

    /// 获取目标后端地址
    pub async fn get_target_backend(&self, port: u16) -> Result<String> {
        let backends = self.backends.read().await;
        backends.get(&port)
            .cloned()
            .ok_or_else(|| anyhow!("未找到端口 {} 对应的后端服务", port))
    }

    /// 构建目标 URI
    fn build_target_uri(&self, target_host: &str, target_port: u16, original_path: &str) -> Result<String> {
        // 移除 /proxy/端口号 前缀（如果存在）
        let clean_path = if original_path.starts_with(&format!("/proxy/{}/", target_port)) {
            let stripped = original_path.strip_prefix(&format!("/proxy/{}/", target_port)).unwrap_or("/");
            // 确保路径以 / 开头
            if !stripped.starts_with('/') {
                format!("/{}", stripped)
            } else {
                stripped.to_string()
            }
        } else {
            original_path.to_string()
        };

        let target_uri = format!("http://{}:{}{}", target_host, target_port, clean_path);
        debug!("构建目标 URI: {}", target_uri);
        Ok(target_uri)
    }

    /// 添加代理头信息
    fn add_proxy_headers(&self, req: &mut Request<Body>) -> Result<()> {
        req.headers_mut().insert("X-Forwarded-Proto", "http".parse().unwrap());
        req.headers_mut().insert("X-Port-Proxy", "simple-proxy".parse().unwrap());
        Ok(())
    }

    /// 处理代理请求转发
    async fn forward_request(
        &self,
        req: Request<Body>,
        target_uri: String,
        _target_host: &str,
        _target_port: u16,
    ) -> Result<Response<Body>> {
        // 转发请求
        let method = req.method().clone();
        let headers = req.headers().clone();
        let body_bytes = axum::body::to_bytes(req.into_body(), usize::MAX).await?;

        let client_req = self.client
            .request(method, &target_uri)
            .headers(headers)
            .body(body_bytes);

        match client_req.send().await {
            Ok(response) => {
                debug!("代理响应成功: {}", response.status());

                let status = response.status();
                let headers = response.headers().clone();
                let body_bytes = response.bytes().await?;

                let mut axum_response = Response::builder()
                    .status(status)
                    .body(Body::from(body_bytes))?;

                *axum_response.headers_mut() = headers;

                Ok(axum_response)
            }
            Err(e) => {
                error!("代理请求失败: {}", e);
                let error_response = Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from(format!("代理失败: {}", e)))?;
                Ok(error_response)
            }
        }
    }

    /// 代理请求
    pub async fn proxy_request(&self, mut req: Request<Body>) -> Result<Response<Body>> {
        // 提取目标端口
        let target_port = self.extract_target_port(&req)?;

        // 获取目标后端
        let target_host = self.get_target_backend(target_port).await?;

        // 构建新的 URI
        let original_path = req.uri().path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
        let target_uri = self.build_target_uri(&target_host, target_port, original_path)?;

        // 更新请求的 URI
        let new_uri: Uri = target_uri.parse()?;
        *req.uri_mut() = new_uri;

        // 添加代理头信息
        self.add_proxy_headers(&mut req)?;

        info!("代理请求: {} -> {}:{}", req.method(), target_host, target_port);

        // 转发请求
        self.forward_request(req, target_uri, &target_host, target_port).await
    }

    /// 获取配置的只读引用
    pub fn config(&self) -> &ProxyConfig {
        &self.config
    }

    /// 获取后端映射的 Arc 引用
    pub fn backends(&self) -> Arc<RwLock<HashMap<u16, String>>> {
        self.backends.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::Request,
    };

    fn create_test_config() -> ProxyConfig {
        ProxyConfig {
            listen_port: 8080,
            default_backend_port: 3000,
            backend_host: "127.0.0.1".to_string(),
            port_param: "port".to_string(),
            config_file: None,
            verbose: false,
        }
    }

    #[test]
    fn test_service_creation() {
        let config = create_test_config();
        let service = PortProxyService::new(config);

        assert_eq!(service.config().listen_port, 8080);
        assert_eq!(service.config().default_backend_port, 3000);
    }

    #[tokio::test]
    async fn test_backend_management() {
        let config = create_test_config();
        let service = PortProxyService::new(config);

        // 测试添加后端
        service.add_backend(8081, "localhost".to_string()).await;
        assert!(service.has_backend(8081).await);
        assert_eq!(service.backend_count().await, 2); // 默认3000 + 新添加的8081

        // 测试获取后端
        let backend = service.get_target_backend(8081).await.unwrap();
        assert_eq!(backend, "localhost");

        // 测试默认后端
        let default_backend = service.get_target_backend(3000).await.unwrap();
        assert_eq!(default_backend, "127.0.0.1");

        // 测试移除后端
        service.remove_backend(8081).await;
        assert!(!service.has_backend(8081).await);
        assert_eq!(service.backend_count().await, 1);
    }

    #[test]
    fn test_port_extraction() {
        let service = PortProxyService::new(create_test_config());

        // 测试从查询参数提取端口
        let request = Request::builder()
            .uri("/api/data?port=8080&other=value")
            .body(Body::empty())
            .unwrap();
        let port = service.extract_target_port(&request).unwrap();
        assert_eq!(port, 8080);

        // 测试从路径提取端口
        let request = Request::builder()
            .uri("/proxy/8080/api/data")
            .body(Body::empty())
            .unwrap();
        let port = service.extract_target_port(&request).unwrap();
        assert_eq!(port, 8080);

        // 测试默认端口
        let request = Request::builder()
            .uri("/api/data")
            .body(Body::empty())
            .unwrap();
        let port = service.extract_target_port(&request).unwrap();
        assert_eq!(port, 3000);
    }

    #[test]
    fn test_target_uri_building() {
        let service = PortProxyService::new(create_test_config());

        // 测试普通路径
        let uri = service.build_target_uri("localhost", 3000, "/api/data").unwrap();
        assert_eq!(uri, "http://localhost:3000/api/data");

        // 测试代理路径前缀
        let uri = service.build_target_uri("localhost", 8080, "/proxy/8080/api/data").unwrap();
        assert_eq!(uri, "http://localhost:8080/api/data");
    }

    #[test]
    fn test_invalid_port_extraction() {
        let service = PortProxyService::new(create_test_config());

        // 测试无效的端口参数
        let request = Request::builder()
            .uri("/api/data?port=invalid")
            .body(Body::empty())
            .unwrap();
        let port = service.extract_target_port(&request).unwrap();
        assert_eq!(port, 3000); // 应该使用默认端口
    }
}