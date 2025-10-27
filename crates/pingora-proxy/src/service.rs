//! 基于 Pingora 的代理服务模块
//!
//! 提供使用 Cloudflare Pingora 库实现的高性能反向代理服务，支持负载均衡。

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, info};

// Pingora 相关导入
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Result as PingoraResult;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_load_balancing::{health_check, selection::RoundRobin, LoadBalancer};
use pingora_proxy::{ProxyHttp, Session};

use crate::config::ProxyConfig;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use tokio::net::TcpStream;
use tokio::time::timeout;

pub struct PerPortMetrics {
    pub requests: AtomicU64,
    pub successes: AtomicU64,
    pub failures: AtomicU64,
    pub total_response_time_ns: AtomicU64,
}

impl PerPortMetrics {
    pub fn new() -> Self {
        Self {
            requests: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            total_response_time_ns: AtomicU64::new(0),
        }
    }
}

pub struct PortSnapshot {
    pub port: u16,
    pub requests: u64,
    pub successes: u64,
    pub failures: u64,
    pub total_response_time_ns: u64,
}

pub struct ProxyMetrics {
    pub total_requests: AtomicU64,
    pub total_responses: AtomicU64,
    pub successful_responses: AtomicU64,
    pub failed_responses: AtomicU64,
    pub total_response_time_ns: AtomicU64,
    // 每端口统计
    port_map: RwLock<HashMap<u16, Arc<PerPortMetrics>>>,
    // 活跃连接数（请求进行中）
    pub active_connections: AtomicU64,
}

impl Default for ProxyMetrics {
    fn default() -> Self {
        Self {
            total_requests: AtomicU64::new(0),
            total_responses: AtomicU64::new(0),
            successful_responses: AtomicU64::new(0),
            failed_responses: AtomicU64::new(0),
            total_response_time_ns: AtomicU64::new(0),
            port_map: RwLock::new(HashMap::new()),
            active_connections: AtomicU64::new(0),
        }
    }
}

impl ProxyMetrics {
    pub fn record_request(&self) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub async fn record_request_port(&self, port: u16) {
        let arc = self.get_or_create_port_metrics(port).await;
        arc.requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_response(&self, status_text: &str, duration: std::time::Duration) {
        self.total_responses.fetch_add(1, Ordering::Relaxed);
        self.total_response_time_ns
            .fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
        // 粗略判断成功：2xx
        let is_success = status_text.starts_with('2');
        if is_success {
            self.successful_responses.fetch_add(1, Ordering::Relaxed);
        } else {
            self.failed_responses.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub async fn record_response_port(
        &self,
        port: u16,
        status_text: &str,
        duration: std::time::Duration,
    ) {
        let arc = self.get_or_create_port_metrics(port).await;
        arc.total_response_time_ns
            .fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
        let is_success = status_text.starts_with('2');
        if is_success {
            arc.successes.fetch_add(1, Ordering::Relaxed);
        } else {
            arc.failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn avg_response_time_ms(&self) -> f64 {
        let total_resp = self.total_responses.load(Ordering::Relaxed);
        if total_resp == 0 {
            0.0
        } else {
            let ns = self.total_response_time_ns.load(Ordering::Relaxed);
            (ns as f64) / 1_000_000.0 / (total_resp as f64)
        }
    }

    pub fn inc_active(&self) {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
    }
    pub fn dec_active(&self) {
        // 饱和减
        let mut current = self.active_connections.load(Ordering::Relaxed);
        while current > 0 {
            let res = self.active_connections.compare_exchange(
                current,
                current - 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
            match res {
                Ok(_) => break,
                Err(new_cur) => current = new_cur,
            }
        }
    }
    pub fn active(&self) -> u64 {
        self.active_connections.load(Ordering::Relaxed)
    }

    async fn get_or_create_port_metrics(&self, port: u16) -> Arc<PerPortMetrics> {
        // 快速读路径
        if let Some(existing) = self.port_map.read().await.get(&port).cloned() {
            return existing;
        }
        // 写入创建
        let mut w = self.port_map.write().await;
        let entry = w.entry(port).or_insert_with(|| Arc::new(PerPortMetrics::new()));
        entry.clone()
    }

    pub async fn port_snapshots(&self) -> Vec<PortSnapshot> {
        let r = self.port_map.read().await;
        r.iter()
            .map(|(port, m)| PortSnapshot {
                port: *port,
                requests: m.requests.load(Ordering::Relaxed),
                successes: m.successes.load(Ordering::Relaxed),
                failures: m.failures.load(Ordering::Relaxed),
                total_response_time_ns: m.total_response_time_ns.load(Ordering::Relaxed),
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum HealthState {
    Healthy,
    Unhealthy,
    Timeout,
}

impl HealthState {
    pub fn as_str(&self) -> &'static str {
        match self {
            HealthState::Healthy => "healthy",
            HealthState::Unhealthy => "unhealthy",
            HealthState::Timeout => "timeout",
        }
    }
}

#[derive(Clone, Debug)]
pub struct HealthInfo {
    pub status: HealthState,
    pub last_check: SystemTime,
}

#[derive(Clone)]
pub struct TrackingCtx {
    pub start: std::time::Instant,
    pub target_port: Option<u16>,
}

impl TrackingCtx {
    pub fn new() -> Self {
        Self { start: std::time::Instant::now(), target_port: None }
    }
}

/// 基于 Pingora 的端口反向代理服务
pub struct PingoraProxyService {
    config: ProxyConfig,
    backends: Arc<RwLock<HashMap<u16, String>>>,
    /// 负载均衡算法选择
    pub use_round_robin: bool,
    /// 指标
    pub metrics: Arc<ProxyMetrics>,
    /// 后端健康状态缓存
    pub health_map: Arc<RwLock<HashMap<u16, HealthInfo>>>,
}

/// Pingora 代理实现
pub struct PortProxy {
    backends: Arc<RwLock<HashMap<u16, String>>>,
    default_backend_port: u16,
    backend_host: String,
    /// 负载均衡算法选择
    pub use_round_robin: bool,
    /// 指标
    pub metrics: Arc<ProxyMetrics>,
}

#[async_trait]
impl ProxyHttp for PortProxy {
    type CTX = TrackingCtx;

    fn new_ctx(&self) -> Self::CTX { TrackingCtx::new() }

    /// 过滤请求头和路径
    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        // 重写 Host 头为 127.0.0.1
        upstream_request.insert_header("Host", "127.0.0.1")?;

        // 添加自定义头
        upstream_request.insert_header("X-Forwarded-Proto", "http")?;
        upstream_request.insert_header("X-Port-Proxy", "pingora-proxy")?;
        upstream_request.insert_header(
            "X-Load-Balancer",
            if self.use_round_robin {
                "round-robin"
            } else {
                "ketama"
            },
        )?;

        // 重写请求路径，移除 /proxy/{port} 前缀
        let target_path = self.extract_target_path(session);
        let original_uri = upstream_request.uri.clone();

        // 构建新的URI，保持查询参数和原始协议
        if let Some(query) = original_uri.query() {
            let new_uri_str = format!("{}?{}", target_path, query);
            let new_uri = new_uri_str
                .parse()
                .map_err(|_| pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400)))?;
            upstream_request.set_uri(new_uri);
        } else {
            let new_uri = target_path
                .parse()
                .map_err(|_| pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400)))?;
            upstream_request.set_uri(new_uri);
        }

        debug!("路径重写: {} -> {}", original_uri.path(), target_path);

        Ok(())
    }

    /// 选择上游服务器
    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<Box<HttpPeer>> {
        // 从请求中提取目标端口
        let target_port = self.extract_target_port(session)?;
        self.metrics.record_request();
        ctx.target_port = Some(target_port);
        self.metrics.record_request_port(target_port).await;
        self.metrics.inc_active();

        // 如果端口不在后端映射中，动态添加到默认主机
        if !self.backends.read().await.contains_key(&target_port) {
            let backend_host = self.backend_host.clone();
            self.backends
                .write()
                .await
                .insert(target_port, backend_host.clone());
            debug!("动态添加后端服务: {} -> {}", target_port, backend_host);
        }

        // 获取后端主机地址
        let backend_host = self.get_backend_host(target_port).await?;

        debug!("路由到后端: {}:{}", backend_host, target_port);

        // 创建 HTTP Peer
        let peer = Box::new(HttpPeer::new(
            (backend_host.as_str(), target_port),
            false,          // 不使用 TLS
            "".to_string(), // SNI
        ));

        Ok(peer)
    }

    /// 处理上游响应
    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        let duration = ctx.start.elapsed();
        let status_text = format!("{}", upstream_response.status);
        self.metrics.record_response(&status_text, duration);
        if let Some(port) = ctx.target_port {
            self.metrics
                .record_response_port(port, &status_text, duration)
                .await;
        }
        self.metrics.dec_active();
        debug!("收到上游响应: {}", upstream_response.status);
        Ok(())
    }
}

impl PortProxy {
    /// 从请求中提取目标端口
    fn extract_target_port(&self, session: &Session) -> PingoraResult<u16> {
        let req_header = session.req_header();
        let path = req_header.uri.path();

        // 1. 首先尝试从路径中提取端口 (例如 /proxy/8080/path)
        if path.starts_with("/proxy/") {
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() >= 3 {
                if let Ok(port) = parts[2].parse::<u16>() {
                    debug!("从路径中提取端口: {}", port);
                    return Ok(port);
                }
            }
        }

        // 2. 使用默认端口
        debug!("使用默认端口: {}", self.default_backend_port);
        Ok(self.default_backend_port)
    }

    /// 从请求中提取目标路径（移除 /proxy/{port} 前缀）
    fn extract_target_path(&self, session: &Session) -> String {
        let req_header = session.req_header();
        let path = req_header.uri.path();

        // 如果路径以 /proxy/{port} 开头，则提取后面的路径
        if path.starts_with("/proxy/") {
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() >= 4 {
                // 重组路径：/ + parts[4..]
                let remaining_path = parts[3..].join("/");
                return format!("/{}", remaining_path);
            } else if parts.len() == 3 {
                // 只有 /proxy/{port} 的情况，返回根路径
                return "/".to_string();
            }
        }

        // 如果不是 /proxy/ 格式，返回原路径
        path.to_string()
    }

    /// 获取后端主机地址
    async fn get_backend_host(&self, port: u16) -> PingoraResult<String> {
        let backends = self.backends.read().await;
        backends
            .get(&port)
            .cloned()
            .ok_or_else(|| anyhow!("未找到端口 {} 对应的后端服务", port))
            .or_else(|_| Ok(self.backend_host.clone())) // 如果找不到，使用默认主机
    }
}

impl PingoraProxyService {
    /// 创建新的 Pingora 代理服务
    pub fn new(config: ProxyConfig) -> Self {
        let mut backends = HashMap::new();
        // 添加默认后端
        backends.insert(config.default_backend_port, config.backend_host.clone());

        Self {
            config,
            backends: Arc::new(RwLock::new(backends)),
            use_round_robin: true, // 默认使用轮询算法
            metrics: Arc::new(ProxyMetrics::default()),
            health_map: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 设置负载均衡算法
    pub fn with_load_balancing(mut self, use_round_robin: bool) -> Self {
        self.use_round_robin = use_round_robin;
        self
    }

    /// 创建 Pingora 代理服务实例
    pub fn create_pingora_proxy(&self) -> PortProxy {
        PortProxy {
            backends: self.backends.clone(),
            default_backend_port: self.config.default_backend_port,
            backend_host: self.config.backend_host.clone(),
            use_round_robin: self.use_round_robin,
            metrics: self.metrics.clone(),
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

    /// 从请求中提取目标端口（兼容接口）
    pub fn extract_target_port(&self, req: &axum::extract::Request) -> Result<u16> {
        // 1. 首先尝试从 Path 中提取端口 (例如 /proxy/8080/path)
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

        // 2. 然后尝试从 URL 查询参数中获取端口 (向后兼容)
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

        // 3. 使用默认端口
        debug!("使用默认端口: {}", self.config.default_backend_port);
        Ok(self.config.default_backend_port)
    }

    /// 获取目标后端地址
    pub async fn get_target_backend(&self, port: u16) -> Result<String> {
        let backends = self.backends.read().await;
        backends
            .get(&port)
            .cloned()
            .ok_or_else(|| anyhow!("未找到端口 {} 对应的后端服务", port))
    }

    /// 创建负载均衡器
    pub async fn create_load_balancer(
        &self,
        backend_list: Vec<String>,
    ) -> Result<LoadBalancer<RoundRobin>> {
        let mut lb = LoadBalancer::try_from_iter(backend_list)?;

        // 添加健康检查
        let hc = health_check::TcpHealthCheck::new();
        lb.set_health_check(hc);
        lb.health_check_frequency = Some(Duration::from_secs(5));

        Ok(lb)
    }

    /// 获取配置的只读引用
    pub fn config(&self) -> &ProxyConfig {
        &self.config
    }

    /// 获取后端映射的 Arc 引用
    pub fn backends(&self) -> Arc<RwLock<HashMap<u16, String>>> {
        self.backends.clone()
    }

    /// 兼容性方法：代理请求（用于与现有接口兼容）
    ///
    /// 注意：这个方法仅用于兼容性，实际的代理功能由 Pingora 服务器处理
    pub async fn proxy_request(
        &self,
        _req: axum::extract::Request,
    ) -> Result<axum::response::Response> {
        // 这个方法提供兼容性，但实际的代理由 Pingora 服务器处理
        // 在实际部署中，请求会直接发送到 Pingora 监听的端口
        Err(anyhow!("此方法仅用于兼容性。实际的代理功能由 Pingora 服务器处理，请直接请求 Pingora 监听的端口"))
    }

    /// 更新一次所有后端的健康状态
    pub async fn update_health_once(&self, timeout_ms: u64) {
        let backends = self.backends.read().await.clone();
        for (port, host) in backends.into_iter() {
            let status = match timeout(
                std::time::Duration::from_millis(timeout_ms),
                TcpStream::connect((host.as_str(), port)),
            )
            .await
            {
                Ok(Ok(_)) => HealthState::Healthy,
                Ok(Err(_)) => HealthState::Unhealthy,
                Err(_) => HealthState::Timeout,
            };
            let mut map = self.health_map.write().await;
            map.insert(
                port,
                HealthInfo {
                    status,
                    last_check: SystemTime::now(),
                },
            );
        }
    }

    /// 启动健康检查循环
    pub fn start_health_check_loop(&self, interval_secs: u64, timeout_ms: u64) {
        let svc = self.clone();
        tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(interval_secs);
            loop {
                svc.update_health_once(timeout_ms).await;
                tokio::time::sleep(interval).await;
            }
        });
    }

    /// 获取健康状态快照
    pub async fn health_snapshot(&self) -> HashMap<u16, HealthInfo> {
        self.health_map.read().await.clone()
    }}

impl Clone for PingoraProxyService {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            backends: self.backends.clone(),
            use_round_robin: self.use_round_robin,
            metrics: self.metrics.clone(),
            health_map: self.health_map.clone(),
        }
    }
}

/// 为了兼容现有接口，我们保留原来的 PortProxyService 别名
pub type PortProxyService = PingoraProxyService;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};

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
        let service = PingoraProxyService::new(config);

        assert_eq!(service.config().listen_port, 8080);
        assert_eq!(service.config().default_backend_port, 3000);
        assert!(service.use_round_robin);
    }

    #[test]
    fn test_load_balancing_config() {
        let config = create_test_config();
        let service = PingoraProxyService::new(config).with_load_balancing(false);

        assert!(!service.use_round_robin);
    }

    #[tokio::test]
    async fn test_backend_management() {
        let config = create_test_config();
        let service = PingoraProxyService::new(config);

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
        let service = PingoraProxyService::new(create_test_config());

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
    fn test_pingora_proxy_creation() {
        let config = create_test_config();
        let service = PingoraProxyService::new(config);

        let pingora_proxy = service.create_pingora_proxy();
        assert_eq!(pingora_proxy.default_backend_port, 3000);
        assert_eq!(pingora_proxy.backend_host, "127.0.0.1");
        assert!(pingora_proxy.use_round_robin);
    }

    #[test]
    fn test_invalid_port_extraction() {
        let service = PingoraProxyService::new(create_test_config());

        // 测试无效的端口参数
        let request = Request::builder()
            .uri("/api/data?port=invalid")
            .body(Body::empty())
            .unwrap();
        let port = service.extract_target_port(&request).unwrap();
        assert_eq!(port, 3000); // 应该使用默认端口
    }

    #[test]
    fn test_service_clone() {
        let config = create_test_config();
        let service = PingoraProxyService::new(config);
        let cloned = service.clone();

        assert_eq!(service.config().listen_port, cloned.config().listen_port);
        assert_eq!(
            service.config().default_backend_port,
            cloned.config().default_backend_port
        );
        assert_eq!(service.use_round_robin, cloned.use_round_robin);
    }
}
