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
///
/// 注册表脏值自愈：容器被外部删除（docker rm / 回收）后 state.projects 残留死 IP，
/// 且 ensure 被注册表命中挡住不会重建——转发前轻量探活（GET /api/version，3s 超时），
/// 失败则清注册重新 ensure（新容器新 IP），下一次请求即恢复。
async fn resolve_dev_addr(state: &AppState, app_id: &str) -> Result<String, Response> {
    let mut info = ensure_userapp_builder(state, app_id).await.map_err(|e| {
        warn!("[USERAPP_FORWARD] ensure dev container failed: app_id={app_id}: {e:#}");
        HttpResultError::bad_gateway(format!("dev container unavailable: {e:#}")).into_response()
    })?;
    let mut addr = dev_file_server_addr(state, &info);
    // 探活正缓存(30s): 每次转发都探活会给高频文件操作(批量列表/读写)平添一个
    // RTT; 成功后窗口内免探。失败路径(自愈重建)不受缓存影响; 窗口内死容器漏检
    // 可接受——send 失败仍会 502, 下一请求自愈。
    let cache = PROBE_OK.get_or_init(dashmap::DashMap::new);
    let probe_fresh = cache.get(app_id).is_some_and(|t| t.elapsed() < PROBE_TTL);
    if !probe_fresh && !probe_dev_container(&addr).await {
        warn!(
            "[USERAPP_FORWARD] dev container probe failed (stale registry entry?), recreating: app_id={app_id}, addr={addr}"
        );
        state.remove_project(app_id);
        info = ensure_userapp_builder(state, app_id).await.map_err(|e| {
            warn!("[USERAPP_FORWARD] re-ensure dev container failed: app_id={app_id}: {e:#}");
            HttpResultError::bad_gateway(format!("dev container unavailable: {e:#}"))
                .into_response()
        })?;
        addr = dev_file_server_addr(state, &info);
        // 重建的新容器可能仍在启动(agent_runner+file-server+PG 全套)——不写探活
        // 缓存, 由本次 send 定成败; 下一请求重新探活
        return Ok(addr);
    }
    if !probe_fresh {
        cache.insert(app_id.to_string(), std::time::Instant::now());
    }
    Ok(addr)
}

/// 探活正缓存: app_id → 最近一次探活成功时刻(重建自愈后刷新)。
static PROBE_OK: std::sync::OnceLock<dashmap::DashMap<String, std::time::Instant>> =
    std::sync::OnceLock::new();
const PROBE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// 开发容器 file-server 轻量探活（连接失败/非 2xx 均视为不可用）。
async fn probe_dev_container(addr: &str) -> bool {
    crate::http_client::shared_client()
        .get(format!("{addr}/api/version"))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
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
        // 与 shared_types::HttpResult 同形态(code=字符串错误码/message/data/tid/success),
        // 但保留真实 HTTP 状态码(400/502 对代理与客户端有语义; HttpResult 的
        // IntoResponse 恒 200, 不适用于透传层的传输级错误)
        let payload = serde_json::json!({
            "code": error_code_for(self.status),
            "message": self.message,
            "data": serde_json::Value::Null,
            "success": false,
        });
        (self.status, axum::Json(payload)).into_response()
    }
}

/// HTTP 状态码 → 全站字符串错误码(对齐 shared_types::error_codes 词表)。
fn error_code_for(status: axum::http::StatusCode) -> &'static str {
    match status {
        axum::http::StatusCode::BAD_REQUEST => shared_types::error_codes::ERR_VALIDATION,
        axum::http::StatusCode::BAD_GATEWAY => shared_types::error_codes::ERR_BACKEND_ERROR,
        _ => shared_types::error_codes::ERR_INTERNAL_SERVER_ERROR,
    }
}
