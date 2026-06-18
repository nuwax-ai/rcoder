//! 基于 Pingora 的代理服务模块
//!
//! 提供使用 Cloudflare Pingora 库实现的高性能反向代理服务，支持负载均衡。
//!
//! ## 模块结构
//!
//! - `types` - 类型定义（PerPortMetrics, ProxyMetrics, TrackingCtx 等）
//! - `utils` - 工具函数（normalize_path, rewrite_uri, set_common_headers 等）
//! - `handlers` - 请求处理函数（VNC, ttyd, audio, IME, API proxy, port proxy）

pub mod handlers;
pub mod types;
pub mod utils;

use anyhow::Result;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use dashmap::DashMap;
use matchit::Router;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

// 导入 shared_types 以使用 ModelProviderConfig
use shared_types::ModelProviderConfig;

// Pingora 相关导入
use pingora_core::Result as PingoraResult;
use pingora_core::protocols::Digest;
use pingora_core::upstreams::peer::{ALPN, HttpPeer};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_load_balancing::{LoadBalancer, health_check, selection::RoundRobin};
use pingora_proxy::{ProxyHttp, Session};

use crate::config::ProxyConfig;
use crate::router::{RouteType, create_router};
use std::time::SystemTime;
use tokio::net::TcpStream;
use tokio::time::timeout;

// 重新导出类型，保持向后兼容
pub use types::*;
pub use utils::*;

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
    /// VNC 用户活跃时间戳：user_id → Unix 秒（AtomicI64 无锁写入）
    ///
    /// 由 response_body_filter 在每个 VNC WS chunk 触发时（10s 节流）写入。
    /// rcoder 后台任务每 30s 扫描，把"近期有 VNC 活动"的 user 续期到 storage，
    /// 防止 cleanup_task 在用户使用 VNC 桌面期间误判 idle 并销毁容器。
    pub vnc_activity: Arc<DashMap<String, AtomicI64>>,
    /// 🔒 API 密钥管理器: service_name -> ModelProviderConfig
    /// 用于 /api/{service_name}/{*path} 路由
    pub api_key_manager: Arc<DashMap<String, ModelProviderConfig>>,
    /// 🔒 API Key 鉴权配置（可选，用于 VNC 等路由的鉴权，使用 ArcSwap 实现无锁读取）
    pub api_key_config: Option<Arc<ArcSwap<shared_types::ApiKeyAuthConfig>>>,
}

/// 为了兼容现有接口，我们保留原来的 PortProxyService 别名
pub type PortProxyService = PingoraProxyService;

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
    /// VNC 用户活跃时间戳（与 PingoraProxyService.vnc_activity 共享同一 Arc）
    /// 由 response_body_filter 写入
    vnc_activity: Arc<DashMap<String, AtomicI64>>,
    /// 路由表
    router: Router<RouteType>,
    /// 🔒 API 密钥管理器: service_name -> ModelProviderConfig
    api_key_manager: Arc<DashMap<String, ModelProviderConfig>>,
    /// 🔒 API Key 鉴权配置（可选，用于 VNC 等路由的鉴权，使用 ArcSwap 实现无锁读取）
    #[allow(dead_code)]
    api_key_config: Option<Arc<ArcSwap<shared_types::ApiKeyAuthConfig>>>,
}

#[async_trait]
impl ProxyHttp for PortProxy {
    type CTX = TrackingCtx;

    fn new_ctx(&self) -> Self::CTX {
        TrackingCtx::new()
    }

    /// 请求过滤阶段：路由分发和请求处理
    async fn request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> PingoraResult<bool> {
        let req_header = session.req_header();
        info!(
            "🔍 [PINGORA] request_filter called: path={}",
            req_header.uri.path()
        );
        // 协议转换已移除，所有请求直接走正常代理流程
        Ok(false)
    }

    /// 上游请求过滤阶段
    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        info!(
            "🔍 [PINGORA] upstream_request_filter called: path={}",
            upstream_request.uri.path()
        );

        // ========================================
        // API Key 验证（在所有路由处理之前）
        // ========================================
        if let Some(ref api_key_config) = self.api_key_config {
            let path = upstream_request.uri.path();

            // 提取 x-api-key header
            let api_key = session
                .req_header()
                .headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok());

            // 验证 API Key（无锁同步验证）
            match shared_types::ApiKeyValidator::validate(api_key_config, path, api_key) {
                Ok(()) => {
                    // 验证通过，继续处理
                }
                Err(shared_types::ApiKeyAuthError::Invalid) => {
                    warn!("🔒 [PINGORA_AUTH] Invalid API key for path: {}", path);
                    return Err(
                        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(401))
                            .more_context("Invalid API key".to_string()),
                    );
                }
                Err(shared_types::ApiKeyAuthError::Missing) => {
                    warn!(
                        "🔒 [PINGORA_AUTH] Missing x-api-key header for path: {}",
                        path
                    );
                    return Err(
                        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(401))
                            .more_context("Missing x-api-key header".to_string()),
                    );
                }
                Err(shared_types::ApiKeyAuthError::ConfigError) => {
                    error!("🔒 [PINGORA_AUTH] Configuration error");
                    return Err(
                        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(500))
                            .more_context("Internal configuration error".to_string()),
                    );
                }
            }
        }

        let path = Self::normalize_path(upstream_request.uri.path()).to_string();

        // 使用 matchit 匹配路由
        let matched = self.router.at(&path).map_err(|_| {
            warn!("route not found: {}", path);
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
        })?;

        let original_uri = upstream_request.uri.clone();

        match matched.value {
            RouteType::VncProxy => {
                handlers::vnc::handle_vnc_request(
                    upstream_request,
                    &original_uri,
                    matched.params,
                    ctx,
                )
                .await?;
            }
            RouteType::PortProxy => {
                handlers::port_proxy::handle_port_proxy_request(
                    upstream_request,
                    &original_uri,
                    matched.params,
                    self.use_round_robin,
                )
                .await?;
            }
            RouteType::HealthCheck => {
                // 健康检查：代理到 Axum 的 /health 端点
                // 这样既能验证 Pingora 正常运行，又能验证 Axum 正常运行
                debug!(
                    "🏥 Health check request: {} - proxying to Axum ({})",
                    path, self.default_backend_port
                );

                // 修改请求路径为 /health
                let health_uri = http::Uri::from_static("/health");
                upstream_request.set_uri(health_uri);

                // 设置目标端口为默认后端端口 (Axum)
                ctx.target_port = Some(self.default_backend_port);
            }
            RouteType::ApiProxy => {
                handlers::api_proxy::handle_api_proxy_request(
                    upstream_request,
                    &original_uri,
                    matched.params,
                    ctx,
                    &self.api_key_manager,
                )
                .await?;
            }
            RouteType::AudioProxy => {
                handlers::audio::handle_audio_request(
                    upstream_request,
                    &original_uri,
                    matched.params,
                    ctx,
                    &self.vnc_backends,
                )
                .await?;
            }
            RouteType::ImeProxy => {
                handlers::ime::handle_ime_request(
                    upstream_request,
                    &original_uri,
                    matched.params,
                    ctx,
                    &self.vnc_backends,
                )
                .await?;
            }
            RouteType::TtydProxy => {
                handlers::ttyd::handle_ttyd_request(
                    upstream_request,
                    &original_uri,
                    matched.params,
                    ctx,
                )
                .await?;
            }
            RouteType::WebTtydProxy => {
                handlers::ttyd::handle_web_ttyd_request(
                    upstream_request,
                    &original_uri,
                    matched.params,
                )
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
        let path = Self::normalize_path(req_header.uri.path());

        info!("🔍 [PINGORA] upstream_peer called: path={}", path);

        // 使用 matchit 匹配路由
        let matched = self.router.at(path).map_err(|_| {
            warn!("route not found: {}", path);
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
        })?;

        match matched.value {
            RouteType::VncProxy => {
                handlers::vnc::handle_vnc_upstream(
                    ctx,
                    matched.params,
                    &self.vnc_backends,
                    &self.metrics,
                )
                .await
            }
            RouteType::PortProxy => {
                handlers::port_proxy::handle_port_proxy_upstream(
                    ctx,
                    matched.params,
                    &self.backends,
                    &self.backend_host,
                    &self.metrics,
                )
                .await
            }
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
                handlers::api_proxy::handle_api_proxy_upstream(
                    ctx,
                    matched.params,
                    &self.api_key_manager,
                    &self.metrics,
                )
                .await
            }
            RouteType::AudioProxy => {
                handlers::audio::handle_audio_upstream(
                    ctx,
                    matched.params,
                    &self.vnc_backends,
                    &self.metrics,
                )
                .await
            }
            RouteType::ImeProxy => {
                handlers::ime::handle_ime_upstream(
                    ctx,
                    matched.params,
                    &self.vnc_backends,
                    &self.metrics,
                )
                .await
            }
            RouteType::TtydProxy => {
                handlers::ttyd::handle_ttyd_upstream(
                    ctx,
                    matched.params,
                    &self.vnc_backends,
                    &self.metrics,
                )
                .await
            }
            RouteType::WebTtydProxy => {
                handlers::ttyd::handle_web_ttyd_upstream(ctx, matched.params, &self.metrics).await
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
            ALPN::H2H1 => "HTTP/2 preferred (H2H1)",
            ALPN::H1 => "HTTP/1.1 (H1)",
            ALPN::Custom(_) => "Custom ALPN",
        };
        ctx.http_version = Some(alpn_str.to_string());

        // 获取 TLS 版本信息
        let tls_info = digest
            .and_then(|d| d.ssl_digest.as_ref())
            .map(|ssl| format!("TLS {}", ssl.version))
            .unwrap_or_else(|| "No TLS".to_string());

        // 只在 API 代理场景打印详细日志
        if ctx.upstream_host.is_some() {
            debug!(
                "🔌 [API_PROXY] Connection established: ALPN={}, {}, reused={}",
                alpn_str, tls_info, reused
            );
        }

        Ok(())
    }

    /// 响应过滤阶段
    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()>
    where
        Self::CTX: Send,
    {
        // 记录响应状态
        let status = upstream_response.status;
        let status_text = status.to_string();
        let duration = ctx.start.elapsed();

        // 记录指标
        self.metrics.record_response(&status_text, duration);

        // 如果有目标端口，记录端口指标
        if let Some(port) = ctx.target_port {
            self.metrics
                .record_response_port(port, &status_text, duration)
                .await;
        }

        // 减少活跃连接数
        self.metrics.dec_active();

        // 只在 API 代理场景打印详细日志
        if ctx.upstream_host.is_some() {
            debug!(
                "📥 [API_PROXY] Response: status={}, duration={:?}",
                status_text, duration
            );
        }

        Ok(())
    }

    /// 上游响应体过滤
    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<bytes::Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<Option<Duration>> {
        // 如果是 4xx/5xx 响应，收集错误响应体
        if let Some(status) = ctx.upstream_status
            && status >= 400
            && let Some(body_bytes) = body
        {
            ctx.error_body_buf.extend_from_slice(body_bytes);
        }

        // 注意：VNC WS 流量跟踪**不放在这里**！
        // upstream_response_body_filter 只在收到上游响应时触发一次（每个 chunk），
        // **WebSocket upgrade 之后的 raw stream（UpgradedBody）不会触发此 hook**。
        // VNC 活跃跟踪放在 response_body_filter（下游发送前过滤），后者对 UpgradedBody 也触发。
        // 见 pingora-proxy 0.8 源码 proxy_h1.rs:723-740 和 proxy_custom.rs:618-627。

        Ok(None)
    }

    /// 下游响应体过滤（每个发送到客户端的 chunk 都触发，**包括 WS upgrade 后的 UpgradedBody**）
    ///
    /// 关键设计：本 hook 是 pingora 中唯一能在 WS upgrade 后持续被调用的 HTTP 层 hook。
    /// VNC WS 流量（屏幕刷新、鼠标位置等）经过此 hook 时（节流 10s），写入 vnc_activity[user_id]。
    /// rcoder 后台任务每 30s 扫描，对近期有 VNC 活动的 user 续期 storage 活跃时间，
    /// 防止 cleanup_task 在用户使用 VNC 桌面期间误判 idle 并销毁容器。
    fn response_body_filter(
        &self,
        _session: &mut Session,
        _body: &mut Option<bytes::Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<Option<Duration>>
    where
        Self::CTX: Send + Sync,
    {
        // 只对设置了 ctx.user_id 的路由生效（目前仅 VNC）
        if let Some(ref user_id) = ctx.user_id {
            record_vnc_activity(&self.vnc_activity, user_id);
        }
        Ok(None)
    }
}

/// VNC 活跃时间戳节流写入（10s 一次）
///
/// 独立函数便于单元测试。设计要点：
/// - 用 entry API 原子化"检查 + 写入"，避免 get+insert TOCTOU
/// - 已有条目距上次写入 < 10s 时跳过（节流，VNC 30fps 但 10s 内只写一次）
/// - value 是 AtomicI64，写入只需 entry read lock + atomic store（无写锁竞争）
pub fn record_vnc_activity(vnc_activity: &DashMap<String, AtomicI64>, user_id: &str) {
    const THROTTLE_SECS: i64 = 10;
    let now_secs = chrono::Utc::now().timestamp();

    match vnc_activity.entry(user_id.to_string()) {
        dashmap::mapref::entry::Entry::Occupied(e) => {
            // 读 last_seen 决定是否需要更新（view 风格：闭包结束锁立即释放）
            let last = e.get().load(Ordering::Relaxed);
            if now_secs - last >= THROTTLE_SECS {
                e.get().store(now_secs, Ordering::Relaxed);
            }
        }
        dashmap::mapref::entry::Entry::Vacant(e) => {
            e.insert(AtomicI64::new(now_secs));
        }
    }
}

impl PortProxy {
    /// 规范化路径（去除尾部斜杠）
    fn normalize_path(raw: &str) -> &str {
        utils::normalize_path(raw)
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
            vnc_activity: Arc::new(DashMap::new()),
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

    /// 创建 PortProxy 实例
    pub fn create_pingora_proxy(&self) -> Result<PortProxy, crate::ProxyError> {
        let router = create_router()?;

        Ok(PortProxy {
            backends: self.backends.clone(),
            default_backend_port: self.config.default_backend_port,
            backend_host: self.config.backend_host.clone(),
            use_round_robin: self.use_round_robin,
            metrics: self.metrics.clone(),
            vnc_backends: self.vnc_backends.clone(),
            vnc_activity: self.vnc_activity.clone(),
            router,
            api_key_manager: self.api_key_manager.clone(),
            api_key_config: self.api_key_config.clone(),
        })
    }

    /// 获取 VNC 活跃时间快照（user_id → Unix 秒）
    ///
    /// 供 rcoder 后台任务每 30s 扫描使用。snapshot 后立即释放 DashMap 读锁，
    /// 不跨 await 持锁。
    pub fn vnc_activity_snapshot(&self) -> Vec<(String, i64)> {
        self.vnc_activity
            .iter()
            .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
            .collect()
    }

    /// 清理过期的 VNC 活跃记录（避免 vnc_activity 无限增长）
    ///
    /// 删除 last_seen 在 `max_age_secs` 之前的条目。返回清理的条目数。
    /// 两步操作：先 iter 收集 stale keys，再 remove（避免 iter+remove 死锁）。
    pub fn evict_stale_vnc_activity(&self, max_age_secs: i64) -> usize {
        let now = chrono::Utc::now().timestamp();
        let stale: Vec<String> = self
            .vnc_activity
            .iter()
            .filter(|e| now - e.value().load(Ordering::Relaxed) > max_age_secs)
            .map(|e| e.key().clone())
            .collect();
        let count = stale.len();
        for k in &stale {
            self.vnc_activity.remove(k);
        }
        count
    }

    /// 获取后端数量
    pub async fn backend_count(&self) -> usize {
        let backends = self.backends.read().await;
        backends.len()
    }

    /// 从请求中提取目标端口（兼容接口）
    #[allow(dead_code)]
    pub fn extract_target_port(&self, req: &axum::extract::Request) -> crate::ProxyResult<u16> {
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
    pub async fn get_target_backend(&self, port: u16) -> crate::ProxyResult<String> {
        let backends = self.backends.read().await;
        backends.get(&port).cloned().ok_or_else(|| {
            crate::ProxyError::Backend(format!("backend service not found for port {}", port))
        })
    }

    /// 添加后端服务
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

    /// 列出所有后端服务
    pub async fn list_backends(&self) -> HashMap<u16, String> {
        let backends = self.backends.read().await;
        backends.clone()
    }

    /// 检查后端服务是否存在
    pub async fn has_backend(&self, port: u16) -> bool {
        let backends = self.backends.read().await;
        backends.contains_key(&port)
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
    ) -> crate::ProxyResult<axum::response::Response> {
        // 这个方法提供兼容性，但实际的代理由 Pingora 服务器处理
        // 在实际部署中，请求会直接发送到 Pingora 监听的端口
        Err(crate::ProxyError::RequestHandling(
            "This method is only for compatibility. Actual proxy functionality is handled by Pingora server, please directly request the port Pingora is listening on".to_string()
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
        let mut results = HashMap::new();
        for (port, host) in &backends_snapshot {
            let addr = format!("{}:{}", host, port);
            let state =
                match timeout(Duration::from_millis(timeout_ms), TcpStream::connect(&addr)).await {
                    Ok(Ok(_stream)) => HealthState::Healthy,
                    Ok(Err(_)) => HealthState::Unhealthy,
                    Err(_) => HealthState::Timeout,
                };
            results.insert(
                *port,
                HealthInfo {
                    status: state,
                    last_check: SystemTime::now(),
                },
            );
        }

        // 3. 批量更新 health_map（只获取一次写锁）
        let mut health_map = self.health_map.write().await;
        for (port, info) in results {
            health_map.insert(port, info);
        }
    }

    /// 获取所有后端的健康状态快照
    pub async fn get_health_snapshot(&self) -> HashMap<u16, HealthInfo> {
        let health_map = self.health_map.read().await;
        health_map.clone()
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

    /// 获取健康状态快照（兼容接口）
    pub async fn health_snapshot(&self) -> HashMap<u16, HealthInfo> {
        self.get_health_snapshot().await
    }

    // ========================================================================
    // 🔧 VNC 后端管理方法
    // ========================================================================

    /// 添加 VNC 后端映射
    ///
    /// 当创建 ComputerAgentRunner 容器时调用，注册 user_id 到 container_ip 的映射
    pub fn add_vnc_backend(&self, user_id: &str, container_ip: &str) {
        self.vnc_backends
            .insert(user_id.to_string(), container_ip.to_string());
        info!(
            "Added VNC backend: user_id={} -> container_ip={}",
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
            vnc_activity: self.vnc_activity.clone(),
            api_key_manager: self.api_key_manager.clone(),
            api_key_config: self.api_key_config.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProxyConfig;

    #[test]
    fn test_service_creation() {
        let config = ProxyConfig::default();
        let service = PingoraProxyService::new(config);
        assert!(service.use_round_robin);
    }

    // ========== VNC 活跃时间跟踪测试 ==========

    #[test]
    fn test_record_vnc_activity_first_write() {
        // 首次写入：entry 不存在 → Vacant 分支 → insert
        let map: DashMap<String, AtomicI64> = DashMap::new();
        record_vnc_activity(&map, "user_A");
        assert_eq!(map.len(), 1);
        let ts = map.get("user_A").unwrap().load(Ordering::Relaxed);
        assert!(ts > 0);
    }

    #[test]
    fn test_record_vnc_activity_throttle() {
        // 节流：10s 内重复调用不应更新时间戳
        let map: DashMap<String, AtomicI64> = DashMap::new();

        // 第一次写入
        record_vnc_activity(&map, "user_A");
        let first_ts = map.get("user_A").unwrap().load(Ordering::Relaxed);

        // 立即第二次：不应更新（节流窗口内）
        record_vnc_activity(&map, "user_A");
        let second_ts = map.get("user_A").unwrap().load(Ordering::Relaxed);

        assert_eq!(first_ts, second_ts, "10s 内重复调用不应更新");
    }

    #[test]
    fn test_record_vnc_activity_force_update_after_throttle() {
        // 模拟节流窗口外：手动设置一个旧时间戳，再调 record 应该更新
        let map: DashMap<String, AtomicI64> = DashMap::new();
        let old_ts = chrono::Utc::now().timestamp() - 100; // 100s 前
        map.insert("user_A".to_string(), AtomicI64::new(old_ts));

        record_vnc_activity(&map, "user_A");

        let new_ts = map.get("user_A").unwrap().load(Ordering::Relaxed);
        assert!(new_ts > old_ts, "节流窗口外应更新");
        assert!(new_ts > old_ts + 50, "新时间戳应明显晚于旧时间戳");
    }

    #[test]
    fn test_record_vnc_activity_concurrent_different_users() {
        // 不同 user_id 应互不影响
        let map: Arc<DashMap<String, AtomicI64>> = Arc::new(DashMap::new());

        let mut handles = vec![];
        for i in 0..10 {
            let map = map.clone();
            handles.push(std::thread::spawn(move || {
                let user_id = format!("user_{}", i);
                record_vnc_activity(&map, &user_id);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(map.len(), 10, "10 个不同 user 应有 10 个 entry");
    }

    #[test]
    fn test_record_vnc_activity_concurrent_same_user() {
        // 同一 user_id 并发调用：节流语义下应该只写一次（或少量次数）
        let map: Arc<DashMap<String, AtomicI64>> = Arc::new(DashMap::new());

        let mut handles = vec![];
        for _ in 0..20 {
            let map = map.clone();
            handles.push(std::thread::spawn(move || {
                record_vnc_activity(&map, "user_X");
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        // 所有 20 个线程在同一节流窗口内，最终只有一个时间戳
        assert_eq!(map.len(), 1);
        let ts = map.get("user_X").unwrap().load(Ordering::Relaxed);
        assert!(ts > 0);
    }

    #[test]
    fn test_vnc_activity_snapshot() {
        let config = ProxyConfig::default();
        let service = PingoraProxyService::new(config);

        // 空 snapshot
        assert!(service.vnc_activity_snapshot().is_empty());

        // 添加 3 个 user
        let now = chrono::Utc::now().timestamp();
        service
            .vnc_activity
            .insert("user_A".to_string(), AtomicI64::new(now));
        service
            .vnc_activity
            .insert("user_B".to_string(), AtomicI64::new(now - 100));
        service
            .vnc_activity
            .insert("user_C".to_string(), AtomicI64::new(now - 200));

        let snapshot = service.vnc_activity_snapshot();
        assert_eq!(snapshot.len(), 3);

        // snapshot 应该是 (user_id, ts) 对的集合
        let users: Vec<_> = snapshot.iter().map(|(u, _)| u.as_str()).collect();
        assert!(users.contains(&"user_A"));
        assert!(users.contains(&"user_B"));
        assert!(users.contains(&"user_C"));
    }

    #[test]
    fn test_evict_stale_vnc_activity() {
        let config = ProxyConfig::default();
        let service = PingoraProxyService::new(config);

        let now = chrono::Utc::now().timestamp();
        // 3 个 user：1 个近期，2 个过期
        service
            .vnc_activity
            .insert("active_user".to_string(), AtomicI64::new(now));
        service
            .vnc_activity
            .insert("stale_user_1".to_string(), AtomicI64::new(now - 200));
        service
            .vnc_activity
            .insert("stale_user_2".to_string(), AtomicI64::new(now - 300));

        // max_age = 100s：清理 100s 前的
        let evicted = service.evict_stale_vnc_activity(100);
        assert_eq!(evicted, 2, "应清理 2 个过期条目");
        assert_eq!(service.vnc_activity.len(), 1);
        assert!(service.vnc_activity.contains_key("active_user"));
        assert!(!service.vnc_activity.contains_key("stale_user_1"));
        assert!(!service.vnc_activity.contains_key("stale_user_2"));
    }

    #[test]
    fn test_evict_stale_vnc_activity_empty() {
        let config = ProxyConfig::default();
        let service = PingoraProxyService::new(config);
        assert_eq!(service.evict_stale_vnc_activity(60), 0);
    }

    #[test]
    fn test_evict_stale_vnc_activity_boundary() {
        // 边界测试：last_seen 正好等于 max_age 边界
        let config = ProxyConfig::default();
        let service = PingoraProxyService::new(config);

        let now = chrono::Utc::now().timestamp();
        // now - 100，max_age = 100：now - last_seen = 100，不 > 100，不清理（严格大于）
        service
            .vnc_activity
            .insert("boundary_user".to_string(), AtomicI64::new(now - 100));

        let evicted = service.evict_stale_vnc_activity(100);
        assert_eq!(evicted, 0, "严格大于语义，正好等于不清理");
        assert!(service.vnc_activity.contains_key("boundary_user"));
    }

    #[test]
    fn test_record_then_snapshot_then_evict_integration() {
        // 集成测试：record → snapshot → evict 完整流程
        let config = ProxyConfig::default();
        let service = PingoraProxyService::new(config);

        // Step 1: record 两个活跃 user
        record_vnc_activity(&service.vnc_activity, "user_A");
        record_vnc_activity(&service.vnc_activity, "user_B");

        // Step 2: snapshot 应有 2 个
        let snapshot = service.vnc_activity_snapshot();
        assert_eq!(snapshot.len(), 2);

        // Step 3: 手动让一个 user 变 stale（模拟 60s 没活动）
        let old_ts = chrono::Utc::now().timestamp() - 120;
        service
            .vnc_activity
            .get("user_A")
            .unwrap()
            .store(old_ts, Ordering::Relaxed);

        // Step 4: evict 60s 前的（实际 max_age = 60s）
        let evicted = service.evict_stale_vnc_activity(60);
        assert_eq!(evicted, 1);
        assert!(!service.vnc_activity.contains_key("user_A"));
        assert!(service.vnc_activity.contains_key("user_B"));
    }

    #[test]
    fn test_service_clone() {
        let config = ProxyConfig::default();
        let service = PingoraProxyService::new(config);
        let cloned = service.clone();
        assert_eq!(service.use_round_robin, cloned.use_round_robin);
    }

    #[tokio::test]
    async fn test_vnc_backend_management() {
        let config = ProxyConfig::default();
        let service = PingoraProxyService::new(config);

        // 添加 VNC 后端
        service.add_vnc_backend("user1", "192.168.1.100");
        assert!(service.has_vnc_backend("user1"));
        assert_eq!(
            service.get_vnc_backend("user1"),
            Some("192.168.1.100".to_string())
        );
        assert_eq!(service.vnc_backend_count(), 1);

        // 列出所有后端
        let backends = service.list_vnc_backends();
        assert_eq!(backends.len(), 1);
        assert_eq!(backends.get("user1"), Some(&"192.168.1.100".to_string()));

        // 移除后端
        let removed = service.remove_vnc_backend("user1");
        assert_eq!(removed, Some("192.168.1.100".to_string()));
        assert!(!service.has_vnc_backend("user1"));
        assert_eq!(service.vnc_backend_count(), 0);
    }

    #[tokio::test]
    async fn test_backend_count() {
        let config = ProxyConfig::default();
        let service = PingoraProxyService::new(config);
        assert_eq!(service.backend_count().await, 1); // 默认后端
    }
}
