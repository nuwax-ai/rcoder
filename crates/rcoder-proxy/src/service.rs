//! 基于 Pingora 的代理服务模块
//!
//! 提供使用 Cloudflare Pingora 库实现的高性能反向代理服务，支持负载均衡。
//!
//! ## VNC WebSocket 代理
//!
//! 支持 `/computer/vnc/{user_id}/{project_id}` 路径的 WebSocket 透明代理，
//! 将请求路由到对应用户容器的 noVNC 服务（端口 6080）。

use anyhow::{Result, anyhow};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use dashmap::DashMap;
use matchit::{Params, Router};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

// 导入 shared_types 以使用 ModelProviderConfig
use shared_types::ModelProviderConfig;

// Pingora 相关导入
use pingora_core::Result as PingoraResult;
use pingora_core::protocols::Digest;
use pingora_core::protocols::TcpKeepalive;
use pingora_core::upstreams::peer::{ALPN, HttpPeer};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_load_balancing::{LoadBalancer, health_check, selection::RoundRobin};
use pingora_proxy::{ProxyHttp, Session};

use crate::config::ProxyConfig;
use crate::router::{RouteType, create_router};
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

impl Default for PerPortMetrics {
    fn default() -> Self {
        Self::new()
    }
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

/// 对 header value 进行脱敏，保留前 4 和后 4 个字符
fn mask_header_value(value: &str) -> String {
    if value.len() <= 10 {
        return "***".to_string();
    }
    format!("{}***{}", &value[..4], &value[value.len() - 4..])
}

/// 对 URL 进行脱敏处理，隐藏域名中间部分
///
/// # 示例
/// - `https://anthropic-code-api.nuwax.com/api/...` -> `https://ant***ax.com/api/...`
/// - `https://api.openai.com/v1/chat` -> `https://api***ai.com/v1/chat`
fn mask_url(url: &str) -> String {
    // 尝试解析 URL
    if let Ok(parsed_url) = url::Url::parse(url)
        && let Some(host) = parsed_url.host_str()
    {
        let masked_host = mask_domain(host);
        // 重新构建 URL，保留协议、端口、路径等
        let scheme = parsed_url.scheme();
        let port = parsed_url
            .port()
            .map(|p| format!(":{}", p))
            .unwrap_or_default();
        let path = parsed_url.path();
        let query = parsed_url
            .query()
            .map(|q| format!("?{}", q))
            .unwrap_or_default();
        return format!("{}://{}{}{}{}", scheme, masked_host, port, path, query);
    }

    // 如果解析失败，直接返回原始 URL（不应该发生）
    url.to_string()
}

/// 对域名进行脱敏处理
///
/// # 规则
/// - 保留前 3 个字符和后 6 个字符（包括顶级域名）
/// - 中间部分用 `***` 替代
///
/// # 示例
/// - `anthropic-code-api.nuwax.com` -> `ant***ax.com`
/// - `api.openai.com` -> `api***ai.com`
/// - `localhost` -> `loc***ost` (短域名)
fn mask_domain(domain: &str) -> String {
    // 使用字符而非字节处理，避免 Unicode 边界问题
    let chars: Vec<char> = domain.chars().collect();
    let len = chars.len();

    // 如果域名太短（小于等于 6 个字符），不脱敏
    if len <= 6 {
        return domain.to_string();
    }

    // 如果域名较短（小于等于 10 个字符），保留首尾各 3 个字符
    if len <= 10 {
        let prefix: String = chars[..3].iter().collect();
        let suffix: String = chars[len - 3..].iter().collect();
        return format!("{}***{}", prefix, suffix);
    }

    // 正常情况：保留前 3 个字符和后 6 个字符
    let prefix: String = chars[..3].iter().collect();
    let suffix: String = chars[len - 6..].iter().collect();
    format!("{}***{}", prefix, suffix)
}

pub struct ProxyMetrics {
    pub total_requests: AtomicU64,
    pub total_responses: AtomicU64,
    pub successful_responses: AtomicU64,
    pub failed_responses: AtomicU64,
    pub total_response_time_ns: AtomicU64,
    // 每端口统计（使用 DashMap 避免死锁和 TOCTOU 竞态）
    port_map: DashMap<u16, Arc<PerPortMetrics>>,
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
            port_map: DashMap::new(),
            active_connections: AtomicU64::new(0),
        }
    }
}

impl ProxyMetrics {
    pub fn record_request(&self) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub async fn record_request_port(&self, port: u16) {
        let arc = self.get_or_create_port_metrics(port);
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
        let arc = self.get_or_create_port_metrics(port);
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

    /// 获取或创建端口指标
    ///
    /// 使用 DashMap entry API 实现，避免 TOCTOU 竞态条件
    fn get_or_create_port_metrics(&self, port: u16) -> Arc<PerPortMetrics> {
        self.port_map
            .entry(port)
            .or_insert_with(|| Arc::new(PerPortMetrics::new()))
            .clone()
    }

    /// 获取端口指标快照
    pub fn port_snapshots(&self) -> Vec<PortSnapshot> {
        self.port_map
            .iter()
            .map(|entry| {
                let port = *entry.key();
                let m = entry.value();
                PortSnapshot {
                    port,
                    requests: m.requests.load(Ordering::Relaxed),
                    successes: m.successes.load(Ordering::Relaxed),
                    failures: m.failures.load(Ordering::Relaxed),
                    total_response_time_ns: m.total_response_time_ns.load(Ordering::Relaxed),
                }
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
    /// VNC 目标 IP（用于 VNC WebSocket 代理）
    pub vnc_target_ip: Option<String>,
    /// 上游目标主机（用于日志）
    pub upstream_host: Option<String>,
    /// 是否使用 TLS
    pub use_tls: bool,
    /// 连接协议（HTTP/1.1 或 HTTP/2）
    pub http_version: Option<String>,
    /// 连接是否被重用
    pub connection_reused: bool,
    /// API 代理服务名称（用于错误响应体日志）
    pub api_service_name: Option<String>,
    /// 上游响应状态码（用于判断是否需要捕获错误响应体）
    pub upstream_status: Option<u16>,
    /// 错误响应体缓冲（仅在 4xx/5xx 时收集）
    pub error_body_buf: Vec<u8>,
}

impl Default for TrackingCtx {
    fn default() -> Self {
        Self::new()
    }
}

impl TrackingCtx {
    pub fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
            target_port: None,
            vnc_target_ip: None,
            upstream_host: None,
            use_tls: false,
            http_version: None,
            connection_reused: false,
            api_service_name: None,
            upstream_status: None,
            error_body_buf: Vec::new(),
        }
    }
}

/// noVNC 默认端口
pub const NOVNC_PORT: u16 = 6080;

/// 音频服务端口
pub const AUDIO_HTTP_PORT: u16 = 6090; // 音频静态文件服务
pub const AUDIO_WS_PORT: u16 = 6089; // 音频 WebSocket 流

/// IME 输入法服务端口
pub const IME_PORT: u16 = 6091;

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
    /// VNC 后端映射: user_id -> container_ip
    /// 用于 /computer/vnc/{user_id}/{project_id} 路由
    pub vnc_backends: Arc<DashMap<String, String>>,
    /// 🔒 API 密钥管理器: service_name -> ModelProviderConfig
    /// 用于 /api/{service_name}/{*path} 路由
    pub api_key_manager: Arc<DashMap<String, ModelProviderConfig>>,
    /// 🔒 API Key 鉴权配置（可选，用于 VNC 等路由的鉴权，使用 ArcSwap 实现无锁读取）
    pub api_key_config: Option<Arc<ArcSwap<shared_types::ApiKeyAuthConfig>>>,
}

/// Pingora 代理实现
pub struct PortProxy {
    backends: Arc<RwLock<HashMap<u16, String>>>,
    #[allow(dead_code)]
    default_backend_port: u16,
    backend_host: String,
    /// 负载均衡算法选择
    pub use_round_robin: bool,
    /// 指标
    pub metrics: Arc<ProxyMetrics>,
    /// VNC 后端映射: user_id -> container_ip
    vnc_backends: Arc<DashMap<String, String>>,
    /// 路由表
    router: Router<RouteType>,
    /// 🔒 API 密钥管理器: service_name -> ModelProviderConfig
    api_key_manager: Arc<DashMap<String, ModelProviderConfig>>,
    /// 🔒 API Key 鉴权配置（可选，用于 VNC 等路由的鉴权，使用 ArcSwap 实现无锁读取）
    api_key_config: Option<Arc<ArcSwap<shared_types::ApiKeyAuthConfig>>>,
}

#[async_trait]
impl ProxyHttp for PortProxy {
    type CTX = TrackingCtx;

    fn new_ctx(&self) -> Self::CTX {
        TrackingCtx::new()
    }

    /// 过滤请求头和路径
    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        // ========================================
        // 🆕 API Key 验证（在所有路由处理之前）
        // ========================================
        if let Some(ref api_key_config) = self.api_key_config {
            let path = upstream_request.uri.path();

            // 提取 x-api-key header
            let api_key = session
                .req_header()
                .headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok());

            // 🚀 验证 API Key（无锁同步验证）
            match shared_types::ApiKeyValidator::validate(api_key_config, path, api_key) {
                Ok(()) => {
                    // 验证通过，继续处理
                }
                Err(shared_types::ApiKeyAuthError::Invalid) => {
                    tracing::warn!("🔒 [PINGORA_AUTH] Invalid API key for path: {}", path);
                    return Err(Box::new(pingora_core::Error::new(
                        pingora_core::ErrorType::HTTPStatus(401),
                    ))
                    .more_context("Invalid API key".to_string()));
                }
                Err(shared_types::ApiKeyAuthError::Missing) => {
                    tracing::warn!(
                        "🔒 [PINGORA_AUTH] Missing x-api-key header for path: {}",
                        path
                    );
                    return Err(Box::new(pingora_core::Error::new(
                        pingora_core::ErrorType::HTTPStatus(401),
                    ))
                    .more_context("Missing x-api-key header".to_string()));
                }
                Err(shared_types::ApiKeyAuthError::ConfigError) => {
                    tracing::error!("🔒 [PINGORA_AUTH] Configuration error");
                    return Err(Box::new(pingora_core::Error::new(
                        pingora_core::ErrorType::HTTPStatus(500),
                    ))
                    .more_context("Internal configuration error".to_string()));
                }
            }
        }

        // ========================================
        // 原有路由处理逻辑
        // ========================================
        let original_uri = upstream_request.uri.clone();
        let path = original_uri.path();

        // 使用 matchit 匹配路由
        let matched = self.router.at(path).map_err(|_| {
            warn!("route not found: {}", path);
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
        })?;

        // 根据路由类型分发处理
        match matched.value {
            RouteType::VncProxy => {
                self.handle_vnc_request(upstream_request, &original_uri, matched.params, ctx)
                    .await?;
            }
            RouteType::PortProxy => {
                self.handle_port_proxy_request(upstream_request, &original_uri, matched.params)
                    .await?;
            }
            RouteType::HealthCheck => {
                // 健康检查：代理到 Axum 的 /health 端点
                // 这样既能验证 Pingora 正常运行，又能验证 Axum 正常运行
                info!(
                    "🏥 健康检查请求: {} - 代理到 Axum ({})",
                    path, self.default_backend_port
                );

                // 修改请求路径为 /health
                let health_uri = http::Uri::from_static("/health");
                upstream_request.set_uri(health_uri);

                // 设置目标端口为默认后端端口 (Axum)
                ctx.target_port = Some(self.default_backend_port);
            }
            RouteType::ApiProxy => {
                // 🔒 API 密钥代理：注入真实密钥后转发到真实 API
                self.handle_api_proxy_request(upstream_request, &original_uri, matched.params, ctx)
                    .await?;
            }
            RouteType::AudioProxy => {
                // 🎵 音频流代理：根据路径路由到 HTTP 或 WebSocket 端口
                self.handle_audio_request(upstream_request, &original_uri, matched.params, ctx)
                    .await?;
            }
            RouteType::ImeProxy => {
                // ⌨️ IME 输入法代理：路由到 IME WebSocket 服务
                self.handle_ime_request(upstream_request, &original_uri, matched.params, ctx)
                    .await?;
            }
        }

        Ok(())
    }

    /// 选择上游服务器
    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<Box<HttpPeer>> {
        let req_header = session.req_header();
        let path = req_header.uri.path();

        // 使用 matchit 匹配路由
        let matched = self.router.at(path).map_err(|_| {
            warn!("route not found: {}", path);
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
        })?;

        match matched.value {
            RouteType::VncProxy => self.handle_vnc_upstream(ctx, matched.params).await,
            RouteType::PortProxy => self.handle_port_proxy_upstream(ctx, matched.params).await,
            RouteType::HealthCheck => {
                // 健康检查已在 upstream_request_filter 中设置 target_port
                // 这里返回对应的后端 peer
                let target_port = ctx.target_port.unwrap_or(self.default_backend_port);

                // 记录指标
                self.metrics.record_request();
                self.metrics.inc_active();

                // 返回 Axum 服务的 peer
                let peer = Box::new(HttpPeer::new(
                    ("127.0.0.1", target_port),
                    false,
                    "".to_string(),
                ));

                Ok(peer)
            }
            RouteType::ApiProxy => {
                // 🔒 API 代理：返回真实 API 端点的 peer
                self.handle_api_proxy_upstream(ctx, matched.params).await
            }
            RouteType::AudioProxy => {
                // 🎵 音频流代理：返回音频服务的 peer
                self.handle_audio_upstream(ctx, matched.params).await
            }
            RouteType::ImeProxy => {
                // ⌨️ IME 输入法代理：返回 IME 服务的 peer
                self.handle_ime_upstream(ctx, matched.params).await
            }
        }
    }

    /// 连接到上游后的回调
    ///
    /// 用于记录连接协议信息（HTTP/1.1 或 HTTP/2）
    /// 注意: http_version 显示的是 ALPN 配置偏好，实际协商结果可在 Pingora 底层日志查看
    async fn connected_to_upstream(
        &self,
        _session: &mut Session,
        reused: bool,
        peer: &HttpPeer,
        #[cfg(unix)] _fd: std::os::unix::io::RawFd,
        #[cfg(windows)] _sock: std::os::windows::io::RawSocket,
        digest: Option<&Digest>,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        // 记录连接是否被重用
        ctx.connection_reused = reused;

        // 根据 peer 的 ALPN 配置推断协议
        let alpn_str = match peer.options.alpn {
            ALPN::H2 => "HTTP/2 (H2)",
            ALPN::H2H1 => "HTTP/2 优先 (H2H1)",
            ALPN::H1 => "HTTP/1.1 (H1)",
            ALPN::Custom(_) => "Custom ALPN",
        };
        ctx.http_version = Some(alpn_str.to_string());

        // 获取 TLS 版本信息
        let tls_info = digest
            .and_then(|d| d.ssl_digest.as_ref())
            .map(|ssl| format!("TLS {}", ssl.version))
            .unwrap_or_else(|| "无 TLS".to_string());

        // 只在 API 代理场景打印详细日志
        if ctx.upstream_host.is_some() {
            debug!(
                "🔌 [API_PROXY] 连接建立: ALPN={}, {}, 复用={}",
                alpn_str, tls_info, reused
            );
        }

        Ok(())
    }

    /// 处理上游响应
    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        let duration = ctx.start.elapsed();
        let status_code = upstream_response.status.as_u16();
        let status_text = format!("{}", upstream_response.status);

        // 记录响应指标
        self.metrics.record_response(&status_text, duration);

        // 如果是普通端口代理，记录端口指标
        if let Some(port) = ctx.target_port {
            self.metrics
                .record_response_port(port, &status_text, duration)
                .await;
        }

        // 减少活跃连接计数
        self.metrics.dec_active();

        // 记录上游状态码（用于 body filter 判断是否捕获错误响应体）
        ctx.upstream_status = Some(status_code);

        // 日志记录
        if ctx.vnc_target_ip.is_some() {
            debug!(
                "VNC 响应: {} (耗时: {:?})",
                upstream_response.status, duration
            );
        } else if let Some(upstream_host) = &ctx.upstream_host {
            // 🔗 API 代理响应: 打印协议版本
            let http_ver = ctx.http_version.as_deref().unwrap_or("unknown");
            let reused = if ctx.connection_reused { "是" } else { "否" };

            if status_code >= 400 {
                // ⚠️ 错误响应：提升到 WARN 级别，打印响应 headers
                warn!(
                    "❌ [API_PROXY] 上游错误响应: {} -> {} (协议: {}, TLS: {}, 复用: {}, 耗时: {:?})",
                    upstream_host,
                    upstream_response.status,
                    http_ver,
                    ctx.use_tls,
                    reused,
                    duration
                );
                // 打印上游响应 headers
                for (name, value) in upstream_response.headers.iter() {
                    let val_str = value.to_str().unwrap_or("<binary>");
                    debug!("[API_PROXY_DEBUG] response Header: {} = {}", name, val_str);
                }
            } else {
                info!(
                    "📡 [API_PROXY] 上游响应: {} -> {} (协议: {}, TLS: {}, 复用: {}, 耗时: {:?})",
                    upstream_host,
                    upstream_response.status,
                    http_ver,
                    ctx.use_tls,
                    reused,
                    duration
                );
            }
        } else {
            debug!("upstream response: {}", upstream_response.status);
        }

        Ok(())
    }

    /// 处理上游响应体 — 捕获 4xx/5xx 错误响应体用于调试
    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<bytes::Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<Option<Duration>> {
        // 仅在 API 代理场景且上游返回 4xx/5xx 时捕获
        if let (Some(status), Some(service_name)) =
            (ctx.upstream_status, ctx.api_service_name.as_ref())
        {
            if status >= 400 {
                // 限制捕获大小（最多 4KB，避免 OOM）
                const MAX_ERROR_BODY: usize = 4096;
                if let Some(b) = body.as_ref() {
                    let remaining = MAX_ERROR_BODY.saturating_sub(ctx.error_body_buf.len());
                    if remaining > 0 {
                        let to_copy = b.len().min(remaining);
                        ctx.error_body_buf.extend_from_slice(&b[..to_copy]);
                    }
                }

                if end_of_stream && !ctx.error_body_buf.is_empty() {
                    let body_str = String::from_utf8_lossy(&ctx.error_body_buf);
                    warn!(
                        "❌ [API_PROXY] 上游错误响应体 (service={}, status={}): {}",
                        service_name, status, body_str
                    );
                }
            }
        }

        // 不修改响应体，透传给客户端
        Ok(None)
    }
}

impl PortProxy {
    /// 统一的 URI 重写方法，消除重复代码
    fn rewrite_uri(original_uri: &http::Uri, target_path: String) -> PingoraResult<http::Uri> {
        let new_uri_str = if let Some(query) = original_uri.query() {
            format!("{}?{}", target_path, query)
        } else {
            target_path
        };

        new_uri_str.parse().map_err(|e| {
            error!("URI rewrite failed: {} - {}", new_uri_str, e);
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })
    }

    /// 设置通用请求头
    fn set_common_headers(upstream_request: &mut RequestHeader) -> PingoraResult<()> {
        upstream_request.insert_header("X-Forwarded-Proto", "http")?;
        Ok(())
    }

    /// 处理 VNC WebSocket 代理请求
    async fn handle_vnc_request(
        &self,
        upstream_request: &mut RequestHeader,
        original_uri: &http::Uri,
        params: Params<'_, '_>,
        ctx: &TrackingCtx,
    ) -> PingoraResult<()> {
        // 从路径参数中提取 user_id 和 project_id
        let user_id = params.get("user_id").ok_or_else(|| {
            error!("VNC route missing user_id param");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        let project_id = params.get("project_id").ok_or_else(|| {
            error!("VNC route missing project_id param");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        // 提取剩余路径（通配符部分）
        let remaining_path = params.get("path").unwrap_or("");
        let target_path = if remaining_path.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", remaining_path)
        };

        debug!(
            "VNC 请求: user_id={}, project_id={}, target_path={}",
            user_id, project_id, target_path
        );

        // 设置 Host 头
        let host = ctx.vnc_target_ip.as_deref().unwrap_or("127.0.0.1");
        upstream_request.insert_header("Host", host)?;

        // 重写 URI
        let new_uri = Self::rewrite_uri(original_uri, target_path)?;
        upstream_request.set_uri(new_uri);

        // 设置代理标识头
        Self::set_common_headers(upstream_request)?;
        upstream_request.insert_header("X-VNC-Proxy", "pingora")?;
        upstream_request.insert_header("X-VNC-User-Id", user_id)?;
        upstream_request.insert_header("X-VNC-Project-Id", project_id)?;

        Ok(())
    }

    /// 处理端口代理请求
    async fn handle_port_proxy_request(
        &self,
        upstream_request: &mut RequestHeader,
        original_uri: &http::Uri,
        params: Params<'_, '_>,
    ) -> PingoraResult<()> {
        // 从路径参数中提取端口
        let port_str = params.get("port").ok_or_else(|| {
            error!("port proxy route missing port params");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        let port: u16 = port_str.parse().map_err(|_| {
            error!(" parse port failed: {}", port_str);
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        // 提取剩余路径
        let remaining_path = params.get("path").unwrap_or("");
        let target_path = if remaining_path.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", remaining_path)
        };

        debug!(
            "portproxyrequest: port={}, target_path={}",
            port, target_path
        );

        // 设置 Host 头
        upstream_request.insert_header("Host", "127.0.0.1")?;

        // 重写 URI
        let new_uri = Self::rewrite_uri(original_uri, target_path)?;
        upstream_request.set_uri(new_uri);

        // 设置代理标识头
        Self::set_common_headers(upstream_request)?;
        upstream_request.insert_header("X-Port-Proxy", "pingora-proxy")?;
        upstream_request.insert_header("X-Target-Port", port.to_string())?;
        upstream_request.insert_header(
            "X-Load-Balancer",
            if self.use_round_robin {
                "round-robin"
            } else {
                "ketama"
            },
        )?;

        Ok(())
    }

    // ========================================================================
    // 🎵 音频流代理方法
    // ========================================================================

    /// 根据路径判断音频服务端口和目标路径
    ///
    /// # 端口判断逻辑
    /// - `path == "ws"` 或 `path.starts_with("ws/")` → WebSocket 端口 6089
    /// - 其他(包括空路径) → HTTP 端口 6090
    ///
    /// # 返回
    /// (目标端口, 标准化后的目标路径)
    fn determine_audio_port_and_path(path: &str) -> (u16, String) {
        if path == "ws" || path.starts_with("ws/") {
            (AUDIO_WS_PORT, format!("/{}", path))
        } else {
            let normalized = if path.is_empty() { "/" } else { path };
            (
                AUDIO_HTTP_PORT,
                format!("/{}", normalized.trim_start_matches('/')),
            )
        }
    }

    /// 处理音频流代理请求
    ///
    /// 路径格式: /computer/audio/{user_id}/{project_id}/{*path}
    ///
    /// 端口判断逻辑：
    /// - `path == "ws"` 或 `path.starts_with("ws/")` → WebSocket 端口 6089
    /// - 其他(包括空路径) → HTTP 端口 6090
    async fn handle_audio_request(
        &self,
        upstream_request: &mut RequestHeader,
        original_uri: &http::Uri,
        params: Params<'_, '_>,
        ctx: &mut TrackingCtx,
    ) -> PingoraResult<()> {
        // 提取参数
        let user_id = params.get("user_id").ok_or_else(|| {
            error!(" missing route user_id params");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        let project_id = params.get("project_id").ok_or_else(|| {
            error!(" missing route project_id params");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        // 标准化路径
        let remaining_path = match params.get("path") {
            Some(p) if !p.is_empty() => p,
            _ => "",
        };

        // 判断目标端口和路径（使用辅助函数）
        let (target_port, target_path) = Self::determine_audio_port_and_path(remaining_path);

        // 获取容器 IP (复用 VNC 的容器 IP 解析机制)
        let container_ip = self
            .vnc_backends
            .get(user_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| {
                warn!("[AUDIO] container not found: user_id={}", user_id);
                pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
                    .more_context(format!("audio backend for user {} not found, please create container first", user_id))
            })?;

        // 记录上下文
        ctx.target_port = Some(target_port);
        ctx.upstream_host = Some(format!("{}:{}", container_ip, target_port));

        info!(
            "🎵 [AUDIO] 音频代理: user_id={}, project_id={}, path={}, target={}:{}",
            user_id, project_id, remaining_path, container_ip, target_port
        );

        // 设置 Host 头
        upstream_request.insert_header("Host", &container_ip)?;

        // 重写 URI
        let new_uri = Self::rewrite_uri(original_uri, target_path)?;
        upstream_request.set_uri(new_uri);

        // 设置代理标识头
        Self::set_common_headers(upstream_request)?;
        upstream_request.insert_header("X-Audio-Proxy", "pingora")?;
        upstream_request.insert_header("X-Audio-User-Id", user_id)?;
        upstream_request.insert_header("X-Audio-Project-Id", project_id)?;

        Ok(())
    }

    // ========================================================================
    // ⌨️ IME 输入法代理方法
    // ========================================================================

    /// 处理 IME 输入法代理请求
    ///
    /// 路径格式: /computer/ime/{user_id}/{project_id}/{*path}
    async fn handle_ime_request(
        &self,
        upstream_request: &mut RequestHeader,
        original_uri: &http::Uri,
        params: Params<'_, '_>,
        ctx: &mut TrackingCtx,
    ) -> PingoraResult<()> {
        let user_id = params.get("user_id").ok_or_else(|| {
            error!("IME route missing user_id param");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        let project_id = params.get("project_id").ok_or_else(|| {
            error!("IME route missing project_id param");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        let remaining_path = params.get("path").unwrap_or("");
        let target_path = format!("/{}", remaining_path.trim_start_matches('/'));

        // 获取容器 IP
        let container_ip = self
            .vnc_backends
            .get(user_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| {
                warn!("[IME] container not found: user_id={}", user_id);
                pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
                    .more_context(format!("IME backend for user {} not found, please create container first", user_id))
            })?;

        ctx.target_port = Some(IME_PORT);
        ctx.upstream_host = Some(format!("{}:{}", container_ip, IME_PORT));

        info!(
            "⌨️ [IME] 输入法代理: user_id={}, project_id={}, path={}, target={}:{}",
            user_id, project_id, remaining_path, container_ip, IME_PORT
        );

        upstream_request.insert_header("Host", &container_ip)?;

        let new_uri = Self::rewrite_uri(original_uri, target_path)?;
        upstream_request.set_uri(new_uri);

        Self::set_common_headers(upstream_request)?;
        upstream_request.insert_header("X-IME-Proxy", "pingora")?;
        upstream_request.insert_header("X-IME-User-Id", user_id)?;
        upstream_request.insert_header("X-IME-Project-Id", project_id)?;

        Ok(())
    }

    // ========================================================================
    // 🔒 API 密钥代理方法
    // ========================================================================

    /// 处理 API 密钥代理请求
    ///
    /// 路径格式: /api/{service_name}/{*path}
    /// 例如: /api/anthropic/v1/messages
    ///
    /// 安全机制：
    /// 1. 从 ApiKeyManager 读取真实 API 密钥配置
    /// 2. 移除客户端传入的占位密钥
    /// 3. 注入真实 API 密钥到请求头
    /// 4. 重写 URI 到真实 API 端点
    async fn handle_api_proxy_request(
        &self,
        upstream_request: &mut RequestHeader,
        original_uri: &http::Uri,
        params: Params<'_, '_>,
        ctx: &mut TrackingCtx,
    ) -> PingoraResult<()> {
        // 1. 提取服务名称（如 "anthropic", "openai"）
        let service_name = params.get("service_name").ok_or_else(|| {
            error!("API proxy route missing service_name param");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        // 2. 提取 API 路径（如 "v1/messages"）
        let api_path = params.get("path").unwrap_or("");

        // 记录服务名到 ctx（用于错误响应体日志）
        ctx.api_service_name = Some(service_name.to_string());

        debug!(
            "🔒 API 代理请求: service_name={}, api_path={}",
            service_name, api_path
        );

        // 🔍 [DEBUG] 打印原始请求的所有 headers
        {
            let method = upstream_request.method.as_str();
            let uri = original_uri.to_string();
            debug!(
                "🔍 [API_PROXY_DEBUG] ====== 原始请求 ======\n  Method: {}\n  URI: {}",
                method, uri
            );
            for (name, value) in upstream_request.headers.iter() {
                let val_str = value.to_str().unwrap_or("<binary>");
                // 对敏感 header 做脱敏
                if name.as_str().eq_ignore_ascii_case("x-api-key")
                    || name.as_str().eq_ignore_ascii_case("authorization")
                {
                    let masked = mask_header_value(val_str);
                    debug!("[API_PROXY_DEBUG]   Header: {} = {}", name, masked);
                } else {
                    debug!("[API_PROXY_DEBUG]   Header: {} = {}", name, val_str);
                }
            }
        }

        // 3. 从 ApiKeyManager 查询 API 密钥配置
        let api_config = self.api_key_manager.get(service_name).ok_or_else(|| {
            warn!(
                "🔑 [API_PROXY] 找不到服务 '{}' 的 API 密钥配置",
                service_name
            );
            // 打印所有可用的 key 用于调试
            let available_keys: Vec<_> = self
                .api_key_manager
                .iter()
                .map(|r| r.key().clone())
                .collect();
            warn!("🔑 [API_PROXY] available keys: {:?}", available_keys);
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404)).more_context(
                format!(
                    "找不到服务 {} 的 API 密钥配置，请确保已正确配置",
                    service_name
                ),
            )
        })?;

        let config = api_config.value();
        let base_url = config.base_url.trim_end_matches('/');

        // 🔍 [DEBUG] 打印完整的 ModelProviderConfig（脱敏）
        debug!(
            "🔍 [API_PROXY_DEBUG] ====== DashMap 配置 (service={}) ======\n  base_url: {}\n  api_protocol: {:?}\n  requires_openai_auth: {}\n  api_key: {}",
            service_name,
            base_url, // 不脱敏，debug 模式下需要完整 URL 排查
            config.api_protocol,
            config.requires_openai_auth,
            mask_header_value(&config.api_key),
        );

        // 4. 移除客户端传入的占位密钥（安全措施）
        upstream_request.remove_header("x-api-key");
        upstream_request.remove_header("authorization");
        upstream_request.remove_header("x-api-version"); // 移除可能的版本标识

        // 5. 注入真实 API 密钥
        // Anthropic 协议使用 x-api-key，OpenAI 协议使用 Authorization: Bearer
        // 🔧 优先根据 api_protocol 判断，而不是 requires_openai_auth

        // 判断使用哪种认证格式
        let use_anthropic_auth = config
            .api_protocol
            .as_ref()
            .map(|p| {
                let protocol = p.to_lowercase();
                protocol != "openai" // 不是 openai 就用 Anthropic 格式
            })
            .unwrap_or(!config.requires_openai_auth);

        if use_anthropic_auth {
            upstream_request.insert_header("x-api-key", &config.api_key)?;
            info!(
                "🔑 [API_PROXY] 已注入 Anthropic 格式 x-api-key: {} (api_protocol={:?})",
                mask_header_value(&config.api_key),
                config.api_protocol
            );
        } else {
            upstream_request
                .insert_header("authorization", format!("Bearer {}", config.api_key))?;
            info!(
                "🔑 [API_PROXY] 已注入 OpenAI 格式 Bearer: {} (api_protocol={:?})",
                mask_header_value(&config.api_key),
                config.api_protocol
            );
        }

        // 6. 重写 URI 到真实 API 端点
        let new_uri_str = if api_path.is_empty() {
            format!("{}/", base_url)
        } else {
            format!("{}/{}", base_url, api_path)
        };

        // 保留查询参数
        let new_uri_str = if let Some(query) = original_uri.query() {
            format!("{}?{}", new_uri_str, query)
        } else {
            new_uri_str
        };

        // 🔍 [DEBUG] 打印完整的上游 URL（不脱敏）
        debug!("🔍 [API_PROXY_DEBUG] proxy URL: {}", new_uri_str);

        let new_uri = new_uri_str.parse::<http::Uri>().map_err(|e| {
            error!("URI rewrite failed: {} - {}", new_uri_str, e);
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        upstream_request.set_uri(new_uri);

        // 8. 设置 Host 头（从 base_url 提取）
        if let Some(host) = base_url
            .strip_prefix("https://")
            .or_else(|| base_url.strip_prefix("http://"))
            .and_then(|s: &str| s.split('/').next())
        {
            upstream_request.insert_header("Host", host)?;
            debug!("🔑 Host header already set: {}", host);
        }

        // 9. 设置通用代理头
        Self::set_common_headers(upstream_request)?;
        upstream_request.insert_header("X-API-Proxy", "pingora-proxy")?;
        upstream_request.insert_header("X-Service-Name", service_name)?;

        // 对 URL 进行脱敏处理后输出日志
        let masked_url = mask_url(base_url);
        info!(
            "✅ [API_PROXY] {} 请求已重写到: {}",
            service_name, masked_url
        );

        // 🔍 [DEBUG] 打印最终发送到上游的所有 headers
        {
            debug!("[API_PROXY_DEBUG] ====== request Headers ======");
            for (name, value) in upstream_request.headers.iter() {
                let val_str = value.to_str().unwrap_or("<binary>");
                if name.as_str().eq_ignore_ascii_case("x-api-key")
                    || name.as_str().eq_ignore_ascii_case("authorization")
                {
                    let masked = mask_header_value(val_str);
                    debug!("[API_PROXY_DEBUG]   {} = {}", name, masked);
                } else {
                    debug!("[API_PROXY_DEBUG]   {} = {}", name, val_str);
                }
            }
            debug!("[API_PROXY_DEBUG] ====== response Headers ======");
        }

        Ok(())
    }

    /// 处理 API 密钥代理的上游选择
    ///
    /// 返回真实 API 端点的 HttpPeer
    async fn handle_api_proxy_upstream(
        &self,
        ctx: &mut TrackingCtx,
        params: Params<'_, '_>,
    ) -> PingoraResult<Box<HttpPeer>> {
        // 1. 提取服务名称
        let service_name = params.get("service_name").ok_or_else(|| {
            error!("API proxy route missing service_name param");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        // 2. 从 ApiKeyManager 查询 API 配置
        let api_config = self.api_key_manager.get(service_name).ok_or_else(|| {
            warn!("{} not in API config", service_name);
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
                .more_context(format!("API key config for service {} not found", service_name))
        })?;

        let config = api_config.value();
        let base_url = &config.base_url;

        // 3. 解析真实 API 端点的 host 和 port
        // 支持 https://api.anthropic.com 和 https://api.openai.com:443 格式
        let (host, port, use_tls) = if let Some(https_url) = base_url.strip_prefix("https://") {
            let host_part = https_url.split('/').next().unwrap_or(https_url);
            if let Some(port_str) = host_part.split(':').nth(1) {
                let port = port_str.parse::<u16>().unwrap_or(443);
                let host = host_part.split(':').next().unwrap_or(host_part);
                (host, port, true)
            } else {
                (host_part, 443, true)
            }
        } else if let Some(http_url) = base_url.strip_prefix("http://") {
            let host_part = http_url.split('/').next().unwrap_or(http_url);
            if let Some(port_str) = host_part.split(':').nth(1) {
                let port = port_str.parse::<u16>().unwrap_or(80);
                let host = host_part.split(':').next().unwrap_or(host_part);
                (host, port, false)
            } else {
                (host_part, 80, false)
            }
        } else {
            return Err(
                pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
                    .more_context(format!("invalid base_url format: {}", base_url)),
            );
        };

        // 4. 记录指标
        self.metrics.record_request();
        self.metrics.inc_active();

        // 4.1 记录上游信息到 ctx（用于 response_filter 打印协议）
        ctx.upstream_host = Some(format!("{}:{}", host, port));
        ctx.use_tls = use_tls;

        // 5. 创建真实 API 端点的 HttpPeer
        // 注意：SNI 必须设置为目标主机名，否则 TLS 握手会失败
        // 同时需要启用 HTTP/2 支持，因为很多 API 服务（如 open.bigmodel.cn）强制使用 HTTP/2
        let mut peer = HttpPeer::new(
            (host, port),
            use_tls,          // 根据协议决定是否使用 TLS
            host.to_string(), // SNI 必须设置为目标主机名
        );
        // 启用 HTTP/2 支持，优先 H2，兼容 H1
        peer.options.alpn = ALPN::H2H1;

        // 5.1 打印代理连接信息（在 ALPN 设置之后，确保日志准确性）
        let alpn_str = match peer.options.alpn {
            ALPN::H2 => "H2",
            ALPN::H2H1 => "H2H1",
            ALPN::H1 => "H1",
            ALPN::Custom(_) => "Custom",
        };
        info!(
            "🔗 [API_PROXY] {} -> {}:{} (TLS: {}, ALPN: {})",
            service_name,
            mask_domain(host),
            port,
            use_tls,
            alpn_str
        );

        // 🔧 上游连接健康检测配置
        // HTTP/2 PING 心跳: 每 30 秒发送 PING 帧检测上游连接健康
        peer.options.h2_ping_interval = Some(Duration::from_secs(30));
        // TCP Keepalive: 操作系统级别的连接保活，适用于 HTTP/1.1 后备
        peer.options.tcp_keepalive = Some(TcpKeepalive {
            idle: Duration::from_secs(60),    // 60 秒无数据后开始探测
            interval: Duration::from_secs(5), // 每 5 秒探测一次
            count: 5,                         // 5 次失败后认为断开
            #[cfg(target_os = "linux")]
            user_timeout: Duration::from_secs(85), // Linux: 数据未确认的最大时间
        });
        // 连接超时配置
        peer.options.connection_timeout = Some(Duration::from_secs(10)); // 连接建立超时
        peer.options.total_connection_timeout = Some(Duration::from_secs(30)); // 含 TLS 握手的总超时
        // read_timeout: 不设置，默认 None，适合 AI API 长时间推理
        peer.options.idle_timeout = Some(Duration::from_secs(90)); // 连接池空闲超时

        let peer = Box::new(peer);

        Ok(peer)
    }

    /// 获取后端主机地址
    async fn get_backend_host(&self, port: u16) -> PingoraResult<String> {
        let backends = self.backends.read().await;
        backends
            .get(&port)
            .cloned()
            .ok_or_else(|| anyhow!("backend service not found for port {}", port))
            .or_else(|_| Ok(self.backend_host.clone())) // 如果找不到，使用默认主机
    }

    // ========================================================================
    // VNC WebSocket 代理方法
    // ========================================================================

    /// 处理 VNC WebSocket 代理的上游选择
    ///
    /// 路径格式: /computer/vnc/{user_id}/{project_id}[/...]
    /// 例如: /computer/vnc/user_123/proj_456/websockify
    async fn handle_vnc_upstream(
        &self,
        ctx: &mut TrackingCtx,
        params: Params<'_, '_>,
    ) -> PingoraResult<Box<HttpPeer>> {
        // 从路径参数中提取 user_id
        let user_id = params.get("user_id").ok_or_else(|| {
            error!("VNC route missing user_id param");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        let project_id = params.get("project_id").unwrap_or("");

        debug!(
            "VNC 代理请求: user_id={}, project_id={}",
            user_id, project_id
        );

        // 查找用户容器 IP
        let container_ip = match self.vnc_backends.get(user_id) {
            Some(ip_ref) => ip_ref.value().clone(),
            None => {
                info!("routing {} to VNC", user_id);
                return Err(
                    pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
                        .more_context(format!("VNC backend for user {} not found, please create container first", user_id)),
                );
            }
        };

        // 记录指标
        self.metrics.record_request();
        self.metrics.inc_active();

        // 保存 VNC 目标 IP 到上下文（用于响应过滤）
        ctx.vnc_target_ip = Some(container_ip.clone());

        debug!(
            "VNC 代理: user_id={}, project_id={} -> {}:{}",
            user_id, project_id, container_ip, NOVNC_PORT
        );

        // 创建 HTTP Peer 到容器的 noVNC 端口
        // Pingora 会自动处理 WebSocket upgrade
        let peer = Box::new(HttpPeer::new(
            (container_ip.as_str(), NOVNC_PORT),
            false,          // 不使用 TLS
            "".to_string(), // SNI
        ));

        Ok(peer)
    }

    /// 处理音频流的上游连接
    async fn handle_audio_upstream(
        &self,
        _ctx: &mut TrackingCtx,
        params: Params<'_, '_>,
    ) -> PingoraResult<Box<HttpPeer>> {
        let user_id = params.get("user_id").ok_or_else(|| {
            error!(" missing route user_id params");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        let remaining_path = match params.get("path") {
            Some(p) if !p.is_empty() => p,
            _ => "",
        };

        // 判断目标端口（使用辅助函数与 request 阶段保持一致）
        let (target_port, _) = Self::determine_audio_port_and_path(remaining_path);

        let container_ip = self
            .vnc_backends
            .get(user_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| {
                warn!("[AUDIO] containernot found: user_id={}", user_id);
                pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
                    .more_context(format!("audio backend for user {} not found", user_id))
            })?;

        // 记录指标
        self.metrics.record_request();
        self.metrics.record_request_port(target_port).await;
        self.metrics.inc_active();

        let peer_addr = format!("{}:{}", container_ip, target_port);
        let mut peer = Box::new(HttpPeer::new(peer_addr.clone(), false, "".to_string()));

        // WebSocket 长连接优化配置
        peer.options.connection_timeout = Some(Duration::from_secs(10));
        peer.options.read_timeout = None; // 无限等待(音频流可能持续数小时)
        peer.options.write_timeout = Some(Duration::from_secs(30));
        peer.options.total_connection_timeout = Some(Duration::from_secs(15));
        peer.options.idle_timeout = Some(Duration::from_secs(3600)); // 1 小时空闲超时

        debug!("🎵 [AUDIO] connection from: {}", peer_addr);

        Ok(peer)
    }

    /// 处理 IME 输入法的上游连接
    async fn handle_ime_upstream(
        &self,
        _ctx: &mut TrackingCtx,
        params: Params<'_, '_>,
    ) -> PingoraResult<Box<HttpPeer>> {
        let user_id = params.get("user_id").ok_or_else(|| {
            error!("IME route missing user_id param");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        let container_ip = self
            .vnc_backends
            .get(user_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| {
                warn!("[IME] containernot found: user_id={}", user_id);
                pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
                    .more_context(format!("IME backend for user {} not found", user_id))
            })?;

        self.metrics.record_request();
        self.metrics.record_request_port(IME_PORT).await;
        self.metrics.inc_active();

        let peer_addr = format!("{}:{}", container_ip, IME_PORT);
        let mut peer = Box::new(HttpPeer::new(peer_addr.clone(), false, "".to_string()));

        // IME 长连接优化 (与音频流相同配置)
        peer.options.connection_timeout = Some(Duration::from_secs(10));
        peer.options.read_timeout = None; // 无限等待
        peer.options.write_timeout = Some(Duration::from_secs(30));
        peer.options.total_connection_timeout = Some(Duration::from_secs(15));
        peer.options.idle_timeout = Some(Duration::from_secs(3600));

        debug!("⌨️ [IME] connection IME from: {}", peer_addr);

        Ok(peer)
    }

    /// 处理端口代理的上游选择
    async fn handle_port_proxy_upstream(
        &self,
        ctx: &mut TrackingCtx,
        params: Params<'_, '_>,
    ) -> PingoraResult<Box<HttpPeer>> {
        // 从路径参数中提取端口
        let port_str = params.get("port").ok_or_else(|| {
            error!("port proxy route missing port params");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        let target_port: u16 = port_str.parse().map_err(|_| {
            error!(" parse port failed: {}", port_str);
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?;

        self.metrics.record_request();
        ctx.target_port = Some(target_port);
        self.metrics.record_request_port(target_port).await;
        self.metrics.inc_active();

        // 如果端口不在后端映射中，动态添加
        if !self.backends.read().await.contains_key(&target_port) {
            let backend_host = self.backend_host.clone();
            self.backends
                .write()
                .await
                .insert(target_port, backend_host.clone());
            debug!(" routing: {} -> {}", target_port, backend_host);
        }

        // 获取后端主机地址
        let backend_host = self.get_backend_host(target_port).await?;

        debug!("route: {}:{}", backend_host, target_port);

        // 创建 HTTP Peer
        let peer = Box::new(HttpPeer::new(
            (backend_host.as_str(), target_port),
            false,          // 不使用 TLS
            "".to_string(), // SNI
        ));

        Ok(peer)
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
            vnc_backends: Arc::new(DashMap::new()),
            api_key_manager: Arc::new(DashMap::new()),
            api_key_config: None, // 默认不启用 API Key 鉴权
        }
    }

    /// 设置负载均衡算法
    pub fn with_load_balancing(mut self, use_round_robin: bool) -> Self {
        self.use_round_robin = use_round_robin;
        self
    }

    /// 设置共享的 API 密钥管理器
    ///
    /// 这个方法允许从外部传入一个共享的 DashMap，使 agent_runner 和 Pingora
    /// 能够共享 API 密钥配置。
    pub fn with_api_key_manager(
        mut self,
        api_key_manager: Arc<DashMap<String, ModelProviderConfig>>,
    ) -> Self {
        self.api_key_manager = api_key_manager;
        self
    }

    /// 设置 API Key 鉴权配置（builder 模式）
    ///
    /// 传入共享的 API Key 配置，使 Pingora 层也能进行 API Key 验证。
    /// 配置将被传递给 PortProxy，用于在 upstream_request_filter 中验证请求。
    /// 使用 ArcSwap 实现无锁读取，提升并发性能。
    pub fn with_api_key_config(
        mut self,
        config: Arc<ArcSwap<shared_types::ApiKeyAuthConfig>>,
    ) -> Self {
        self.api_key_config = Some(config);
        self
    }

    /// 创建 Pingora 代理服务实例
    pub fn create_pingora_proxy(&self) -> anyhow::Result<PortProxy> {
        // 使用统一的路由配置
        let router = create_router().map_err(|e| {
            tracing::error!("[PROXY] create route failed: {}", e);
            e
        })?;

        Ok(PortProxy {
            backends: self.backends.clone(),
            default_backend_port: self.config.default_backend_port,
            backend_host: self.config.backend_host.clone(),
            use_round_robin: self.use_round_robin,
            metrics: self.metrics.clone(),
            vnc_backends: self.vnc_backends.clone(),
            router,
            api_key_manager: self.api_key_manager.clone(),
            api_key_config: self.api_key_config.clone(), // 传递 API Key 配置
        })
    }

    /// 添加或更新后端服务
    pub async fn add_backend(&self, port: u16, host: String) {
        let mut backends = self.backends.write().await;
        backends.insert(port, host.clone());
        info!(" proxy route: {} -> {}", port, host);
    }

    /// 移除后端服务
    pub async fn remove_backend(&self, port: u16) {
        let mut backends = self.backends.write().await;
        if backends.remove(&port).is_some() {
            info!("removed route: {}", port);
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
    #[allow(dead_code)]
    pub fn extract_target_port(&self, req: &axum::extract::Request) -> Result<u16> {
        // 1. 首先尝试从 Path 中提取端口 (例如 /proxy/8080/path)
        let path = req.uri().path();
        if path.starts_with("/proxy/") {
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() >= 3
                && let Ok(port) = parts[2].parse::<u16>()
            {
                debug!("proxy path for port: {}", port);
                return Ok(port);
            }
        }

        // 2. 然后尝试从 URL 查询参数中获取端口 (向后兼容)
        if let Some(query) = req.uri().query() {
            for param in query.split('&') {
                if let Some((key, value)) = param.split_once('=')
                    && key == self.config.port_param
                    && let Ok(port) = value.parse::<u16>()
                {
                    debug!("URL params for port: {}", port);
                    return Ok(port);
                }
            }
        }

        // 3. 使用默认端口
        debug!("default port: {}", self.config.default_backend_port);
        Ok(self.config.default_backend_port)
    }

    /// 获取目标后端地址
    pub async fn get_target_backend(&self, port: u16) -> Result<String> {
        let backends = self.backends.read().await;
        backends
            .get(&port)
            .cloned()
            .ok_or_else(|| anyhow!("backend service not found for port {}", port))
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
        Err(anyhow!(
            "此方法仅用于兼容性。实际的代理功能由 Pingora 服务器处理，请直接请求 Pingora 监听的端口"
        ))
    }

    /// 更新一次所有后端的健康状态
    ///
    /// # 并发安全性
    /// - 先克隆 backends 并快速释放锁
    /// - 不持有锁进行网络 I/O（避免死锁）
    /// - 批量更新 health_map（只获取一次锁）
    pub async fn update_health_once(&self, timeout_ms: u64) {
        // 1. 快速克隆 backends 并释放锁（避免持有锁期间 await）
        let backends_snapshot = {
            let backends = self.backends.read().await;
            backends.clone()
        }; // 锁在此处释放

        // 2. 不持有任何锁进行网络 I/O
        let mut health_updates = Vec::with_capacity(backends_snapshot.len());
        for (port, host) in backends_snapshot.into_iter() {
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
            health_updates.push((port, status));
        }

        // 3. 批量更新 health_map（只获取一次写锁）
        let mut health_map = self.health_map.write().await;
        for (port, status) in health_updates {
            health_map.insert(
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
    }

    // ========================================================================
    // VNC 后端管理方法
    // ========================================================================

    /// 添加 VNC 后端映射
    ///
    /// 当创建 ComputerAgentRunner 容器时调用，注册 user_id 到 container_ip 的映射
    pub fn add_vnc_backend(&self, user_id: &str, container_ip: &str) {
        self.vnc_backends
            .insert(user_id.to_string(), container_ip.to_string());
        info!(
            "添加 VNC 后端: user_id={} -> container_ip={}",
            user_id, container_ip
        );
    }

    /// 移除 VNC 后端映射
    ///
    /// 当销毁 ComputerAgentRunner 容器时调用
    pub fn remove_vnc_backend(&self, user_id: &str) -> Option<String> {
        let removed = self.vnc_backends.remove(user_id);
        if let Some((_, ip)) = &removed {
            info!("removed VNC route: user_id={} (was: {})", user_id, ip);
        }
        removed.map(|(_, ip)| ip)
    }

    /// 获取 VNC 后端 IP
    pub fn get_vnc_backend(&self, user_id: &str) -> Option<String> {
        self.vnc_backends.get(user_id).map(|r| r.value().clone())
    }

    /// 检查 VNC 后端是否存在
    pub fn has_vnc_backend(&self, user_id: &str) -> bool {
        self.vnc_backends.contains_key(user_id)
    }

    /// 获取所有 VNC 后端映射
    pub fn list_vnc_backends(&self) -> HashMap<String, String> {
        self.vnc_backends
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect()
    }

    /// 获取 VNC 后端数量
    pub fn vnc_backend_count(&self) -> usize {
        self.vnc_backends.len()
    }

    // ========================================================================
    // 🔒 API 密钥管理方法
    // ========================================================================

    /// 设置 API 密钥管理器（用于共享 DashMap）
    ///
    /// 这个方法允许从外部传入一个共享的 DashMap，使 agent_runner 和 Pingora
    /// 能够共享 API 密钥配置。
    pub fn set_api_key_manager(&self, api_key_manager: Arc<DashMap<String, ModelProviderConfig>>) {
        // 由于 DashMap 使用 Arc，我们可以通过修改内部实现来替换
        // 注意：这里需要使用 unsafe 或者重新设计架构
        // 简单起见，我们直接插入所有现有配置到新的 DashMap
        for entry in self.api_key_manager.iter() {
            let (key, value) = (entry.key().clone(), entry.value().clone());
            api_key_manager.insert(key, value);
        }
    }

    /// 获取 API 密钥管理器的引用（用于共享）
    pub fn get_api_key_manager(&self) -> Arc<DashMap<String, ModelProviderConfig>> {
        self.api_key_manager.clone()
    }
}

impl Clone for PingoraProxyService {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            backends: self.backends.clone(),
            use_round_robin: self.use_round_robin,
            metrics: self.metrics.clone(),
            health_map: self.health_map.clone(),
            vnc_backends: self.vnc_backends.clone(),
            api_key_manager: self.api_key_manager.clone(),
            api_key_config: self.api_key_config.clone(),
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

        let pingora_proxy = service.create_pingora_proxy().unwrap();
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

    // ========================================================================
    // matchit 路由测试
    // ========================================================================

    #[test]
    fn test_matchit_debug() {
        // 验证 matchit 0.8 的正确语法: {*name} 而不是 *name
        eprintln!("\n=== matchit 0.8 mismatch ===");

        let mut router: Router<&str> = Router::new();
        router
            .insert("/computer/vnc/{user_id}/{project_id}/{*path}", "VNC")
            .unwrap();

        // 测试匹配
        let path = "/computer/vnc/user_123/proj_456/vnc.html";
        match router.at(path) {
            Ok(m) => {
                eprintln!("✓ route match succeeded!");
                eprintln!("  user_id = {}", m.params.get("user_id").unwrap());
                eprintln!("  project_id = {}", m.params.get("project_id").unwrap());
                eprintln!("  path = {}", m.params.get("path").unwrap());
            }
            Err(e) => {
                panic!("路由匹配失败: {:?}", e);
            }
        }
    }

    #[test]
    fn test_router_vnc_route() {
        let mut router = Router::new();
        router
            .insert(
                "/computer/vnc/{user_id}/{project_id}/{*path}",
                RouteType::VncProxy,
            )
            .unwrap();

        // 测试完整路径
        let matched = router
            .at("/computer/vnc/user_123/proj_456/vnc.html")
            .unwrap();
        assert_eq!(*matched.value, RouteType::VncProxy);
        assert_eq!(matched.params.get("user_id"), Some("user_123"));
        assert_eq!(matched.params.get("project_id"), Some("proj_456"));
        assert_eq!(matched.params.get("path"), Some("vnc.html"));

        // 测试 WebSocket 路径
        let matched = router
            .at("/computer/vnc/user_123/proj_456/websockify")
            .unwrap();
        assert_eq!(*matched.value, RouteType::VncProxy);
        assert_eq!(matched.params.get("user_id"), Some("user_123"));
        assert_eq!(matched.params.get("project_id"), Some("proj_456"));
        assert_eq!(matched.params.get("path"), Some("websockify"));

        // 测试多级子路径
        let matched = router
            .at("/computer/vnc/user_123/proj_456/api/v1/status")
            .unwrap();
        assert_eq!(*matched.value, RouteType::VncProxy);
        assert_eq!(matched.params.get("path"), Some("api/v1/status"));
    }

    #[test]
    fn test_router_port_proxy_route() {
        let mut router = Router::new();
        router
            .insert("/proxy/{port}/{*path}", RouteType::PortProxy)
            .unwrap();

        // 测试带路径
        let matched = router.at("/proxy/8080/api/status").unwrap();
        assert_eq!(*matched.value, RouteType::PortProxy);
        assert_eq!(matched.params.get("port"), Some("8080"));
        assert_eq!(matched.params.get("path"), Some("api/status"));

        // 测试多级路径
        let matched = router.at("/proxy/9000/v1/users/123").unwrap();
        assert_eq!(*matched.value, RouteType::PortProxy);
        assert_eq!(matched.params.get("port"), Some("9000"));
        assert_eq!(matched.params.get("path"), Some("v1/users/123"));
    }

    #[test]
    fn test_router_no_match() {
        let mut router = Router::new();
        router
            .insert(
                "/computer/vnc/{user_id}/{project_id}/{*path}",
                RouteType::VncProxy,
            )
            .unwrap();
        router
            .insert("/proxy/{port}/{*path}", RouteType::PortProxy)
            .unwrap();

        // 测试不匹配的路径
        assert!(router.at("/api/v1/users").is_err());
        assert!(router.at("/unknown/path").is_err());
        assert!(router.at("/computer/desktop").is_err());
    }

    #[test]
    fn test_router_parameter_extraction() {
        let mut router = Router::new();
        router
            .insert(
                "/computer/vnc/{user_id}/{project_id}/{*path}",
                RouteType::VncProxy,
            )
            .unwrap();

        let matched = router
            .at("/computer/vnc/alice_2024/myproj_456/index.html")
            .unwrap();

        // 验证参数提取
        assert_eq!(matched.params.get("user_id"), Some("alice_2024"));
        assert_eq!(matched.params.get("project_id"), Some("myproj_456"));
        assert_eq!(matched.params.get("path"), Some("index.html"));
    }

    #[test]
    fn test_router_route_priority() {
        let mut router = Router::new();
        router
            .insert("/proxy/{port}/{*path}", RouteType::PortProxy)
            .unwrap();
        router
            .insert(
                "/computer/vnc/{user_id}/{project_id}/{*path}",
                RouteType::VncProxy,
            )
            .unwrap();

        // VNC 路由应该匹配
        let matched = router
            .at("/computer/vnc/user_123/proj_456/vnc.html")
            .unwrap();
        assert_eq!(*matched.value, RouteType::VncProxy);

        // 端口代理路由应该匹配
        let matched = router.at("/proxy/8080/api").unwrap();
        assert_eq!(*matched.value, RouteType::PortProxy);
    }

    // ========================================================================
    // VNC 后端管理测试
    // ========================================================================

    #[test]
    fn test_vnc_backend_management() {
        let config = create_test_config();
        let service = PingoraProxyService::new(config);

        // 初始状态没有 VNC 后端
        assert_eq!(service.vnc_backend_count(), 0);
        assert!(!service.has_vnc_backend("user_123"));

        // 添加 VNC 后端
        service.add_vnc_backend("user_123", "172.17.0.5");
        assert_eq!(service.vnc_backend_count(), 1);
        assert!(service.has_vnc_backend("user_123"));

        // 获取 VNC 后端
        let ip = service.get_vnc_backend("user_123");
        assert_eq!(ip, Some("172.17.0.5".to_string()));

        // 添加更多后端
        service.add_vnc_backend("user_456", "172.17.0.6");
        assert_eq!(service.vnc_backend_count(), 2);

        // 列出所有后端
        let backends = service.list_vnc_backends();
        assert_eq!(backends.len(), 2);
        assert_eq!(backends.get("user_123"), Some(&"172.17.0.5".to_string()));
        assert_eq!(backends.get("user_456"), Some(&"172.17.0.6".to_string()));

        // 更新现有后端
        service.add_vnc_backend("user_123", "172.17.0.100");
        let ip = service.get_vnc_backend("user_123");
        assert_eq!(ip, Some("172.17.0.100".to_string()));
        assert_eq!(service.vnc_backend_count(), 2); // 数量不变

        // 移除后端
        let removed = service.remove_vnc_backend("user_123");
        assert_eq!(removed, Some("172.17.0.100".to_string()));
        assert_eq!(service.vnc_backend_count(), 1);
        assert!(!service.has_vnc_backend("user_123"));

        // 移除不存在的后端
        let removed = service.remove_vnc_backend("nonexistent");
        assert!(removed.is_none());
    }

    #[test]
    fn test_vnc_backend_shared_across_clones() {
        let config = create_test_config();
        let service = PingoraProxyService::new(config);

        // 添加后端到原始服务
        service.add_vnc_backend("user_123", "172.17.0.5");

        // 克隆服务
        let cloned = service.clone();

        // 验证克隆共享相同的 vnc_backends
        assert!(cloned.has_vnc_backend("user_123"));
        assert_eq!(
            cloned.get_vnc_backend("user_123"),
            Some("172.17.0.5".to_string())
        );

        // 通过克隆添加后端，原始服务也能看到
        cloned.add_vnc_backend("user_456", "172.17.0.6");
        assert!(service.has_vnc_backend("user_456"));
    }

    // ========================================================================
    // URL 脱敏测试
    // ========================================================================

    #[test]
    fn test_mask_domain_normal() {
        // 正常长度域名：保留前 3 后 6 字符
        assert_eq!(mask_domain("anthropic-code-api.nuwax.com"), "ant***ax.com");
        assert_eq!(mask_domain("api.openai.com"), "api***ai.com");
        assert_eq!(mask_domain("open.bigmodel.cn"), "ope***del.cn");
    }

    #[test]
    fn test_mask_domain_short() {
        // 短域名（7-10 字符）：保留前 3 后 3 字符
        assert_eq!(mask_domain("localhost"), "loc***ost"); // 9 字符
        assert_eq!(mask_domain("test.com"), "tes***com"); // 8 字符
    }

    #[test]
    fn test_mask_domain_very_short() {
        // 非常短的域名（<=6 字符）：不脱敏
        assert_eq!(mask_domain("a.com"), "a.com"); // 5 字符
        assert_eq!(mask_domain("ab.com"), "ab.com"); // 6 字符
    }

    #[test]
    fn test_mask_url_https() {
        let result = mask_url("https://api.openai.com/v1/chat/completions");
        assert_eq!(result, "https://api***ai.com/v1/chat/completions");
    }

    #[test]
    fn test_mask_url_with_port() {
        let result = mask_url("https://api.example.com:8443/api/v1");
        assert_eq!(result, "https://api***le.com:8443/api/v1");
    }

    #[test]
    fn test_mask_url_with_query() {
        let result = mask_url("https://api.openai.com/v1/models?key=value");
        assert_eq!(result, "https://api***ai.com/v1/models?key=value");
    }

    #[test]
    fn test_mask_url_http() {
        let result = mask_url("http://localhost:8080/api");
        assert_eq!(result, "http://loc***ost:8080/api");
    }
}
