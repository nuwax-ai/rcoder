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

// 分流契约常量（X-Service-Type / X-App-Id）定义在 shared_types（rcoder 转发层
// 与容器内 file-server 共用的单一事实源）。
pub use shared_types::{APP_ID_HEADER, SERVICE_TYPE_HEADER, SERVICE_TYPE_USERAPP};

/// 逐跳头静态表：转发前剥离（reqwest/上游自行生成；host 逐跳重写）。
const HOP_BY_HOP: [&str; 10] = [
    "connection",
    "host",
    "content-length",
    "transfer-encoding",
    "keep-alive",
    "upgrade",
    "te",
    "trailer",
    "proxy-authenticate",
    "proxy-authorization",
];

/// 判定请求/响应头是否逐跳剥离：静态表 ∪ `Connection` 头动态列出的头
/// （RFC 9110 §7.6.1：`Connection: X-Foo` 则 X-Foo 亦是逐跳——静态表无法穷尽）。
fn is_hop_by_hop(name: &str, connection_listed: &[&str]) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&lower.as_str()) || connection_listed.contains(&lower.as_str())
}

/// 从 headers 提取 Connection 头动态声明的逐跳头名列表（小写化）。
fn connection_listed_tokens(headers: &axum::http::HeaderMap) -> Vec<String> {
    headers
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(|t| t.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

/// 解析并校验 app_id header（None = 缺失/空/非法，调用方返回 400 HttpResult）。
///
/// identifier 白名单必做：`computer_intercept` 挂在无鉴权的 file-server 路由面
/// （与 TS 一致性设计），app_id 原样进入容器标识与 Docker bind 宿主路径拼接
/// （`host_root.join(app_id)`），含 `/` 即逃逸开发卷根把宿主任意目录挂进容器。
fn require_app_id(req: &Request) -> Option<String> {
    let raw = req
        .headers()
        .get(APP_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    shared_types::validate_identifier(raw, "app_id").ok()?;
    Some(raw.to_owned())
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
        // 就地清 container 字段而非 remove_project：remove 在 PG 模式会持久化删除
        // project 行及其 sessions（刚 durable 写入的会话映射全丢、跨副本路由失效），
        // 且需先关 SSE 流避免后台 gRPC 对死地址空转——探活仅 3s 超时单次判定，
        // 高负载抖动即触发，破坏性过大。清 container 让 ensure 走重建路径即可。
        state.shutdown_sse_streams_for_project(app_id);
        if let Some(mut info) = state.get_project(app_id).map(|p| (*p).clone()) {
            info.set_container(None);
            if let Err(e) = state.insert_project(app_id.to_string(), Arc::new(info)) {
                warn!("[USERAPP_FORWARD] clear stale container field failed: app_id={app_id}: {e}");
            }
        }
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

/// 摘除探活正缓存条目（app purge 后调用，防缓存内残留已删 app 的健康时刻）。
pub(crate) fn invalidate_probe_cache(app_id: &str) {
    if let Some(cache) = PROBE_OK.get() {
        cache.remove(app_id);
    }
}

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
    let listed = connection_listed_tokens(&parts.headers);
    let mut outbound = crate::http_client::shared_client().request(parts.method, &target);
    for (name, value) in &parts.headers {
        if is_hop_by_hop(
            name.as_str(),
            &listed.iter().map(String::as_str).collect::<Vec<_>>(),
        ) {
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
    let resp_listed = connection_listed_tokens(upstream.headers());
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream.headers() {
        if is_hop_by_hop(
            name.as_str(),
            &resp_listed.iter().map(String::as_str).collect::<Vec<_>>(),
        ) {
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
