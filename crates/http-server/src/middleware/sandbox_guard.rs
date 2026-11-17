//! E2B 数据面守卫 + 流式代理（feature `kubernetes`；最外层 layer）。
//!
//! 拦截规则：请求带 `E2b-Sandbox-Id` 头 → 短路进代理（不进入 rcoder 路由树，
//! 也不受全局 API key 中间件约束）；无头 → 原样放行。
//!
//! 代理前授权：沙箱存在、Ready、未到期、`X-Access-Token` 哈希匹配；
//! 目标只取受管沙箱 Pod 的 envd :49983（客户端不可指定地址，防 SSRF）。
//! 双向流式：请求体 axum stream → reqwest Body::wrap_stream；响应字节流回传
//! （Connect 服务端流/PTY 必需，禁用整体超时，仅连接超时）。
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderName, HeaderValue, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures::StreamExt;

use crate::router::sandbox::SandboxDeps;

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

pub async fn e2b_data_plane_guard(
    State(deps): State<SandboxDeps>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let Some(sandbox_id) = request
        .headers()
        .get("E2b-Sandbox-Id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
    else {
        return next.run(request).await;
    };
    let Some(manager) = deps.manager() else {
        return error_response(StatusCode::NOT_FOUND, "e2b sandbox 未启用");
    };
    // 端口头仅支持 envd（49983）；任意端口预览是后续能力。
    if let Some(port) = request.headers().get("E2b-Sandbox-Port").and_then(|value| value.to_str().ok()) {
        if port != "49983" {
            return error_response(
                StatusCode::NOT_IMPLEMENTED,
                "仅支持 envd 数据面（49983）",
            );
        }
    }
    let token = request
        .headers()
        .get("X-Access-Token")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .unwrap_or_default();
    let record = match manager.authorize_data_plane(&sandbox_id, &token).await {
        Ok(record) => record,
        Err(_) => {
            return error_response(StatusCode::UNAUTHORIZED, "沙箱不存在、已到期或 token 无效")
        }
    };
    let pod_ip = match manager.pod_ip(&record).await {
        Ok(ip) => ip,
        Err(_) => return error_response(StatusCode::NOT_FOUND, "沙箱 Pod 不可达"),
    };

    let (parts, body) = request.into_parts();
    let url = format!(
        "http://{pod_ip}:{}{}",
        rcoder_engine::service::sandbox_manager::ENVD_PORT,
        parts.uri.path_and_query().map(|value| value.as_str()).unwrap_or("/")
    );
    let mut outgoing = proxy_client()
        .request(reqwest::Method::from_bytes(parts.method.as_str().as_bytes()).expect("method"), &url);
    for (name, value) in parts.headers.iter() {
        if HOP_BY_HOP.contains(&name.as_str()) || name.as_str() == "e2b-sandbox-id" || name.as_str() == "e2b-sandbox-port" {
            continue;
        }
        if let Ok(value) = HeaderValue::from_bytes(value.as_bytes()) {
            outgoing = outgoing.header(name.as_str(), value);
        }
    }
    // envd 侧鉴权：代理注入已校验的 token（客户端值与库内哈希一致）。
    outgoing = outgoing.header("X-Access-Token", token);
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
