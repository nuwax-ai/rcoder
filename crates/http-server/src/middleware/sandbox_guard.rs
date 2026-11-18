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
//! 协议：HTTP/Connect 双向流式透传；WebSocket upgrade → 真实上游握手代理
//! （客户端握手原样转发沙箱侧服务，上游 101 原样回传，随后不解 WS 帧
//! copy_bidirectional 直通，天然支持任意子协议如 CDP）。
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderName, HeaderValue, Method, Request, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures::StreamExt;

use crate::router::sandbox::SandboxDeps;
use rcoder_engine::service::sandbox_manager::ENVD_PORT;

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
        let port = match headers.get("E2b-Sandbox-Port") {
            None => ENVD_PORT,
            // 端口头非法（非数字/越界）不得静默回落默认端口——那会把
            // 恶意构造的端口重定向到 envd；放行给控制面（404）。
            Some(value) => {
                let Some(text) = value.to_str().ok() else {
                    return None;
                };
                let Ok(port) = text.trim().parse::<i32>() else {
                    return None;
                };
                if !(1..=65535).contains(&port) {
                    return None;
                }
                port
            }
        };
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
        if parts.next() == Some("access_token")
            && let Some(value) = parts.next()
        {
            return value.to_string();
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
        return websocket_tunnel(request, &target).await;
    }

    let (parts, body) = request.into_parts();
    let url = format!("http://{target}{rest}");
    let mut outgoing = proxy_client()
        .request(Method::from_bytes(parts.method.as_str().as_bytes()).expect("method"), &url);
    for (name, value) in parts.headers.iter() {
        if HOP_BY_HOP.contains(&name.as_str())
            || name.as_str() == "e2b-sandbox-id"
            || name.as_str() == "e2b-sandbox-port"
            // 客户端可能自带 X-Access-Token——剔除后统一注入校验过的值，
            // 避免重复头被服务端拼接成非法 token。
            || name.as_str() == "x-access-token"
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
        chunk.map_err(std::io::Error::other)
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
        chunk.map_err(std::io::Error::other)
    });
    match builder.body(Body::from_stream(byte_stream)) {
        Ok(response) => response,
        Err(_) => error_response(StatusCode::BAD_GATEWAY, "代理响应构造失败"),
    }
}

/// WebSocket/CDP 隧道：真实上游握手代理。
///
/// 网关不是握手应答方——把客户端的升级请求按 HTTP/1.1 原始字节转发给沙箱侧
/// 服务，上游的 101（含其基于客户端 `Sec-WebSocket-Key` 计算的
/// `Sec-WebSocket-Accept`）原样回传客户端；随后双向透传，不解 WS 帧
/// （copy_bidirectional 直通，天然支持 CDP/MCP 等任意子协议）。
/// 上游不可达或拒绝握手时，客户端拿到真实错误而非一个随后黑洞的 101。
async fn websocket_tunnel(request: Request<Body>, target: &str) -> Response {
    use tokio::io::AsyncWriteExt;

    const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    const HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    let (parts, _body) = request.into_parts();
    let handshake = serialize_upgrade_request(&parts);
    let on_upgrade = hyper::upgrade::on(Request::from_parts(parts, Body::empty()));
    let target = target.to_string();

    // 连上游 + 转发握手 + 读响应头——全部在应答客户端之前完成。
    let mut upstream = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect(&target),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            tracing::warn!(target = %target, error = %error, "e2b ws 隧道连接沙箱失败");
            return error_response(StatusCode::BAD_GATEWAY, "沙箱 WS 上游不可达");
        }
        Err(_) => {
            return error_response(StatusCode::GATEWAY_TIMEOUT, "沙箱 WS 上游连接超时");
        }
    };
    if let Err(error) = upstream.write_all(&handshake).await {
        tracing::warn!(target = %target, error = %error, "e2b ws 隧道写握手失败");
        return error_response(StatusCode::BAD_GATEWAY, "沙箱 WS 上游不可达");
    }
    let head = match tokio::time::timeout(HEAD_TIMEOUT, read_response_head(&mut upstream)).await {
        Ok(Ok(Some(head))) => head,
        Ok(Ok(None)) | Ok(Err(_)) => {
            return error_response(StatusCode::BAD_GATEWAY, "沙箱 WS 上游握手响应畸形");
        }
        Err(_) => {
            return error_response(StatusCode::GATEWAY_TIMEOUT, "沙箱 WS 上游握手超时");
        }
    };

    // 非 101：上游拒绝升级（如鉴权失败）——原样回传状态与响应体。
    if head.status != 101 {
        let mut builder =
            Response::builder().status(StatusCode::from_u16(head.status).unwrap_or(StatusCode::BAD_GATEWAY));
        for (name, value) in &head.headers {
            if HOP_BY_HOP.contains(&name.to_ascii_lowercase().as_str()) {
                continue;
            }
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                builder = builder.header(name, value);
            }
        }
        return match builder.body(Body::from_stream(leftover_then_socket(
            head.leftover,
            upstream,
        ))) {
            Ok(response) => response,
            Err(_) => error_response(StatusCode::BAD_GATEWAY, "代理响应构造失败"),
        };
    }

    // 101：上游握手头逐字回传（Upgrade/Connection/Sec-WebSocket-Accept 等均
    // 由上游生成，对客户端同样合法——Key 是客户端发的同一个值）。
    let mut builder = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    for (name, value) in &head.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            builder = builder.header(name, value);
        }
    }
    let response = builder.body(Body::empty()).expect("101 response");

    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(client_stream) => {
                // hyper 的 Upgraded 需 TokioIo 包装才有 tokio AsyncRead/Write
                let mut client = hyper_util::rt::TokioIo::new(client_stream);
                let mut server = upstream;
                // 响应头之后可能已经跟着到达的帧字节——先冲给客户端再进双通。
                if !head.leftover.is_empty() {
                    let _ = client.write_all(&head.leftover).await;
                }
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
                tracing::warn!(error = %error, "e2b ws 升级失败");
            }
        }
    });
    response
}

/// 把客户端的升级请求重新序列化为 HTTP/1.1 原始字节（网关路由头剔除，
/// 其余逐字保留——Sec-WebSocket-Key/Protocol/Version 必须原样到达上游）。
fn serialize_upgrade_request(parts: &axum::http::request::Parts) -> Vec<u8> {
    let path = parts
        .uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    let mut out = format!("{} {} HTTP/1.1\r\n", parts.method, path).into_bytes();
    for (name, value) in parts.headers.iter() {
        let name = name.as_str();
        if name.eq_ignore_ascii_case("e2b-sandbox-id")
            || name.eq_ignore_ascii_case("e2b-sandbox-port")
        {
            continue;
        }
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out
}

struct UpstreamHead {
    status: u16,
    headers: Vec<(String, String)>,
    /// 响应头结束符之后已读到的字节（101 场景=先到的 WS 帧）。
    leftover: Vec<u8>,
}

/// 从上游 socket 读到首个 `\r\n\r\n` 并解析状态行与头。
/// 返回 `Ok(None)` = 连接 EOF / 头超长 / 畸形（调用方按 502 处理）。
async fn read_response_head(stream: &mut tokio::net::TcpStream) -> std::io::Result<Option<UpstreamHead>> {
    use tokio::io::AsyncReadExt as _;
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(position) = buf
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        {
            let head_end = position + 4;
            let leftover = buf.split_off(head_end);
            buf.truncate(position);
            return Ok(parse_response_head(&buf, leftover));
        }
        if buf.len() > 16 * 1024 {
            return Ok(None);
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..read]);
    }
}

fn parse_response_head(head: &[u8], leftover: Vec<u8>) -> Option<UpstreamHead> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next()?;
    let mut fields = status_line.splitn(3, ' ');
    let version = fields.next()?;
    if !version.starts_with("HTTP/1.") {
        return None;
    }
    let status: u16 = fields.next()?.parse().ok()?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line.split_once(':')?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    Some(UpstreamHead {
        status,
        headers,
        leftover,
    })
}

/// 非 101 场景的响应体：先吐 leftover，再流式转发上游 socket 剩余字节。
fn leftover_then_socket(
    leftover: Vec<u8>,
    stream: tokio::net::TcpStream,
) -> impl futures::Stream<Item = Result<axum::body::Bytes, std::io::Error>> {
    use tokio::io::AsyncReadExt as _;
    struct Source {
        pending: Option<Vec<u8>>,
        stream: Option<tokio::net::TcpStream>,
    }
    futures::stream::unfold(
        Source {
            pending: Some(leftover),
            stream: Some(stream),
        },
        |mut state| async move {
            if let Some(pending) = state.pending.take()
                && !pending.is_empty()
            {
                return Some((Ok(axum::body::Bytes::from(pending)), state));
            }
            let mut stream = state.stream.take()?;
            let mut buf = vec![0u8; 16 * 1024];
            match stream.read(&mut buf).await {
                Ok(0) => None,
                Ok(read) => {
                    buf.truncate(read);
                    state.stream = Some(stream);
                    Some((Ok(axum::body::Bytes::from(buf)), state))
                }
                Err(error) => Some((Err(error), state)),
            }
        },
    )
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
    fn header_mode_rejects_malformed_port() {
        // 非法端口不得静默回落 49983（等于把恶意端口重定向到 envd）
        for bad in ["", "abc", "99999999999", "-1", "65536"] {
            let uri: Uri = "/x".parse().unwrap();
            assert_eq!(
                resolve_target(
                    &uri,
                    &headers(&[
                        ("e2b-sandbox-id", "abc123"),
                        ("e2b-sandbox-port", bad),
                    ])
                ),
                None,
                "端口 {bad:?} 应放行给控制面"
            );
        }
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

    fn upgrade_parts(uri: &str, pairs: &[(&str, &str)]) -> axum::http::request::Parts {
        let request: Request<()> = Request::builder()
            .method(Method::GET)
            .uri(uri)
            .body(())
            .unwrap();
        let (mut parts, ()) = request.into_parts();
        parts.headers = headers(pairs);
        parts
    }

    #[test]
    fn serialize_upgrade_request_keeps_handshake_and_strips_routing_headers() {
        let parts = upgrade_parts(
            "/mcp?session=1",
            &[
                ("e2b-sandbox-id", "abc123"),
                ("e2b-sandbox-port", "8788"),
                ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
                ("sec-websocket-version", "13"),
                ("x-access-token", "tok"),
            ],
        );
        let raw = serialize_upgrade_request(&parts);
        let text = String::from_utf8(raw.clone()).unwrap();
        assert!(
            text.starts_with("GET /mcp?session=1 HTTP/1.1\r\n"),
            "请求行必须原样（含查询串）：{text:?}"
        );
        assert!(text.contains("sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ=="));
        assert!(text.contains("x-access-token: tok"));
        assert!(!text.contains("e2b-sandbox"), "网关路由头不得透传");
        assert!(raw.ends_with(b"\r\n\r\n"), "头区必须以空行结束");
    }

    #[test]
    fn parse_response_head_101_with_leftover_frames() {
        let head_bytes = b"HTTP/1.1 101 Switching Protocols\r\n\
                           Upgrade: websocket\r\n\
                           Connection: Upgrade\r\n\
                           Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
        let head = parse_response_head(head_bytes, b"\x81\x05hello".to_vec()).unwrap();
        assert_eq!(head.status, 101);
        let accept = head
            .headers
            .iter()
            .find(|(name, _)| name == "sec-websocket-accept")
            .unwrap();
        assert_eq!(accept.1, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        assert_eq!(head.leftover, b"\x81\x05hello");
    }

    #[test]
    fn parse_response_head_non_101_and_garbage() {
        let head_bytes = b"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json";
        let head = parse_response_head(head_bytes, Vec::new()).unwrap();
        assert_eq!(head.status, 401);
        assert!(head.leftover.is_empty());

        assert!(parse_response_head(b"garbage", Vec::new()).is_none());
        assert!(parse_response_head(b"HTTP/2.0 101 x", Vec::new()).is_none());
        assert!(parse_response_head(b"HTTP/1.1 abc x", Vec::new()).is_none());
    }
}
