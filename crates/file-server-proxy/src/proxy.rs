//! hyper 转发核心：accept 循环、单请求代理、进程内直连、错误响应。

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::config::{FileServerProxyConfig, SERVICE_TYPE_HEADER, Upstream};

/// 统一响应 body（上游 Incoming / 直连 axum Body 与本地错误 Full 的归一）。
/// hyper 连接层只要求 `HttpBody + Send`——不设 Sync 约束（axum 的 `Body`
/// 非 Sync，`BoxBody` 的 Sync 要求会把直连路径挡在门外）。
pub type ProxyBody =
    std::pin::Pin<Box<dyn http_body::Body<Data = Bytes, Error = std::io::Error> + Send>>;

/// 上游 HTTP 客户端（连接池复用）。
pub(crate) type ProxyClient = hyper_util::client::legacy::Client<
    hyper_util::client::legacy::connect::HttpConnector,
    Incoming,
>;

/// 进程内直连通道（feature `embed-file-server`；npm/Electron 独立形态）。
///
/// 设置后 rust 域请求不再经 loopback 转发 `127.0.0.1:{rust_upstream_port}`，
/// 而是直接 `router.oneshot(request)` 进程内调用（file-server 以 lib 集成，
/// 单二进制单监听口）。容器/rcoder 嵌入形态不设置本值，转发路径原样。
#[cfg(feature = "embed-file-server")]
mod in_process {
    use std::sync::RwLock;

    static ROUTER: RwLock<Option<axum::Router>> = RwLock::new(None);

    /// 注册直连 router（幂等覆盖；bin 启动装配时调用）。
    pub fn set_in_process_router(router: axum::Router) {
        match ROUTER.write() {
            Ok(mut guard) => *guard = Some(router),
            Err(poisoned) => {
                *poisoned.into_inner() = Some(router);
            }
        }
    }

    /// 清除直连 router（测试复位；清除后回 loopback 转发路径）。
    pub fn clear_in_process_router() {
        match ROUTER.write() {
            Ok(mut guard) => *guard = None,
            Err(poisoned) => {
                *poisoned.into_inner() = None;
            }
        }
    }

    /// 取当前直连 router 的克隆（per-request clone 是 axum 官方模式，Arc 浅拷贝）。
    pub(super) fn take() -> Option<axum::Router> {
        ROUTER.read().ok().and_then(|guard| guard.clone())
    }
}

#[cfg(feature = "embed-file-server")]
pub use in_process::{clear_in_process_router, set_in_process_router};

/// 直连调用（feature 门控）：oneshot 进 file-server 的 axum Router，响应 body
/// 归一到 [`ProxyBody`]。handler 层错误（路由/中间件 panic 被 tower 捕获等）→ 502。
#[cfg(feature = "embed-file-server")]
async fn call_in_process(
    router: axum::Router,
    req: hyper::Request<Incoming>,
) -> hyper::Response<ProxyBody> {
    use tower::ServiceExt;
    match router.oneshot(req).await {
        // 直连不经网络代理跳，hop-by-hop 头不做剥除（我们即服务器，与独立
        // file-server bin 直连行为一致；hyper 连接层的 keep-alive 语义照常）
        Ok(resp) => resp.map(|body| Box::pin(body.map_err(std::io::Error::other)) as ProxyBody),
        Err(e) => {
            error!("file-server 分流代理直调内嵌 file-server 失败: {e}");
            bad_gateway("file-server upstream error")
        }
    }
}

/// Stop accepting, gracefully drain tracked connections, then cancel and join stragglers.
pub(crate) async fn serve(
    listener: tokio::net::TcpListener,
    client: ProxyClient,
    config: FileServerProxyConfig,
    shutdown: CancellationToken,
) -> Result<(), String> {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result { tracing::warn!("proxy connection task failed: {error}"); }
            },
            accepted = listener.accept() => {
                match accepted {
                    Ok((io, _peer)) => {
                        let client = client.clone();
                        let config = config.clone();
                        let closing = shutdown.clone();
                        connections.spawn(async move {
                            let service = hyper::service::service_fn(move |req| {
                                let client = client.clone();
                                let config = config.clone();
                                async move { proxy_request(req, client, config).await }
                            });
                            let io = hyper_util::rt::TokioIo::new(io);
                            // 注: 本代理不透传 WebSocket(upgrade 已列入 hop-by-hop 剥除);
                            // with_upgrades 仅为 hyper 连接层的 upgrade 协商宽容,
                            // 避免 h1 客户端带 upgrade 头时连接被硬断
                            let conn = hyper::server::conn::http1::Builder::new()
                                .serve_connection(io, service);
                            let conn = conn.with_upgrades();
                            tokio::pin!(conn);
                            let result = tokio::select! {
                                result = &mut conn => result,
                                _ = closing.cancelled() => {
                                    conn.as_mut().graceful_shutdown();
                                    conn.await
                                }
                            };
                            if let Err(error) = result {
                                tracing::debug!("proxy connection ended: {error}");
                            }
                        });
                    }
                    Err(e) => {
                        // 瞬时 accept 错误（EMFILE 等）: 记日志退避继续, 不退出整个代理
                        warn!("file-server 分流代理 accept 错误: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }
    drop(listener);
    let drain = async { while connections.join_next().await.is_some() {} };
    if tokio::time::timeout(std::time::Duration::from_secs(10), drain)
        .await
        .is_err()
    {
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        return Err(
            "proxy stopped after forcibly closing connections beyond the drain deadline".into(),
        );
    }
    Ok(())
}

/// 单请求转发：按 `/api/v1/userapp*` 前缀或 `x-service-type` header 选上游，
/// 方法/路径/headers/body 原样透传。
///
/// 上游不可达返回 502（不向客户端裸抛连接错误）。
async fn proxy_request(
    req: hyper::Request<Incoming>,
    client: ProxyClient,
    config: FileServerProxyConfig,
) -> Result<hyper::Response<ProxyBody>, std::convert::Infallible> {
    let (parts, body) = req.into_parts();

    // N07：令牌校验最先（401 早返——凭据不进日志；常量时间比较不必要，
    // 令牌非密钥协商而是准入配对）
    if let Some(expected) = config.auth_token.as_deref() {
        let provided = parts
            .headers
            .get("X-Proxy-Token")
            .and_then(|value| value.to_str().ok());
        if provided != Some(expected) {
            warn!("request rejected: missing or invalid proxy token");
            return Ok(unauthorized());
        }
    }

    let path = parts.uri.path();
    // PX-06: 请求 Connection 头声明的动态 hop-by-hop token（可能多值、逗号分隔）
    let request_connection_tokens = connection_tokens(&parts.headers);
    let service_type = parts
        .headers
        .get(SERVICE_TYPE_HEADER)
        .and_then(|v| v.to_str().ok());
    let port = match config.upstream_port_for(path, service_type) {
        Upstream::Rust(port) => {
            // PX-02: Rust 入口的路径边界独立于策略与 header——任何方式选中 Rust
            //（AllRust 全量、TsFirst 的 userapp 前缀或 x-service-type header）都过
            // 同一白名单; header 只选择实现, 不得把 /internal/* 等上游内部路由面
            //（/internal/pod/ensure 等）经文件入口触达。
            if !all_rust_path_allowed(path) {
                warn!("path rejected outside the Rust entry allowlist: {path}");
                return Ok(not_found("path not served on this entry"));
            }
            // 进程内直连（embed 形态装配后）：原请求重组后直接 oneshot 进
            // file-server 的 axum Router，不经 loopback 转发
            #[cfg(feature = "embed-file-server")]
            if let Some(router) = in_process::take() {
                return Ok(call_in_process(router, hyper::Request::from_parts(parts, body)).await);
            }
            port
        }
        Upstream::Ts(port) => port,
    };

    let path_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string());

    let mut upstream = hyper::Request::builder()
        .method(parts.method.clone())
        .uri(format!("http://127.0.0.1:{port}{path_query}"));
    let Some(headers) = upstream.headers_mut() else {
        // 仅 asterisk-form (`OPTIONS *`) 等异常 request-target 会走到这里
        error!("file-server 分流代理构造上游请求失败: 无效 request-target {path_query:?}");
        return Ok(bad_request("invalid request-target"));
    };
    for (name, value) in parts.headers.iter() {
        // PX-06: 动态/固定 hop-by-hop 与本跳凭据统一经 should_forward 过滤;
        // append 保多值 header (Cookie 链等)
        if !should_forward_request_header(name.as_str(), &request_connection_tokens) {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }

    let upstream_req = match upstream.body(body) {
        Ok(r) => r,
        // method/headers 均来自合法入站请求, 构造失败理论不可达
        Err(e) => {
            error!("file-server 分流代理构造上游请求失败: {e}");
            return Ok(bad_gateway("build upstream request failed"));
        }
    };

    // 整请求 300s 超时（宽限大文件上传/慢接口; 防"上游接受连接后不响应"无限堆积）
    const UPSTREAM_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
    let upstream_result =
        match tokio::time::timeout(UPSTREAM_REQUEST_TIMEOUT, client.request(upstream_req)).await {
            Ok(result) => result,
            Err(_elapsed) => {
                error!(
                    "file-server 分流代理上游 127.0.0.1:{port} 请求超时 \
                     ({UPSTREAM_REQUEST_TIMEOUT:?}, path {path_query})"
                );
                return Ok(bad_gateway("file-server upstream timeout"));
            }
        };
    match upstream_result {
        Ok(resp) => {
            let (mut parts, body) = resp.into_parts();
            // PX-06（响应方向）: 响应 Connection 声明的动态 token 与固定名单一并移除
            let response_connection_tokens = connection_tokens(&parts.headers);
            for h in HOP_BY_HOP {
                parts.headers.remove(h);
            }
            for token in &response_connection_tokens {
                parts.headers.remove(token.as_str());
            }
            parts.headers.remove(PROXY_TOKEN_HEADER);
            // PX-07: 有限响应 body 的空闲预算——SSE（text/event-stream）是合法
            // 长流豁免; 其余 body 空闲超预算以 io 错误终止（headers 已发,
            // 不伪造另一个错误状态码; 上游/路径在日志留痕）。
            let is_sse = parts
                .headers
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"));
            let body = if is_sse {
                Box::pin(body.map_err(std::io::Error::other))
            } else {
                body_with_idle_budget(body, path_query, RESPONSE_BODY_IDLE_BUDGET)
            };
            Ok(hyper::Response::from_parts(parts, body))
        }
        Err(e) => {
            // 对外文案不泄露内部拓扑, 详情在日志
            error!(
                "file-server 分流代理上游 127.0.0.1:{port} 请求失败 \
                 (path {path_query}, service_type={service_type:?}): {e}"
            );
            Ok(bad_gateway("file-server upstream unavailable"))
        }
    }
}

/// PX-06: 解析 `Connection` 头声明的动态 hop-by-hop token（可多值、逗号分隔）。
pub(crate) fn connection_tokens(headers: &hyper::HeaderMap) -> Vec<String> {
    headers
        .get_all("connection")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// PX-06: 请求头是否可转发上游——固定 hop-by-hop 名单、`Connection` 声明的
/// 动态 token、本跳认证头（X-Proxy-Token）三者任一命中即滤除。
pub(crate) fn should_forward_request_header(name: &str, tokens: &[String]) -> bool {
    !is_hop_by_hop(name)
        && !tokens.iter().any(|token| token.eq_ignore_ascii_case(name))
        && !name.eq_ignore_ascii_case(PROXY_TOKEN_HEADER)
}

/// hop-by-hop header 集合（RFC 7231 §6.1 / 2616 §13.5.1）。
const HOP_BY_HOP: [&str; 7] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    // upgrade 也是 hop-by-hop：本代理不透传 WebSocket（当前 file-server 无 ws 路由），
    // 剥除可防"孤立 upgrade 头"到达上游引发歧义
    "upgrade",
];

/// 本跳准入令牌头（PX-06: 认证完成后从双向流量中移除, 不进上游/响应）。
const PROXY_TOKEN_HEADER: &str = "x-proxy-token";

/// PX-07 端点预算（D3 落地）: 有限响应 body 的空闲窗口——连续无数据字节
/// 超过该窗口即终止 body（防"响应头已发后 body 永久挂起"）; SSE 长流豁免。
const RESPONSE_BODY_IDLE_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);

/// PX-07: 有限 body 加空闲预算——连续 `idle` 无数据字节则以 io 错误终止流
/// （headers 已发, 只能终止/标记 body 并在日志诊断, 不伪造另一个 502）。
/// PX-07: 有限 body 加空闲预算——连续 `idle` 无数据帧则以 io 错误终止流
/// （headers 已发, 只能终止/标记 body 并在日志诊断, 不伪造另一个 502）。
/// SSE（text/event-stream）长流豁免, 不经本包装。
fn body_with_idle_budget(
    body: Incoming,
    path_query: String,
    idle: std::time::Duration,
) -> ProxyBody {
    Box::pin(IdleBudgetBody {
        inner: body,
        sleep: None,
        idle,
        path_query,
    })
}

/// [`body_with_idle_budget`] 的 body 适配器（全字段 Unpin, 结构性 pin 由
/// Box 承担）。Pending 期间 arm 计时; 每个数据帧重置; 超窗以 TimedOut 错误
/// 终止 body。
struct IdleBudgetBody {
    inner: Incoming,
    sleep: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    idle: std::time::Duration,
    path_query: String,
}

impl http_body::Body for IdleBudgetBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();
        let mut inner = std::pin::pin!(&mut this.inner);
        match inner.as_mut().poll_frame(cx) {
            ready @ std::task::Poll::Ready(_) => {
                if let std::task::Poll::Ready(Some(Ok(_))) = &ready {
                    // 数据帧到达: 重置空闲窗口
                    match &mut this.sleep {
                        Some(sleep) => sleep
                            .as_mut()
                            .reset(tokio::time::Instant::now() + this.idle),
                        None => {
                            this.sleep = Some(Box::pin(tokio::time::sleep(this.idle)));
                        }
                    }
                }
                ready.map(|option| option.map(|result| result.map_err(std::io::Error::other)))
            }
            std::task::Poll::Pending => {
                let sleep = this
                    .sleep
                    .get_or_insert_with(|| Box::pin(tokio::time::sleep(this.idle)));
                if sleep.as_mut().poll(cx).is_ready() {
                    error!(
                        "file-server 分流代理响应 body 空闲超预算终止 \
                         (idle {:?}, path {})",
                        this.idle, this.path_query
                    );
                    return std::task::Poll::Ready(Some(Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "upstream response body idle budget exceeded",
                    ))));
                }
                std::task::Poll::Pending
            }
        }
    }
}

/// AllRust 模式的 60000 入口白名单：file-server 语义路径（`/api/*`、`/health`、`/`、
/// swagger `/api-docs*`）。TsFirst 的 rust 分支无需白名单——其判据本身已窄面。
pub(crate) fn all_rust_path_allowed(path: &str) -> bool {
    path == "/health" || path == "/" || path.starts_with("/api/") || path.starts_with("/api-docs")
}

pub(crate) fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|h| name.eq_ignore_ascii_case(h))
}

/// 502 错误响应（body 归一到 ProxyBody；Full 的 error 为 Infallible，不可达分支）。
fn bad_gateway(msg: &str) -> hyper::Response<ProxyBody> {
    error_response(hyper::StatusCode::BAD_GATEWAY, msg)
}

/// 400 错误响应（异常 request-target 等）。
fn bad_request(msg: &str) -> hyper::Response<ProxyBody> {
    error_response(hyper::StatusCode::BAD_REQUEST, msg)
}

/// 404：AllRust 白名单外的路径（此入口不服务该路径——不放行 8086 全量路由面）。
fn error_body(msg: &str) -> ProxyBody {
    Box::pin(http_body_util::Full::new(Bytes::from(msg.to_string())).map_err(|e| match e {}))
}

fn unauthorized() -> hyper::Response<ProxyBody> {
    hyper::Response::builder()
        .status(hyper::StatusCode::UNAUTHORIZED)
        .body(error_body("unauthorized"))
        .unwrap_or_else(|_| unreachable!("static 401 response"))
}

fn not_found(msg: &str) -> hyper::Response<ProxyBody> {
    error_response(hyper::StatusCode::NOT_FOUND, msg)
}

fn error_response(status: hyper::StatusCode, msg: &str) -> hyper::Response<ProxyBody> {
    let body: ProxyBody =
        Box::pin(http_body_util::Full::new(Bytes::from(msg.to_string())).map_err(|e| match e {}));
    hyper::Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(body)
        .unwrap_or_else(|_| unreachable!("static error response"))
}

#[cfg(test)]
mod header_filter_tests {
    use super::*;

    /// PX-06 反例: Connection 声明的动态 token 与固定 hop-by-hop、本跳认证头
    /// 都必须被滤除（修复前仅固定名单——Connection: x-local 时 X-Local 跨跳
    /// 转发、X-Proxy-Token 进入上游）; 未声明的业务/多值语义头保持转发。
    #[test]
    fn dynamic_connection_tokens_and_hop_credentials_are_filtered() {
        let mut headers = hyper::HeaderMap::new();
        headers.append(
            "connection",
            hyper::header::HeaderValue::from_static("x-local, X-Other"),
        );
        headers.append(
            "connection",
            hyper::header::HeaderValue::from_static("keep-alive"),
        );
        let tokens = connection_tokens(&headers);
        assert_eq!(tokens, ["x-local", "X-Other", "keep-alive"]);

        assert!(
            !should_forward_request_header("x-local", &tokens),
            "动态声明的 token 不得转发"
        );
        assert!(
            !should_forward_request_header("X-LOCAL", &tokens),
            "token 匹配大小写不敏感"
        );
        assert!(
            !should_forward_request_header("X-Proxy-Token", &[]),
            "本跳凭据不进上游"
        );
        assert!(
            !should_forward_request_header("connection", &[]),
            "固定名单"
        );
        assert!(!should_forward_request_header("te", &[]), "固定名单");
        assert!(
            should_forward_request_header("x-service-type", &tokens),
            "业务选择头保持"
        );
        assert!(
            should_forward_request_header("cookie", &tokens),
            "普通头保持"
        );
        assert!(
            should_forward_request_header("x-local-v2", &tokens),
            "仅前缀相同的头不受影响"
        );
    }
}
