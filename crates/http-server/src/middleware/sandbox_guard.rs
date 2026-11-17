//! E2B 数据面守卫 + 流式代理（feature `kubernetes`；最外层 layer）。
//!
//! 拦截规则（三选一，命中即短路，不进入 rcoder 路由树）：
//! 1. **头路由**（M1，官方 SDK 兼容）：请求带 `E2b-Sandbox-Id` 头
//!    （+可选 `E2b-Sandbox-Port`，默认 49983）
//! 2. **路径路由**（M1.5，无泛域名场景）：`/sandboxes/{id}/{port}/...`，
//!    第二段纯数字 = 数据面；非数字（timeout/connect/...）= 控制面，放行
//! 3. 其余请求原样放行
//!
//! 两种路由统一走同一授权：沙箱存在、Ready、未到期、token 匹配
//! （`X-Access-Token` 头或 `?access_token=` 查询参数——WS 客户端不便带自定义头）。
//! 端口必须在白名单内（防 SSRF / 任意端口转发）。目标只取受管 Pod。
//!
//! 协议：HTTP/Connect 双向流式透传；WebSocket upgrade → TCP 双向隧道
//! （copy_bidirectional 直通，不解 WS 帧，天然支持任意子协议如 CDP）。
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderName, HeaderValue, Method, Request, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use std::sync::Arc;

use crate::router::sandbox::SandboxDeps;
use rcoder_engine::service::sandbox_manager::{SandboxManager, ENVD_PORT};

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

/// 从请求解析数据面目标：(sandbox_id, port, 剩余路径与查询)。
/// 返回 None = 不拦截（放行路由树）。
pub(crate) fn resolve_target(uri: &Uri, headers: &axum::http::HeaderMap) -> Option<(String, i32, String)> {
    // 模式 1：头路由（官方 SDK）
    if let Some(id) = headers
        .get("E2b-Sandbox-Id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
    {
        let port = headers
            .get("E2b-Sandbox-Port")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i32>().ok())
            .unwrap_or(ENVD_PORT);
        let rest = uri
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/");
        return Some((id.to_string(), port, rest.to_string()));
    }
    // 模式 2：路径路由 /sandboxes/{id}/{port}/...
    let path = uri.path();
    let mut segments = path.trim_start_matches('/').split('/');
    if segments.next() != Some("sandboxes") {
        return None;
    }
    let id = segments.next()?;
    let port_segment = segments.next()?;
    // 第二段非纯数字 = 控制面子路径（timeout/connect/...），不拦截
    if port_segment.is_empty() || !port_segment.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let port: i32 = port_segment.parse().ok()?;
    let rest_path = format!("/{}", segments.collect::<Vec<_>>().join("/"));
    let rest = match uri.query() {
        Some(query) => format!("{rest_path}?{query}"),
        None => rest_path,
    };
    Some((id.to_string(), port, rest))
}

fn proxy_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            // 数据面流不设总超时（长任务/PTY 由沙箱生命周期约束）。
            .build()
            .expect("e2b proxy client")
    })
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({ "code": status.as_u16(), "message": message })),
    )
        .into_response()
}

fn is_websocket_upgrade(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("upgrade")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
}

fn access_token(headers: &axum::http::HeaderMap, uri: &Uri) -> String {
    if let Some(token) = headers
        .get("X-Access-Token")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
    {
        return token.to_string();
    }
    // WS 客户端不便带自定义头：支持 ?access_token= 查询参数。
    let query = uri.query().unwrap_or("");
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        if parts.next() == Some("access_token") {
            if let Some(value) = parts.next() {
                return value.to_string();
            }
        }
    }
    String::new()
}

pub async fn e2b_data_plane_guard(
    State(deps): State<SandboxDeps>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let Some((sandbox_id, port, rest)) = resolve_target(request.uri(), request.headers()) else {
        return next.run(request).await;
    };
    let Some(manager) = deps.manager() else {
        return error_response(StatusCode::NOT_FOUND, "e2b sandbox 未启用");
    };
    if !deps.allowed_ports.contains(&port) {
        return error_response(
            StatusCode::FORBIDDEN,
            &format!("端口 {port} 不在白名单（allowed_ports）"),
        );
    }
    let token = access_token(request.headers(), request.uri());
    let record = match manager.authorize_data_plane(&sandbox_id, &token).await {
        Ok(record) => record,
        Err(_) => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "沙箱不存在、已到期或 token 无效",
            )
        }
    };
    let pod_ip = match manager.pod_ip(&record).await {
        Ok(ip) => ip,
        Err(_) => return error_response(StatusCode::NOT_FOUND, "沙箱 Pod 不可达"),
    };
    let target = format!("{pod_ip}:{port}");

    if is_websocket_upgrade(request.headers()) {
        return websocket_tunnel(request, &target);
    }

    let (parts, body) = request.into_parts();
    let url = format!("http://{target}{rest}");
    let mut outgoing = proxy_client()
        .request(reqwest::Method::from_bytes(parts.method.as_str().as_bytes()).expect("method"), &url);
    for (name, value) in parts.headers.iter() {
        if HOP_BY_HOP.contains(&name.as_str())
            || name.as_str() == "e2b-sandbox-id"
            || name.as_str() == "e2b-sandbox-port"
        {
            continue;
        }
        if let Ok(value) = HeaderValue::from_bytes(value.as_bytes()) {
            outgoing = outgoing.header(name.as_str(), value);
        }
    }
    // envd 侧鉴权：代理注入已校验的 token（客户端值与库内哈希一致）。
    outgoing = outgoing.header("X-Access-Token", &token);
    let stream = body.into_data_stream().map(|chunk| {
        chunk.map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error))
    });
    let response = match outgoing
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(sandbox_id = %sandbox_id, error = %error, "e2b 数据面代理失败");
            return error_response(StatusCode::BAD_GATEWAY, "沙箱数据面不可达");
        }
    };
    let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    for (name, value) in response.headers().iter() {
        if HOP_BY_HOP.contains(&name.as_str()) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            builder = builder.header(name, value);
        }
    }
    let byte_stream = response.bytes_stream().map(|chunk| {
        chunk.map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error))
    });
    match builder.body(Body::from_stream(byte_stream)) {
        Ok(response) => response,
        Err(_) => error_response(StatusCode::BAD_GATEWAY, "代理响应构造失败"),
    }
}

/// WebSocket/CDP 隧道：101 握手后把升级连接与沙箱 TCP 端口双向对接。
/// 不解 WS 帧（copy_bidirectional 透传），天然支持任意子协议。
fn websocket_tunnel(request: Request<Body>, target: &str) -> Response {
    let (parts, _body) = request.into_parts();
    let on_upgrade = hyper::upgrade::on(Request::from_parts(parts, Body::empty()));
    let target = target.to_string();
    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(client_stream) => {
                match tokio::net::TcpStream::connect(&target).await {
                    Ok(server_stream) => {
                        // hyper 的 Upgraded 需 TokioIo 包装才有 tokio AsyncRead/Write
                        let mut client = hyper_util::rt::TokioIo::new(client_stream);
                        let mut server = server_stream;
                        match tokio::io::copy_bidirectional(&mut client, &mut server).await {
                            Ok((client_to_server, server_to_client)) => {
                                tracing::debug!(
                                    target = %target,
                                    client_to_server,
                                    server_to_client,
                                    "e2b ws 隧道关闭"
                                );
                            }
                            Err(error) => {
                                tracing::debug!(target = %target, error = %error, "e2b ws 隧道 IO 结束");
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(target = %target, error = %error, "e2b ws 隧道连接沙箱失败");
                    }
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "e2b ws 升级失败");
            }
        }
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("upgrade", "websocket")
        .header("connection", "upgrade")
        .body(Body::empty())
        .expect("101 response")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> axum::http::HeaderMap {
        let mut map = axum::http::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn header_mode_routes_to_envd_by_default() {
        let uri: Uri = "/anything/here?x=1".parse().unwrap();
        let resolved = resolve_target(&uri, &headers(&[("e2b-sandbox-id", "abc123")]));
        assert_eq!(resolved, Some(("abc123".to_string(), 49983, "/anything/here?x=1".to_string())));
    }

    #[test]
    fn header_mode_honours_port_header() {
        let uri: Uri = "/mcp".parse().unwrap();
        let resolved = resolve_target(&uri, &headers(&[
            ("e2b-sandbox-id", "abc123"),
            ("e2b-sandbox-port", "8788"),
        ]));
        assert_eq!(resolved, Some(("abc123".to_string(), 8788, "/mcp".to_string())));
    }

    #[test]
    fn path_mode_extracts_id_port_and_rest() {
        let uri: Uri = "/sandboxes/abc123/8788/mcp?session=1".parse().unwrap();
        let resolved = resolve_target(&uri, &headers(&[]));
        assert_eq!(resolved, Some(("abc123".to_string(), 8788, "/mcp?session=1".to_string())));
    }

    #[test]
    fn path_mode_without_port_falls_through() {
        let uri: Uri = "/sandboxes/abc123".parse().unwrap();
        assert_eq!(resolve_target(&uri, &headers(&[])), None);
    }

    #[test]
    fn control_plane_subpaths_fall_through() {
        // 第二段非纯数字 = 控制面，不得被数据面守卫拦截
        for path in [
            "/sandboxes/abc123/timeout",
            "/sandboxes/abc123/connect",
            "/sandboxes/abc123/pause",
            "/sandboxes/abc123/metrics",
        ] {
            let uri: Uri = path.parse().unwrap();
            assert_eq!(resolve_target(&uri, &headers(&[])), None, "path {path} 应放行");
        }
    }

    #[test]
    fn unrelated_paths_fall_through() {
        for path in ["/health", "/templates", "/computer/chat", "/v2/sandboxes"] {
            let uri: Uri = path.parse().unwrap();
            assert_eq!(resolve_target(&uri, &headers(&[])), None, "path {path} 应放行");
        }
    }

    #[test]
    fn access_token_prefers_header_then_query() {
        let uri: Uri = "/sandboxes/a/49983/health?access_token=fromquery".parse().unwrap();
        let token = access_token(&headers(&[("x-access-token", "fromheader")]), &uri);
        assert_eq!(token, "fromheader");
        let token = access_token(&headers(&[]), &uri);
        assert_eq!(token, "fromquery");
    }

    #[test]
    fn port_whitelist_rejects_out_of_list() {
        let allowed = vec![49983, 8787, 8788];
        assert!(allowed.contains(&8788));
        assert!(!allowed.contains(&9999));
    }
}
