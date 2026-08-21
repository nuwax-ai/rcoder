//! userApp 文件域透传层：rcoder 主服务（8086）→ per-app 开发容器内 file-server（60000）。
//!
//! 两类入口共用 [`forward_to_dev`]：
//! - `/api/userapp/{*rest}` 通配透传（Java 直调的新接口族）
//! - `/api/computer/*` 拦截层（反向代理转来的 TS 老路径 + `X-Service-Type: userapp`
//!   header 分流，路径原样 body 零解析——multipart 在代理层不可解，复杂度内聚于此）
//!
//! 透传语义：method/path/query/headers/body 全量流式转发（含 multipart 上传与 SSE
//! 日志流）；容器定位按 `X-App-Id` → UserAppBuilder（per-app，与 UserApp 运行容器
//! 经 ServiceType 隔离），miss 幂等 ensure；容器不在线 502。

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tracing::{info, warn};

use crate::router::AppState;
use crate::userapp_publish::agent_runner::{dev_file_server_addr, ensure_userapp_builder};

/// userApp 场景标记 header（反向代理/Java 注入；容器内 computer handler 消费做
/// workspace 切换）。值恒 `userapp`（与 /api/userapp 前缀对齐）。
pub const SERVICE_TYPE_HEADER: &str = "x-service-type";
pub const SERVICE_TYPE_USERAPP: &str = "userapp";

/// 开发容器定位 header（Java 调 8086 的所有 userApp 请求统一携带——单一 header
/// 定位，rcoder 零 body 解析，multipart/SSE 全覆盖）。
pub const APP_ID_HEADER: &str = "x-app-id";

/// 逐跳头：转发前剥离（reqwest/上游自行生成；host 逐跳重写）。
const HOP_BY_HOP: [&str; 6] = [
    "connection",
    "host",
    "content-length",
    "transfer-encoding",
    "keep-alive",
    "upgrade",
];

/// 解析 app_id header（None = 缺失/空，调用方返回 400 HttpResult）。
fn require_app_id(req: &Request) -> Option<String> {
    req.headers()
        .get(APP_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn missing_app_id_response() -> Response {
    HttpResultError::bad_request(format!(
        "missing required header `{APP_ID_HEADER}` for userApp forwarding"
    ))
    .into_response()
}

/// 定位（miss 幂等 ensure）开发容器 file-server addr。
async fn resolve_dev_addr(state: &AppState, app_id: &str) -> Result<String, Response> {
    let info = ensure_userapp_builder(state, app_id).await.map_err(|e| {
        warn!("[USERAPP_FORWARD] ensure dev container failed: app_id={app_id}: {e:#}");
        HttpResultError::bad_gateway(format!("dev container unavailable: {e:#}")).into_response()
    })?;
    Ok(dev_file_server_addr(state, &info))
}

/// 全量透传一个请求到该 app 开发容器的 file-server（同 path+query）。
///
/// body 走流（axum Body → reqwest stream），multipart 无需感知；响应同样流式
/// （bytes_stream → axum Body），SSE（tasks/{id}/logs/stream 等）天然支持。
pub(crate) async fn forward_to_dev(state: &AppState, app_id: &str, req: Request) -> Response {
    let addr = match resolve_dev_addr(state, app_id).await {
        Ok(addr) => addr,
        Err(resp) => return resp,
    };
    let target = format!("{addr}{}", req.uri());

    let (parts, body) = req.into_parts();
    let mut outbound = crate::http_client::shared_client().request(parts.method, &target);
    for (name, value) in &parts.headers {
        if HOP_BY_HOP.contains(&name.as_str()) {
            continue;
        }
        outbound = outbound.header(name, value);
    }
    let reqwest_body = reqwest::Body::wrap_stream(body.into_data_stream());
    outbound = outbound.body(reqwest_body);

    let upstream = match outbound.send().await {
        Ok(resp) => resp,
        Err(e) => {
            warn!(
                "[USERAPP_FORWARD] upstream request failed: app_id={app_id}, target={target}: {e}"
            );
            return HttpResultError::bad_gateway(format!("dev container request failed: {e}"))
                .into_response();
        }
    };

    let status = upstream.status();
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream.headers() {
        if HOP_BY_HOP.contains(&name.as_str()) {
            continue;
        }
        builder = builder.header(name, value);
    }
    match builder.body(Body::from_stream(upstream.bytes_stream())) {
        Ok(resp) => resp,
        Err(e) => HttpResultError::system(format!("build upstream response: {e}")).into_response(),
    }
}

/// `/api/userapp/{*rest}` 通配透传 handler。
pub(crate) async fn forward_userapp(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: Request,
) -> Response {
    let Some(app_id) = require_app_id(&req) else {
        return missing_app_id_response();
    };
    info!(
        "[USERAPP_FORWARD] {} {} -> dev container (app_id={app_id})",
        req.method(),
        req.uri().path()
    );
    forward_to_dev(&state, &app_id, req).await
}

/// `/api/computer/*` 拦截层：header `X-Service-Type: userapp` 即短路转发该 app
/// 开发容器**同路径**（TS 路径原样、body 零解析，header 随请求透传供容器内
/// computer handler 消费做 workspace 切换）；无该 header 落本地移植 handler。
pub(crate) async fn computer_intercept(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let is_userapp = req
        .headers()
        .get(SERVICE_TYPE_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim() == SERVICE_TYPE_USERAPP);
    if !is_userapp {
        return next.run(req).await;
    }
    let Some(app_id) = require_app_id(&req) else {
        return missing_app_id_response();
    };
    info!(
        "[USERAPP_FORWARD] intercepted computer request {} -> dev container (app_id={app_id})",
        req.uri().path()
    );
    forward_to_dev(&state, &app_id, req).await
}

// ── HttpResult 错误响应（透传层自身错误；上游业务响应原样透传不重包装） ──────────

struct HttpResultError {
    status: axum::http::StatusCode,
    message: String,
}

impl HttpResultError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: axum::http::StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn bad_gateway(message: impl Into<String>) -> Self {
        Self {
            status: axum::http::StatusCode::BAD_GATEWAY,
            message: message.into(),
        }
    }

    fn system(message: impl Into<String>) -> Self {
        Self {
            status: axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl IntoResponse for HttpResultError {
    fn into_response(self) -> Response {
        let payload = serde_json::json!({
            "code": self.status.as_u16(),
            "message": self.message,
            "data": serde_json::Value::Null,
            "success": false,
        });
        let mut resp = (self.status, axum::Json(payload)).into_response();
        resp.headers_mut().insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
        resp
    }
}
