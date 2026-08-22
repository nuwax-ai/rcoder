//! `PortProxy` 的 `ProxyHttp` trait 实现 —— 请求生命周期（request_filter → upstream → response）。
//!
//! 从 `service/mod.rs` 拆出，使 mod.rs 聚焦 struct 定义 + 构造（new/builders/create_pingora_proxy）。
//! 子模块可直接访问 `PortProxy` 的私有字段（隐私规则：子模块可见祖先模块的私有项）。

use async_trait::async_trait;
use pingora_core::Result as PingoraResult;
use pingora_core::protocols::Digest;
use pingora_core::upstreams::peer::{ALPN, HttpPeer};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};
use std::time::Duration;
use tracing::{debug, error, warn};

use crate::router::RouteType;

use super::{PortProxy, TrackingCtx, utils};

/// 唤醒超时/失败时 503 响应的 Retry-After(秒)。客户端据此延后重试(app 仍在后台启动)。
const WAKE_503_RETRY_AFTER_SECS: &str = "15";

#[async_trait]
impl ProxyHttp for PortProxy {
    type CTX = TrackingCtx;

    fn new_ctx(&self) -> Self::CTX {
        TrackingCtx::new()
    }

    /// 请求过滤阶段：UserApp 访问追踪 + 流量唤醒。
    ///
    /// 仅 `/proxy/apps/{user_id}/{app_id}/{port}/...` 路由触发：
    /// 1. `touch(app_id)` 记录最近 HTTP 访问（闲置回收信号源，内部节流）；
    /// 2. 若 app stopped → `ensure_running`（hold-and-wait ≤60s）拉起；超时/失败 → 503+Retry-After。
    /// 其余路由直接放行（Ok(false) → 继续 upstream_peer）。
    async fn request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> PingoraResult<bool> {
        // 仅 /proxy/apps/* 路由需要访问追踪 + 唤醒;前缀快滤,避免每请求都走 matchit 树匹配
        // (其余路由 /proxy/{port}、/web/ttyd、/computer/vnc、/api/* 等直接放行)
        let path = Self::normalize_path(session.req_header().uri.path());
        if !path.starts_with("/proxy/apps/") {
            return Ok(false);
        }
        let app_id: Option<String> = match self.router.at(path) {
            Ok(m) => match m.value {
                RouteType::AppPortProxy => m.params.get("app_id").map(|s| s.to_string()),
                _ => None,
            },
            Err(_) => None,
        };

        if let Some(app_id) = app_id {
            // ① 访问追踪
            if let Some(ref tracker) = self.access_tracker {
                tracker.touch(&app_id);
            }
            // ② 流量唤醒（stopped app 才触发）
            if let Some(ref wc) = self.wake_control
                && wc.is_stopped(&app_id)
            {
                match wc.ensure_running(&app_id).await {
                    shared_types::WakeOutcome::Ready
                    | shared_types::WakeOutcome::AlreadyRunning => { /* 放行到 upstream */ }
                    shared_types::WakeOutcome::Timeout | shared_types::WakeOutcome::Failed(_) => {
                        // hold-and-wait 超时/失败：app 仍在后台启动，返 503 + Retry-After
                        let mut resp = ResponseHeader::build(503, None)?;
                        resp.insert_header("Retry-After", WAKE_503_RETRY_AFTER_SECS)?;
                        session.write_response_header(Box::new(resp), true).await?;
                        return Ok(true); // 已直接响应，跳过 upstream
                    }
                }
            }
        }
        Ok(false)
    }

    /// 上游请求过滤阶段
    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        debug!(
            "[PINGORA] upstream_request_filter called: path={}",
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
                    warn!("[PINGORA_AUTH] Invalid API key for path: {}", path);
                    return Err(
                        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(401))
                            .more_context("Invalid API key".to_string()),
                    );
                }
                Err(shared_types::ApiKeyAuthError::Missing) => {
                    warn!("[PINGORA_AUTH] Missing x-api-key header for path: {}", path);
                    return Err(
                        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(401))
                            .more_context("Missing x-api-key header".to_string()),
                    );
                }
                Err(shared_types::ApiKeyAuthError::ConfigError) => {
                    error!("[PINGORA_AUTH] Configuration error");
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

        self.dispatch_upstream_request(
            *matched.value,
            matched.params,
            upstream_request,
            &original_uri,
            ctx,
            &path,
        )
        .await?;

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

        debug!("[PINGORA] upstream_peer called: path={}", path);

        // 使用 matchit 匹配路由
        let matched = self.router.at(path).map_err(|_| {
            warn!("route not found: {}", path);
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
        })?;

        self.dispatch_upstream_peer(*matched.value, matched.params, ctx)
            .await
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
                "[API_PROXY] Connection established: ALPN={}, {}, reused={}",
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
                .record_response_port(port, &status_text, duration);
        }

        // 减少活跃连接数
        self.metrics.dec_active();

        // 只在 API 代理场景打印详细日志
        if ctx.upstream_host.is_some() {
            debug!(
                "[API_PROXY] Response: status={}, duration={:?}",
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

        Ok(None)
    }
}

impl PortProxy {
    /// 规范化路径（去除尾部斜杠）
    fn normalize_path(raw: &str) -> &str {
        utils::normalize_path(raw)
    }
}
