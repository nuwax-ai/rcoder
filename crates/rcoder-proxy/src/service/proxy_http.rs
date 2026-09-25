//! `PortProxy` 的 `ProxyHttp` trait 实现 —— 请求生命周期（request_filter → upstream → response）。
//!
//! 从 `service/mod.rs` 拆出，使 mod.rs 聚焦 struct 定义 + 构造（new/builders/create_pingora_proxy）。
//! 子模块可直接访问 `PortProxy` 的私有字段（隐私规则：子模块可见祖先模块的私有项）。

use crate::service::dispatch::DispatchRequest;
use async_trait::async_trait;
use pingora_core::protocols::Digest;
use pingora_core::upstreams::peer::{ALPN, HttpPeer};
use pingora_core::{ErrorType, Result as PingoraResult};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};
use shared_types::RemoteWakeState;
use std::time::Duration;
use tracing::{debug, error, warn};

use crate::router::RouteType;

use super::{PortProxy, ProdConnectRecovery, TrackingCtx, utils};

/// 唤醒超时/失败时 503 响应的 Retry-After(秒)。客户端据此延后重试(app 仍在后台启动)。
const WAKE_503_RETRY_AFTER_SECS: &str = "15";
const CONNECT_RECOVERY_RETRY_DELAY: Duration = Duration::from_millis(250);
const CONNECT_RECOVERY_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);

/// Probe only after Pingora's real first connection failed. This uses the same
/// destination as the request but sends no HTTP bytes; Pingora retries the
/// original connection after the Service path becomes reachable.
async fn wait_for_peer_connection(peer: &HttpPeer, deadline: tokio::time::Instant) -> bool {
    let Some(address) = peer._address.as_inet().copied() else {
        return false;
    };
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        let attempt_deadline = (now + CONNECT_RECOVERY_ATTEMPT_TIMEOUT).min(deadline);
        if matches!(
            tokio::time::timeout_at(attempt_deadline, tokio::net::TcpStream::connect(address))
                .await,
            Ok(Ok(_))
        ) {
            return true;
        }
        tokio::time::sleep(
            CONNECT_RECOVERY_RETRY_DELAY
                .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
        )
        .await;
    }
}

#[async_trait]
impl ProxyHttp for PortProxy {
    type CTX = TrackingCtx;

    fn new_ctx(&self) -> Self::CTX {
        TrackingCtx::new()
    }

    /// 请求过滤阶段：Userapp 访问追踪 + 流量唤醒。
    ///
    /// 两类 prod 路由触发唤醒（stopped/starting app → `ensure_running` hold-and-wait ≤60s
    /// 拉起；超时/失败 → 503+Retry-After）：
    /// - `/api/v1/userapp/proxy/app/prod/{user_id}/{app_id}/...` 应用业务流量：
    ///   **touch + wake**（touch 记录最近访问，是闲置回收的信号源）；
    /// - `/api/v1/userapp/proxy/{ttyd,dbx}/prod/{user_id}/{app_id}` 工具族：
    ///   **只 wake 不 touch**（终端/DB 客户端连接不算业务活跃——挂终端不阻止
    ///   闲置回收，回收后下次连接自动唤醒再转发，ttyd 前端自动重连兜底首连窗口）。
    /// 其余路由直接放行（Ok(false) → 继续 upstream_peer）。
    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<bool> {
        // Custom Page 预览转发内部入口：令牌校验（缺失/不符 → 404，不暴露端点
        // 存在性）+ 实例身份校验（不匹配 → 410 触发转发方缓存失效重解析；
        // 登记缺失（对账中）→ 503）。校验通过记 ctx（后续阶段免重复校验）。
        // 在 dbx/wake 之前——内部入口无唤醒语义，且必须先挡住外部探测。
        if session
            .req_header()
            .uri
            .path()
            .starts_with("/internal/preview-forward")
        {
            return Self::preview_forward_gate(session, ctx, &self.preview_slot).await;
        }

        // dbx 入口尾斜杠规范化（先于唤醒判定——重定向后的二次请求才进入 wake，
        // stopped app 不在无子路径请求上空等一轮）。
        let raw_uri = session.req_header().uri.clone();
        if let Some(location) =
            dbx_root_redirect_location(&self.router, raw_uri.path(), raw_uri.query())
        {
            let mut resp = ResponseHeader::build(307, None)?;
            resp.insert_header("Location", location)?;
            session.write_response_header(Box::new(resp), true).await?;
            return Ok(true); // 已直接响应，跳过 upstream
        }

        // 前缀快滤，避免每请求都走 matchit 树匹配（其余路由 /proxy/{port}、
        // /web/ttyd、/computer/vnc、/api/* 等直接放行；dev 流量无闲置回收语义，不触发）
        let path = Self::normalize_path(session.req_header().uri.path());
        if let Some((app_id, touch)) = classify_wake_target(&self.router, path) {
            // ① 访问追踪（仅业务流量）
            if touch && let Some(ref tracker) = self.access_tracker {
                let _ = tracker.touch(&app_id).await;
            }
            // ② 流量唤醒（stopped/starting app 触发；手动 stop 与闲置回收统一——
            //    有请求即唤醒，见 AppWakeControl::ensure_running 语义）。
            //    is_stopped 为内存视图：多副本下其他副本 stop 后本副本不知情，
            //    remote_wake_pending 兜底查集群真实状态（TTL 缓存节流）。
            if let Some(ref wc) = self.wake_control {
                let deadline = tokio::time::Instant::now() + wc.wake_timeout();
                ctx.prod_connect_recovery = Some(ProdConnectRecovery {
                    app_id: app_id.clone(),
                    deadline,
                    retry_requested: false,
                    runtime_checked: false,
                    unavailable_response: false,
                });
                let wake_pending = if wc.is_stopped(&app_id) {
                    true
                } else {
                    match tokio::time::timeout_at(deadline, wc.remote_wake_pending(&app_id)).await {
                        Ok(pending) => pending,
                        Err(_) => {
                            let mut resp = ResponseHeader::build(503, None)?;
                            resp.insert_header("Retry-After", WAKE_503_RETRY_AFTER_SECS)?;
                            session.write_response_header(Box::new(resp), true).await?;
                            return Ok(true);
                        }
                    }
                };
                if !wake_pending {
                    return Ok(false);
                }
                tracing::info!(
                    "[WAKE] {} traffic waits for app: {}",
                    if touch { "app" } else { "tool" },
                    app_id
                );
                match tokio::time::timeout_at(deadline, wc.ensure_running(&app_id)).await {
                    Ok(
                        shared_types::WakeOutcome::Ready
                        | shared_types::WakeOutcome::AlreadyRunning,
                    ) => {}
                    Ok(
                        shared_types::WakeOutcome::Timeout
                        | shared_types::WakeOutcome::Failed(_)
                        | shared_types::WakeOutcome::Blocked { .. },
                    )
                    | Err(_) => {
                        // 未获得可用上游（包括操作占用）；返回 503，不宣称仍在启动。
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
                    return Err(pingora_core::Error::new(ErrorType::HTTPStatus(401))
                        .more_context("Invalid API key".to_string()));
                }
                Err(shared_types::ApiKeyAuthError::Missing) => {
                    warn!("[PINGORA_AUTH] Missing x-api-key header for path: {}", path);
                    return Err(pingora_core::Error::new(ErrorType::HTTPStatus(401))
                        .more_context("Missing x-api-key header".to_string()));
                }
                Err(shared_types::ApiKeyAuthError::ConfigError) => {
                    error!("[PINGORA_AUTH] Configuration error");
                    return Err(pingora_core::Error::new(ErrorType::HTTPStatus(500))
                        .more_context("Internal configuration error".to_string()));
                }
            }
        }

        let path = Self::normalize_path(upstream_request.uri.path()).to_string();

        // 使用 matchit 匹配路由
        let matched = self.router.at(&path).map_err(|_| {
            warn!("route not found: {}", path);
            pingora_core::Error::new(ErrorType::HTTPStatus(404))
        })?;

        let original_uri = upstream_request.uri.clone();

        self.dispatch_upstream_request(DispatchRequest {
            route: *matched.value,
            params: matched.params,
            original_uri: &original_uri,
            path: &path,
            upstream_request,
            ctx,
        })
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
            pingora_core::Error::new(ErrorType::HTTPStatus(404))
        })?;

        let recovery = ctx.prod_connect_recovery.as_mut().and_then(|state| {
            if state.retry_requested {
                state.retry_requested = false;
                let check_runtime = !state.runtime_checked;
                state.runtime_checked = true;
                Some((state.app_id.clone(), state.deadline, check_runtime))
            } else {
                None
            }
        });
        if let Some((app_id, deadline, check_runtime)) = &recovery {
            let Some(wc) = self.wake_control.as_ref() else {
                return Err(pingora_core::Error::new(ErrorType::HTTPStatus(503)));
            };
            if *check_runtime {
                let state =
                    tokio::time::timeout_at(*deadline, wc.remote_wake_state_fresh(app_id)).await;
                match state {
                    Ok(RemoteWakeState::WakePending) => {
                        match tokio::time::timeout_at(*deadline, wc.ensure_running(app_id)).await {
                            Ok(
                                shared_types::WakeOutcome::Ready
                                | shared_types::WakeOutcome::AlreadyRunning,
                            ) => {}
                            _ => {
                                if let Some(ctx_state) = ctx.prod_connect_recovery.as_mut() {
                                    ctx_state.unavailable_response = true;
                                }
                                return Err(pingora_core::Error::new(ErrorType::HTTPStatus(503)));
                            }
                        }
                    }
                    Ok(RemoteWakeState::Running) => {}
                    Ok(RemoteWakeState::Unavailable) => {
                        return Err(pingora_core::Error::new(ErrorType::HTTPStatus(502)));
                    }
                    Err(_) => {
                        if let Some(ctx_state) = ctx.prod_connect_recovery.as_mut() {
                            ctx_state.unavailable_response = true;
                        }
                        return Err(pingora_core::Error::new(ErrorType::HTTPStatus(503)));
                    }
                }
            }
        }
        let mut peer = self
            .dispatch_upstream_peer(*matched.value, matched.params, ctx)
            .await?;
        if let Some(state) = ctx.prod_connect_recovery.as_mut() {
            let remaining = state
                .deadline
                .saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                state.unavailable_response = true;
                return Err(pingora_core::Error::new(ErrorType::HTTPStatus(503)));
            }
            peer.options.connection_timeout = Some(
                peer.options
                    .connection_timeout
                    .unwrap_or(remaining)
                    .min(remaining),
            );
            peer.options.total_connection_timeout = Some(
                peer.options
                    .total_connection_timeout
                    .unwrap_or(remaining)
                    .min(remaining),
            );
        }
        if let Some((app_id, deadline, _)) = recovery
            && !wait_for_peer_connection(&peer, deadline).await
        {
            warn!(%app_id, "Prod UserApp Service connection did not recover before wake deadline");
            if let Some(state) = ctx.prod_connect_recovery.as_mut() {
                state.unavailable_response = true;
            }
            return Err(pingora_core::Error::new(ErrorType::HTTPStatus(503)));
        }
        Ok(peer)
    }

    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Self::CTX,
        mut error: Box<pingora_core::Error>,
    ) -> Box<pingora_core::Error> {
        if let Some(state) = ctx.prod_connect_recovery.as_mut()
            && matches!(
                error.etype(),
                ErrorType::ConnectRefused | ErrorType::ConnectNoRoute | ErrorType::ConnectTimedout
            )
        {
            if tokio::time::Instant::now() < state.deadline {
                state.retry_requested = true;
                error.set_retry(true);
            } else {
                state.unavailable_response = true;
            }
        }
        error
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
        ctx.prod_connect_recovery = None;
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
        // 预览跨 Pod 转发收到宿主 410（身份不匹配：IP 复用/实例换代）——
        // 立即失效本端口的解析缓存，下次请求重解析到新宿主（免等 TTL）。
        if ctx.preview_peer.is_some()
            && upstream_response.status == 410
            && let Some(origin_port) = ctx.preview_origin_port
            && let Some(deps) = self.preview_slot.load().as_ref()
        {
            tracing::info!(
                port = origin_port,
                "preview forward got 410 from host; invalidating route cache"
            );
            deps.coordination.invalidate_route(origin_port).await;
        }
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
        ctx.prod_metrics_counted = false;

        // 记录上游状态码：upstream_response_body_filter 据此收集 4xx/5xx 错误体
        //（此前从未写入，收集分支是死代码）
        ctx.upstream_status = Some(status.as_u16());

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
        // 如果是 4xx/5xx 响应，收集错误响应体（封顶 64KB：网关 HTML 大页/
        // 误配代理到的默认站点会让单请求内存随 body 线性增长）
        if let Some(status) = ctx.upstream_status
            && status >= 400
            && let Some(body_bytes) = body
        {
            const MAX_ERROR_BODY: usize = 64 * 1024;
            let remaining = MAX_ERROR_BODY.saturating_sub(ctx.error_body_buf.len());
            if remaining > 0 {
                ctx.error_body_buf
                    .extend_from_slice(&body_bytes[..remaining.min(body_bytes.len())]);
            }
            // 流结束时消费：错误体首 1KB 打日志（排障——网关 HTML 大页/
            // 误配默认站点的根因就在 body 里；buffer 本身 64KB 封顶防膨胀）
            if _end_of_stream && !ctx.error_body_buf.is_empty() {
                let head: Vec<u8> = ctx.error_body_buf.iter().copied().take(1024).collect();
                debug!(
                    "[PORT_PROXY] upstream {} error body: {}",
                    ctx.upstream_status.unwrap_or(0),
                    String::from_utf8_lossy(&head)
                );
            }
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

/// 唤醒目标分类：路径 → `(app_id, touch)`。
///
/// - `/api/v1/userapp/proxy/app/prod/...` 应用业务流量 → touch=true（闲置回收信号源）；
/// - `/api/v1/userapp/proxy/{ttyd,dbx}/prod/{app_id}` 工具族 → touch=false（终端/DB
///   连接不算业务活跃，不刷新闲置计时——挂终端不阻止回收，回收后下次连接再唤醒）；
/// - 其余路由 → None（不触发唤醒）。
fn classify_wake_target(router: &matchit::Router<RouteType>, path: &str) -> Option<(String, bool)> {
    if !path.starts_with("/api/v1/userapp/proxy/") {
        return None;
    }
    match router.at(path) {
        Ok(m) => match m.value {
            RouteType::ProdAppProxy => m.params.get("app_id").map(|s| (s.to_string(), true)),
            RouteType::RuntimeTtydProxy | RouteType::ProdDbxProxy => {
                m.params.get("app_id").map(|s| (s.to_string(), false))
            }
            _ => None,
        },
        Err(_) => None,
    }
}

/// dbx 入口无尾斜杠 → 307 目标（原路径 + `/`，query 原样保留；相对 Location，
/// 不拼 origin——代理可被挂任意 host/端口下）。
///
/// dbx-web 是相对路径 SPA（index.html `./assets/...`），浏览器以 URL 目录段为
/// 基准解析相对引用——无尾斜杠时 `{app_id}` 被当文件名、基准向上错位一级
/// （`./assets` 落到 `/{user_id}/assets` → 静态资源 404 白屏）。判定=路由命中
/// dbx 族且逻辑上无子路径（`{*path}` 缺失或为空——两种 matchit 命中形态等价，
/// 不依赖 matchit 在精确/通配间的择优）；带尾斜杠/带子路径请求不受影响，
/// `!path.ends_with('/')` 兼作 matchit 尾斜杠行为变化时的死循环双保险。
fn dbx_root_redirect_location(
    router: &matchit::Router<RouteType>,
    path: &str,
    query: Option<&str>,
) -> Option<String> {
    if !path.starts_with("/api/v1/userapp/proxy/dbx/") || path.ends_with('/') {
        return None;
    }
    let matched = router.at(path).ok()?;
    match matched.value {
        RouteType::DevDbxProxy | RouteType::ProdDbxProxy
            if matched.params.get("path").is_none_or(|p| p.is_empty()) =>
        {
            Some(match query {
                Some(q) => format!("{path}/?{q}"),
                None => format!("{path}/"),
            })
        }
        _ => None,
    }
}

/// 预览转发闸门挂载（impl PortProxy 块，非 ProxyHttp trait 成员）。
impl PortProxy {
    /// 预览转发内部入口闸门（request_filter 专用短路）。
    ///
    /// 语义（spec 行为不变量 11/12）：
    /// - 槽未装配/令牌缺失或不符 → 404（外部探测者不可区分端点是否存在）；
    /// - 实例/宿主/端口身份不匹配（IP 复用、实例换代）→ 410（转发方据此失效
    ///   缓存重解析）；
    /// - 权威库称本机宿主但本地登记缺失（对账/心跳收敛中）→ 503，不转发；
    /// - 校验通过 → 记 `ctx.preview_forward_port` 放行（后续阶段不再重复校验）。
    async fn preview_forward_gate(
        session: &mut Session,
        ctx: &mut TrackingCtx,
        preview_slot: &std::sync::Arc<
            arc_swap::ArcSwapOption<std::sync::Arc<super::types::PreviewRouteDeps>>,
        >,
    ) -> PingoraResult<bool> {
        async fn respond(status: u16, session: &mut Session) -> PingoraResult<bool> {
            let resp = ResponseHeader::build(status, None)?;
            session.write_response_header(Box::new(resp), true).await?;
            Ok(true)
        }
        let Some(deps) = preview_slot.load().as_ref().map(std::sync::Arc::clone) else {
            return respond(404, session).await;
        };
        let token_ok = session
            .req_header()
            .headers
            .get("x-preview-internal-token")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|provided| provided == deps.internal_token);
        if !token_ok {
            return respond(404, session).await;
        }
        // 路径手工解析（request_filter 阶段无 matchit 参数）：
        // /internal/preview-forward/{instance_id}/{port}/{rest...}
        let segments: Vec<&str> = session
            .req_header()
            .uri
            .path()
            .trim_start_matches('/')
            .split('/')
            .collect();
        // [internal, preview-forward, {instance_id}, {port}, {rest...}...]
        if segments.len() < 5 || segments[0] != "internal" || segments[1] != "preview-forward" {
            // 根形态（无 rest）由路由层兜底，但无 rest 的转发无意义——404
            return respond(404, session).await;
        }
        let instance_id = segments[2];
        let Ok(vite_port) = segments[3].parse::<u16>() else {
            return respond(404, session).await;
        };
        match deps
            .coordination
            .check_forward(instance_id, vite_port)
            .await
        {
            shared_types::PreviewForwardCheck::Allowed => {
                ctx.preview_forward_port = Some(vite_port);
                Ok(false)
            }
            shared_types::PreviewForwardCheck::IdentityMismatch => {
                tracing::info!(
                    instance_id,
                    vite_port,
                    "preview forward identity mismatch -> 410 (forwarder should re-resolve)"
                );
                respond(410, session).await
            }
            shared_types::PreviewForwardCheck::NotReady => respond(503, session).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::create_router;
    use matchit::Router;

    fn test_router() -> Router<RouteType> {
        create_router().expect("router build")
    }

    /// 工具族路由：识别为唤醒目标且不 touch（挂终端不算业务活跃）。
    #[test]
    fn tool_routes_wake_without_touch() {
        let router = test_router();
        // classify 接收 request_filter 规范化后的路径（尾部斜杠已剥）
        for path in [
            "/api/v1/userapp/proxy/ttyd/prod/u1/app-1",
            "/api/v1/userapp/proxy/ttyd/prod/u1/app-1/token.js",
            "/api/v1/userapp/proxy/dbx/prod/u1/app-1",
        ] {
            let (app_id, touch) =
                classify_wake_target(&router, path).unwrap_or_else(|| panic!("{path} unmatched"));
            assert_eq!(app_id, "app-1", "{path}");
            assert!(!touch, "tool traffic must not refresh idle timer: {path}");
        }
    }

    /// 应用业务流量：touch + wake。
    #[test]
    fn app_traffic_wakes_with_touch() {
        let router = test_router();
        let (app_id, touch) =
            classify_wake_target(&router, "/api/v1/userapp/proxy/app/prod/u1/app-1/x")
                .expect("app proxy route must match");
        assert_eq!(app_id, "app-1");
        assert!(touch);
    }

    /// 非唤醒路由与 dev 工具族不触发。
    #[test]
    fn other_routes_do_not_wake() {
        let router = test_router();
        assert!(classify_wake_target(&router, "/api/v1/userapp/build").is_none());
        assert!(classify_wake_target(&router, "/web/ttyd/u1").is_none());
        // dev 工具族走 builder 注册表定位，不在 prod 唤醒范围
        assert!(classify_wake_target(&router, "/api/v1/userapp/proxy/ttyd/dev/u1/app-1").is_none());
        // 旧路径形态（前缀统一前）不再命中任何 userApp 路由
        assert!(classify_wake_target(&router, "/userapp/proxy/ttyd/u1/app-1").is_none());
        assert!(classify_wake_target(&router, "/proxy/userapp/proxy/1/app-1/x").is_none());
    }

    /// dbx 入口无尾斜杠：307 到同路径 + `/`（query 原样保留），dev/prod 双阶段。
    #[test]
    fn dbx_root_without_trailing_slash_redirects() {
        let router = test_router();
        assert_eq!(
            dbx_root_redirect_location(&router, "/api/v1/userapp/proxy/dbx/dev/4/5", None),
            Some("/api/v1/userapp/proxy/dbx/dev/4/5/".to_string())
        );
        assert_eq!(
            dbx_root_redirect_location(
                &router,
                "/api/v1/userapp/proxy/dbx/prod/4/app-5",
                Some("a=1")
            ),
            Some("/api/v1/userapp/proxy/dbx/prod/4/app-5/?a=1".to_string())
        );
    }

    /// 带尾斜杠/带子路径已是正确基准不重定向；其他工具族与无关路由不误伤。
    #[test]
    fn dbx_paths_with_content_do_not_redirect() {
        let router = test_router();
        let base = "/api/v1/userapp/proxy/dbx/dev/4/5";
        assert_eq!(
            dbx_root_redirect_location(&router, &format!("{base}/"), None),
            None
        );
        assert_eq!(
            dbx_root_redirect_location(&router, &format!("{base}/assets/index.js"), None),
            None
        );
        assert_eq!(
            dbx_root_redirect_location(&router, &format!("{base}/api/auth/check"), None),
            None
        );
        // ttyd/vnc 非 dbx 族不受影响（ttyd 为 WS 直连形态，无相对路径基准问题）
        assert_eq!(
            dbx_root_redirect_location(&router, "/api/v1/userapp/proxy/ttyd/dev/4/5", None),
            None
        );
        assert_eq!(
            dbx_root_redirect_location(&router, "/api/v1/userapp/proxy/ttyd/prod/4/5", None),
            None
        );
        assert_eq!(
            dbx_root_redirect_location(&router, "/web/ttyd/u1", None),
            None
        );
    }
}
