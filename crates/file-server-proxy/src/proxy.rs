//! hyper 转发核心：accept 循环、单请求代理、进程内直连、错误响应。

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::config::{FileServerProxyConfig, RoutePolicy, SERVICE_TYPE_HEADER, Upstream};

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

/// accept 循环 + 每连接 hyper 服务（shutdown 只停 accept，活跃连接随 task 结束断开）。
pub(crate) async fn serve(
    listener: tokio::net::TcpListener,
    client: ProxyClient,
    config: FileServerProxyConfig,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((io, _peer)) => {
                        let client = client.clone();
                        let config = config.clone();
                        tokio::spawn(async move {
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
                            if let Err(e) = conn.with_upgrades().await {
                                // 客户端中断/半关闭常见, debug 级即可
                                tracing::debug!("proxy connection ended: {e}");
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

    let path = parts.uri.path();
    let service_type = parts
        .headers
        .get(SERVICE_TYPE_HEADER)
        .and_then(|v| v.to_str().ok());
    let port = match config.upstream_port_for(path, service_type) {
        Upstream::Rust(port) => {
            // AllRust 白名单: 60000 只放行 file-server 语义路径, 不把上游 8086 的
            // 全量路由面（/chat、/agent-mgmt/* 等集群内面）推到对外入口
            if config.policy == RoutePolicy::AllRust && !all_rust_path_allowed(path) {
                warn!("AllRust 白名单外路径已拒绝: {path}");
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
        // hop-by-hop headers 不跨代理转发; append 保多值 header (Cookie 链等)
        if !is_hop_by_hop(name.as_str()) {
            headers.append(name.clone(), value.clone());
        }
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
            for h in HOP_BY_HOP {
                parts.headers.remove(h);
            }
            Ok(hyper::Response::from_parts(
                parts,
                Box::pin(body.map_err(std::io::Error::other)),
            ))
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

/// AllRust 模式的 60000 入口白名单：file-server 语义路径（`/api/*`、`/health`、`/`、
/// swagger `/api-docs*`）。UserappSplit 的 rust 分支无需白名单——其判据本身已窄面。
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
        .expect("static error response parts are valid")
}
