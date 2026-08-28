//! userApp 透传层的上游定位与转发内核。
//!
//! - 定位契约：dev=注册表 + 探活自愈（30s 正缓存；脏值清 container 字段重建，
//!   不 remove_project 保 PG 会话映射）；prod=存在性检查 + 唤醒（stopped 自动
//!   拉起，503+Retry-After）+ 确定性命名/容器 IPv4
//! - 转发内核：method/path/query/headers/body 全量流式（multipart/SSE 天然
//!   支持），hop-by-hop 头按 RFC 9110 剥离（静态表 ∪ Connection 动态列举）
//! - 容器定位按 `X-App-Id` header（白名单校验）；容器不在线 502（dev）/
//!   503+Retry-After（prod 唤醒失败）

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use tracing::{info, warn};

use shared_types::APP_ID_HEADER;

use crate::router::AppState;
use crate::userapp_builder::{dev_file_server_addr, ensure_userapp_builder};

use super::semantics::HttpResultError;

/// userApp 透传面 API 前缀（运行时路径分派与透传清单共用词根）。
pub(super) const USERAPP_API_PREFIX: &str = "/api/v1/userapp";
/// tasks 族路径前缀（query app_id 定位 + 短路语义识别共用）。
pub(super) const TASKS_PATH_PREFIX: &str = "/api/v1/userapp/tasks/";
/// static 族路径前缀（构建链制品下载，path 段 app_id 定位）。
pub(super) const STATIC_PATH_PREFIX: &str = "/api/v1/userapp/static/";

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
    HOP_BY_HOP.contains(&name.to_ascii_lowercase().as_str())
        || connection_listed
            .iter()
            .any(|listed| listed.eq_ignore_ascii_case(name))
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
pub(super) fn require_app_id(req: &Request) -> Option<String> {
    let raw = req
        .headers()
        .get(APP_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    shared_types::validate_identifier(raw, "app_id").ok()?;
    Some(raw.to_owned())
}

pub(super) fn missing_app_id_response() -> Response {
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
async fn resolve_dev_addr(
    state: &AppState,
    app_id: &str,
    explicit_user_id: Option<&str>,
) -> Result<String, Box<Response>> {
    let mut info = ensure_userapp_builder(state, app_id, explicit_user_id)
        .await
        .map_err(|e| {
            warn!("[USERAPP_FORWARD] ensure dev container failed: app_id={app_id}: {e:#}");
            HttpResultError::bad_gateway(format!("dev container unavailable: {e:#}"))
                .into_boxed_response()
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
        info = ensure_userapp_builder(state, app_id, explicit_user_id)
            .await
            .map_err(|e| {
                warn!("[USERAPP_FORWARD] re-ensure dev container failed: app_id={app_id}: {e:#}");
                HttpResultError::bad_gateway(format!("dev container unavailable: {e:#}"))
                    .into_boxed_response()
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

async fn forward_to_addr(target_label: &str, app_id: &str, addr: &str, req: Request) -> Response {
    let target = format!("{addr}{}", req.uri());

    let (parts, body) = req.into_parts();
    let listed = connection_listed_tokens(&parts.headers);
    // 循环外一次构造引用视图（原先每个 header 重建一次 Vec）
    let listed_refs: Vec<&str> = listed.iter().map(String::as_str).collect();
    let mut outbound = crate::http_client::shared_client().request(parts.method, &target);
    for (name, value) in &parts.headers {
        if is_hop_by_hop(name.as_str(), &listed_refs) {
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
            return HttpResultError::bad_gateway(format!(
                "{target_label} container request failed: {e}"
            ))
            .into_response();
        }
    };

    let status = upstream.status();
    let resp_listed = connection_listed_tokens(upstream.headers());
    let resp_listed_refs: Vec<&str> = resp_listed.iter().map(String::as_str).collect();
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream.headers() {
        if is_hop_by_hop(name.as_str(), &resp_listed_refs) {
            continue;
        }
        builder = builder.header(name, value);
    }
    match builder.body(Body::from_stream(upstream.bytes_stream())) {
        Ok(resp) => resp,
        Err(e) => HttpResultError::system(format!("build upstream response: {e}")).into_response(),
    }
}

/// 全量透传一个请求到该 app 开发容器的 file-server（同 path+query）。
///
/// `explicit_user_id`：请求入参显式携带的 owner（懒创建容器的宿主树分区
/// 显式档；透传族 body 内 user_id 流式不解析，仅 query 可见的接口传值）。
pub(crate) async fn forward_to_dev(
    state: &AppState,
    app_id: &str,
    req: Request,
    explicit_user_id: Option<&str>,
) -> Response {
    let addr = match resolve_dev_addr(state, app_id, explicit_user_id).await {
        Ok(addr) => addr,
        Err(resp) => return *resp,
    };
    forward_to_addr("dev", app_id, &addr, req).await
}

/// 全量透传一个请求到该 app 生产运行容器的 file-server-proxy（同 path+query）。
///
/// 定位语义（与 pod ensure prod 分支同款）：
/// 1. 存在性检查（`get_app`）——`ensure_running` 对不存在的 app 返回 AlreadyRunning
///    （stopped-set 语义），必须前置拦截防幻报；
/// 2. 唤醒——闲置回收（scale 0）的 app 自动拉起（用户拍板：文件操作前容器没启动
///    要自动启动）；Timeout/Failed → 503 + Retry-After（对齐 proxy_http 流量唤醒）；
/// 3. 地址——K8s 确定性命名 FQDN（Service 换 Pod DNS 自愈）；Docker 直查容器 IPv4
///    （容器名 DNS 可能返回 AAAA 而容器内 file-server 只 bind IPv4）。
///
/// 唤醒后立刻转发可能与容器内 file-server 启动赛跑（connect refused → 502）——
/// 客户端重试/下一请求即恢复，不做二次等待。
pub(crate) async fn forward_to_prod(state: &AppState, app_id: &str, req: Request) -> Response {
    let addr = match resolve_prod_addr(state, app_id).await {
        Ok(addr) => addr,
        Err(resp) => return *resp,
    };
    forward_to_addr("prod runtime", app_id, &addr, req).await
}

/// 定位（含唤醒）生产运行容器 file-server addr（`http://{host}:60000`）。
async fn resolve_prod_addr(state: &AppState, app_id: &str) -> Result<String, Box<Response>> {
    // 幻报拦截：不存在的 app 直接 404（pod ensure prod 同款语义）
    if let Err(e) = state.app_service.get_app(app_id).await {
        info!(
            "[USERAPP_FORWARD] prod forward target check failed (treated as not found): app_id={app_id}: {e}"
        );
        return Err(Box::new(
            HttpResultError::not_found(format!(
                "userapp prod app not found or unavailable: {app_id}"
            ))
            .into_response(),
        ));
    }
    // 唤醒（仅 stopped 真时触发——Running 高频文件操作零开销）
    use shared_types::AppWakeControl;
    if state.activity.is_stopped(app_id) {
        match state.activity.ensure_running(app_id).await {
            shared_types::WakeOutcome::Ready | shared_types::WakeOutcome::AlreadyRunning => {}
            shared_types::WakeOutcome::Timeout => {
                warn!("[USERAPP_FORWARD] prod wake timeout: app_id={app_id}");
                return Err(Box::new(
                    HttpResultError::service_unavailable(
                        format!("app {app_id} wake timed out; retry later"),
                        WAKE_503_RETRY_AFTER_SECS,
                    )
                    .into_response(),
                ));
            }
            shared_types::WakeOutcome::Failed(e) => {
                warn!("[USERAPP_FORWARD] prod wake failed: app_id={app_id}: {e}");
                return Err(Box::new(
                    HttpResultError::service_unavailable(
                        format!("app {app_id} wake failed: {e}"),
                        WAKE_503_RETRY_AFTER_SECS,
                    )
                    .into_response(),
                ));
            }
        }
    }
    // 地址解析
    let host = if shared_types::is_kubernetes_runtime() {
        use shared_types::ContainerLookup;
        state.projects.find_app_runtime_addr(app_id)
    } else {
        // Docker：直查容器 IPv4（同 pod restart 的 UserApp 定位模式）
        state
            .runtime()
            .get_container_info_by_identifier(app_id, &shared_types::ServiceType::UserApp)
            .await
            .ok()
            .flatten()
            .map(|info| info.container_ip)
            .filter(|ip| !ip.is_empty())
    };
    match host.filter(|h| !h.is_empty()) {
        Some(host) => Ok(format!(
            "http://{host}:{}",
            shared_types::AGENT_FILE_SERVER_PORT
        )),
        None => {
            // 走到这里 = get_app 成功但容器定位失败（回收过渡态等）
            warn!("[USERAPP_FORWARD] prod runtime addr unavailable: app_id={app_id}");
            Err(Box::new(
                HttpResultError::not_found(format!("runtime address for app {app_id} unavailable"))
                    .into_response(),
            ))
        }
    }
}

/// 唤醒 503 的 Retry-After 秒数（对齐 proxy_http 流量唤醒面）。
const WAKE_503_RETRY_AFTER_SECS: u32 = 15;
