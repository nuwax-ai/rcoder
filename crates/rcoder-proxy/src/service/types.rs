//! 类型定义模块
//!
//! 包含代理服务使用的所有类型定义、常量和指标统计结构。

use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// 每端口指标统计
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

/// 端口指标快照
pub struct PortSnapshot {
    pub port: u16,
    pub requests: u64,
    pub successes: u64,
    pub failures: u64,
    pub total_response_time_ns: u64,
}

/// 代理指标统计
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

    pub fn record_request_port(&self, port: u16) {
        let arc = self.get_or_create_port_metrics(port);
        arc.requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_response(&self, status_text: &str, duration: std::time::Duration) {
        self.total_responses.fetch_add(1, Ordering::Relaxed);
        self.total_response_time_ns.fetch_add(
            duration.as_nanos().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        // 粗略判断成功：2xx
        let is_success = status_text.starts_with('2');
        if is_success {
            self.successful_responses.fetch_add(1, Ordering::Relaxed);
        } else {
            self.failed_responses.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_response_port(
        &self,
        port: u16,
        status_text: &str,
        duration: std::time::Duration,
    ) {
        let arc = self.get_or_create_port_metrics(port);
        arc.total_response_time_ns.fetch_add(
            duration.as_nanos().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
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
        if let Some(metrics) = self.port_map.get(&port) {
            return Arc::clone(metrics.value());
        }
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

/// 健康状态枚举
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

/// 健康信息
#[derive(Clone, Debug)]
pub struct HealthInfo {
    pub status: HealthState,
    pub last_check: SystemTime,
}

/// 请求追踪上下文
#[derive(Clone)]
pub struct ProdConnectRecovery {
    pub app_id: String,
    pub deadline: tokio::time::Instant,
    pub retry_requested: bool,
    pub runtime_checked: bool,
    pub unavailable_response: bool,
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
    /// 预览路由覆盖上游（跨 Pod 转发=宿主 Pod:peer_api_port；
    /// 宿主侧 internal 路由=127.0.0.1:vite_port）。None=非预览路径。
    /// 由 upstream_peer 阶段的 resolve_route 决策写入（pingora 阶段序
    /// upstream_peer 先于 upstream_request_filter——决策必须发生在 peer
    /// 选择时，否则非宿主副本在连接 127.0.0.1:vite_port 时即失败）。
    pub preview_peer: Option<(String, u16)>,
    /// 预览跨 Pod 转发的路径重写要素 (instance_id, preview_port)——与
    /// preview_peer 同点（upstream_peer 阶段）产出，upstream_request_filter
    /// 阶段消费为 `/internal/preview-forward/{instance}/{port}{path}` + 令牌头。
    /// 用 `take()` 一次性消费（request 阶段不再 load slot 二次读取令牌）。
    pub preview_rewrite: Option<(String, u16)>,
    /// 预览内部令牌——peer 阶段从 deps 取出写入，request 阶段直接消费
    /// （避免 ArcSwap 二次 load；token 与决策同批次、同值保证一致性）。
    pub preview_internal_token: Option<String>,
    /// 预览转发校验已过的 vite 端口（internal/preview-forward 路由用，
    /// request_filter 校验通过后记录，后续阶段免重复校验）。
    pub preview_forward_port: Option<u16>,
    /// 预览跨 Pod 转发的原始入口端口（`/proxy/{port}`；宿主 410 后失效缓存用）。
    pub preview_origin_port: Option<u16>,
    /// Request-scoped deadline and retry state for prod UserApp connections.
    pub prod_connect_recovery: Option<ProdConnectRecovery>,
    /// Keep prod app request counting stable across retries, including route failures.
    pub prod_request_recorded: bool,
    /// Only one handler metric increment and active gauge increment per request.
    pub prod_metrics_counted: bool,
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
            preview_peer: None,
            preview_rewrite: None,
            preview_internal_token: None,
            preview_forward_port: None,
            preview_origin_port: None,
            prod_connect_recovery: None,
            prod_request_recorded: false,
            prod_metrics_counted: false,
        }
    }
}

// ============================================================================
// 端口常量
// ============================================================================

/// 音频服务端口（rcoder-proxy 专用，shared_types 未定义——builder 容器内无
/// 音频服务，不进 K8s Service 端口清单）
pub const AUDIO_HTTP_PORT: u16 = 6090; // 音频静态文件服务
pub const AUDIO_WS_PORT: u16 = 6089; // 音频 WebSocket 流

// 注：跨 crate 共享的端口常量（NOVNC_PORT、WS_TERMINAL_PORT、TTYD_PORT、IME_PORT 等）
// 统一定义在 `shared_types::constants`，本 crate 直接引用，不在本地重复定义。
// （IME_PORT 已提升 shared_types——K8s Service 端口清单消费同值。）

/// 预览路由依赖（rcoder 装配后经 ArcSwap 槽回填；None 槽=预览路由未启用）。
///
/// rcoder 主进程装配协调器晚于 Pingora 启动——与 dev_ensure/app_runtime_ip
/// 槽同款回填模式（PortProxy 与 PingoraProxyService 共享同一 Arc）。
pub struct PreviewRouteDeps {
    pub coordination: Arc<dyn shared_types::PreviewCoordination>,
    /// 对等副本主 API 端口（跨 Pod stop/verify/log 派发 → axum 内部端点）。
    pub peer_api_port: u16,
    /// 对等副本 Pingora 代理端口（跨 Pod 预览转发 `/internal/preview-forward`
    /// 的宿主面——该路由注册在 Pingora 路由表，非 axum 主 API）。
    pub peer_proxy_port: u16,
    /// 内部令牌（转发 hop 注入；宿主侧校验，不符 404）。
    pub internal_token: String,
}
