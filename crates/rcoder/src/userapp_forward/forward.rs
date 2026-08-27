//! userApp 文件域透传层：rcoder 主服务（8086）→ 目标容器内 file-server-proxy（60000）。
//!
//! **dev/prod 两阶段分派**（`X-App-Stage` header，缺省 dev——同一 app_id 可同时
//! 存在开发容器与生产 Deployment，必须显式区分）：
//! - `dev`：UserAppBuilder 开发容器（注册表定位 + 探活自愈，miss 幂等 ensure）
//! - `prod`：UserApp 生产运行容器（存在性检查 + 唤醒 + 确定性命名定位）
//!
//! 两类入口共用 [`forward_to_dev`]/[`forward_to_prod`]：
//! - `/api/v1/userapp/{*rest}` 通配透传（Java 直调的新接口族）
//! - `/api/computer/*` 拦截层（反向代理转来的 TS 老路径 + `X-Service-Type: userapp`
//!   header 分流，路径原样 body 零解析——multipart 在代理层不可解，复杂度内聚于此）
//!
//! 透传语义：method/path/query/headers/body 全量流式转发（含 multipart 上传与 SSE
//! 日志流）；容器定位按 `X-App-Id`，容器不在线 502（dev）/ 503+Retry-After（prod 唤醒失败）。

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::Uri;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tracing::{info, warn};

use shared_types::HttpResult;

use crate::router::AppState;
use crate::userapp_builder::{dev_file_server_addr, ensure_userapp_builder, registered_builder};

// 分流契约常量（X-Service-Type / X-App-Id / X-App-Stage）定义在 shared_types
// （rcoder 转发层与容器内 file-server 共用的单一事实源）。
use shared_types::UserappStage;
pub use shared_types::{
    APP_ID_HEADER, APP_STAGE_DEV, APP_STAGE_HEADER, APP_STAGE_PROD, SERVICE_TYPE_HEADER,
};

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

// ── 容器懒启动语义分派（容器不在时按路径短路/报错/ensure）────────────────────

/// dev 容器不在时该路径应采取的行为（纯函数，按 path 判定）。
enum DevAbsentAction {
    /// 使用语义（默认）：容器不在 → ensure 创建后转发
    Ensure,
    /// 停止/取消语义：容器不在 = 目标态已达成（服务停着/任务不可能还在跑），
    /// 短路 200 成功——不 ensure（起容器为了停它是反语义）
    SkipSuccess(SkipKind),
    /// 查询语义：容器不在 → 直接报错。任务/进程态在容器内存，重建容器也
    /// 救不回；且空列表/任务 404 会与「业务上确实没有」混淆——报
    /// CONTAINER_NOT_FOUND 让调用方按 code 区分真实原因
    Unavailable,
}

enum SkipKind {
    /// tasks/{task_id}/cancel：终态幂等成功（同容器侧 already_terminal 形状）
    CancelTask(String),
    /// dev/stop：无进程即停（同容器侧 "No running process found" 形状）
    DevStop,
}

fn classify_dev_absent(path: &str) -> DevAbsentAction {
    let rest = path.strip_prefix("/api/v1/userapp").unwrap_or(path);
    if let Some(tail) = rest.strip_prefix("/tasks/") {
        let segs: Vec<&str> = tail.split('/').collect();
        return match segs.as_slice() {
            [_task_id] => DevAbsentAction::Unavailable,
            [task_id, "cancel"] => {
                DevAbsentAction::SkipSuccess(SkipKind::CancelTask(task_id.to_string()))
            }
            [_task_id, "logs"] | [_task_id, "logs", "stream"] => DevAbsentAction::Unavailable,
            // 未知子路径兜底 ensure（容器自答 404，不在此拦截）
            _ => DevAbsentAction::Ensure,
        };
    }
    match rest {
        "/dev/stop" => DevAbsentAction::SkipSuccess(SkipKind::DevStop),
        "/dev/list" => DevAbsentAction::Unavailable,
        _ => DevAbsentAction::Ensure,
    }
}

/// 短路语义的容器在否判定（peek）：只读探测，不 ensure、不写探活缓存、
/// 不触发自愈——短路路径必须零副作用。
async fn dev_container_absent(state: &AppState, app_id: &str) -> bool {
    let Some(info) = registered_builder(state, app_id) else {
        return true;
    };
    let cache = PROBE_OK.get_or_init(dashmap::DashMap::new);
    if cache.get(app_id).is_some_and(|t| t.elapsed() < PROBE_TTL) {
        return false;
    }
    let addr = dev_file_server_addr(state, &info);
    !probe_dev_container(&addr).await
}

/// 全站 HttpResult 信封短路响应（HTTP 恒 200，调用方按信封 code 判断：
/// "0000"=成功、非 0000=失败）。
fn envelope_response(code: &str, message: &str, data: serde_json::Value) -> Response {
    let payload = serde_json::json!({
        "code": code,
        "message": message,
        "data": data,
        "tid": null,
        "success": code == shared_types::error_codes::SUCCESS,
    });
    (axum::http::StatusCode::OK, axum::Json(payload)).into_response()
}

/// 查询类短路：容器不在，报 CONTAINER_NOT_FOUND（message 只陈述事实）。
fn unavailable_response(app_id: &str) -> Response {
    envelope_response(
        shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
        &format!("userApp dev container not running: app_id={app_id}"),
        serde_json::Value::Null,
    )
}

/// cancel 短路成功（容器侧终态幂等同款形状）。
fn cancel_skip_response(task_id: &str) -> Response {
    envelope_response(
        shared_types::error_codes::SUCCESS,
        "success",
        serde_json::json!({
            "task_id": task_id,
            "status": null,
            "already_terminal": true,
        }),
    )
}

/// dev/stop 短路成功（容器侧无进程即停的同款形状）。
fn dev_stop_skip_response(app_id: &str) -> Response {
    envelope_response(
        shared_types::error_codes::SUCCESS,
        "success",
        serde_json::json!({
            "message": "No running process found",
            "app_id": app_id,
            "pid": null,
            "killed_pids": [],
        }),
    )
}

/// 从 raw query string 提取单值参数（值均为白名单字符集，无需 percent-decode）。
fn query_param<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    query?
        .split('&')
        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
}

/// tasks 族定位（签名自描述）：query `app_id` 必填——该族不消费 X-App-Id
/// header（接口签名上不可见的隐式依赖，本批显式化）。
fn require_query_app_id(query: Option<&str>) -> Result<String, HttpResultError> {
    let raw = query_param(query, "app_id")
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(raw) = raw else {
        return Err(HttpResultError::bad_request(
            "missing required query parameter `app_id` for tasks endpoints",
        ));
    };
    shared_types::validate_identifier(raw, "app_id")
        .map(|_| raw.to_string())
        .map_err(HttpResultError::bad_request)
}

/// static/{app_id} 的 query `user_id` 必填（🟢 ensure 显式档：懒创建容器
/// 宿主树分区直取，不依赖 metadata 注册）。非 static 路径返回 None（不要求）。
fn require_static_user_id(
    path: &str,
    query: Option<&str>,
) -> Result<Option<String>, HttpResultError> {
    if !path.starts_with("/api/v1/userapp/static/") {
        return Ok(None);
    }
    let raw = query_param(query, "user_id")
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(raw) = raw else {
        return Err(HttpResultError::bad_request(
            "missing required query parameter `user_id` for static artifact download",
        ));
    };
    shared_types::validate_identifier(raw, "user_id")
        .map(|_| Some(raw.to_string()))
        .map_err(HttpResultError::bad_request)
}

/// 全量透传一个请求到目标 addr（同 path+query）——dev/prod 两阶段共用内核。
///
/// body 走流（axum Body → reqwest stream），multipart 无需感知；响应同样流式
/// （bytes_stream → axum Body），SSE（tasks/{id}/logs/stream 等）天然支持。
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

/// dev/prod 阶段分派解析：缺省 dev（向后兼容既有无 header 调用）；
/// 未知值 fail-fast 400（header 拼错不该静默落错容器）。
fn parse_app_stage(req: &Request) -> Result<UserappStage, Box<Response>> {
    let Some(value) = req
        .headers()
        .get(APP_STAGE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(UserappStage::Dev);
    };
    match value.to_ascii_lowercase().as_str() {
        v if v == APP_STAGE_DEV => Ok(UserappStage::Dev),
        v if v == APP_STAGE_PROD => Ok(UserappStage::Prod),
        other => Err(Box::new(
            HttpResultError::bad_request(format!(
                "invalid `{APP_STAGE_HEADER}` value '{other}'; expected `{APP_STAGE_DEV}` or `{APP_STAGE_PROD}`"
            ))
            .into_response(),
        )),
    }
}

/// `/api/v1/userapp/{*rest}` 通配透传 handler。
///
/// 容器懒启动语义分派：tasks 族 query app_id 自描述定位（不消费 X-App-Id），
/// 容器不在时按 [`classify_dev_absent`] 短路（cancel/dev-stop 成功、查询类
/// CONTAINER_NOT_FOUND）或 ensure 创建；static 族 query user_id 必填（显式档）。
pub(crate) async fn forward_userapp(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: Request,
) -> Response {
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);

    // tasks 族：构建链 dev-only（忽略 X-App-Stage——构建任务只存在于 dev
    // builder），query app_id 定位；容器不在时短路
    if path.starts_with("/api/v1/userapp/tasks/") {
        let app_id = match require_query_app_id(query.as_deref()) {
            Ok(app_id) => app_id,
            Err(e) => return e.into_response(),
        };
        if dev_container_absent(&state, &app_id).await {
            return match classify_dev_absent(&path) {
                DevAbsentAction::SkipSuccess(SkipKind::CancelTask(task_id)) => {
                    info!(
                        "[USERAPP_FORWARD] dev container absent, cancel short-circuit ok: app_id={app_id}, task_id={task_id}"
                    );
                    cancel_skip_response(&task_id)
                }
                _ => {
                    info!(
                        "[USERAPP_FORWARD] dev container absent, task query rejected: app_id={app_id}"
                    );
                    unavailable_response(&app_id)
                }
            };
        }
        info!(
            "[USERAPP_FORWARD] {} {} -> dev container (app_id={app_id}, query-located)",
            req.method(),
            req.uri().path()
        );
        return forward_to_dev(&state, &app_id, req, None).await;
    }

    let Some(app_id) = require_app_id(&req) else {
        return missing_app_id_response();
    };
    let stage = match parse_app_stage(&req) {
        Ok(stage) => stage,
        Err(resp) => return *resp,
    };
    match stage {
        UserappStage::Dev => {
            // 停止/查询短路：仅容器不在时生效（容器在则照常转发）
            let action = classify_dev_absent(&path);
            let short_circuit = !matches!(action, DevAbsentAction::Ensure)
                && dev_container_absent(&state, &app_id).await;
            if short_circuit {
                return match action {
                    DevAbsentAction::SkipSuccess(SkipKind::DevStop) => {
                        info!(
                            "[USERAPP_FORWARD] dev container absent, dev/stop short-circuit ok: app_id={app_id}"
                        );
                        dev_stop_skip_response(&app_id)
                    }
                    // cancel 已在 tasks 分支按 query app_id 处理；此处兜底不可达
                    DevAbsentAction::SkipSuccess(SkipKind::CancelTask(task_id)) => {
                        cancel_skip_response(&task_id)
                    }
                    DevAbsentAction::Unavailable => {
                        info!(
                            "[USERAPP_FORWARD] dev container absent, dev/list rejected: app_id={app_id}"
                        );
                        unavailable_response(&app_id)
                    }
                    DevAbsentAction::Ensure => unreachable!("Ensure 已被 short_circuit 条件排除"),
                };
            }
            let explicit_user = match require_static_user_id(&path, query.as_deref()) {
                Ok(v) => v,
                Err(e) => return e.into_response(),
            };
            info!(
                "[USERAPP_FORWARD] {} {} -> dev container (app_id={app_id})",
                req.method(),
                req.uri().path()
            );
            forward_to_dev(&state, &app_id, req, explicit_user.as_deref()).await
        }
        UserappStage::Prod => {
            info!(
                "[USERAPP_FORWARD] {} {} -> prod runtime container (app_id={app_id})",
                req.method(),
                req.uri().path()
            );
            forward_to_prod(&state, &app_id, req).await
        }
    }
}

/// `/api/v1/userapp/{app_id}/{app_stage}` 门面折叠转发（dev-only 构建链公用内核）：
///
/// 1. `{app_stage}` 仅认 `dev`——构建链是开发阶段能力，传 prod 返回 400 明示；
/// 2. 容器定位沿用透传面契约：`X-App-Id` header（require_app_id 白名单校验），
///    body 自带的 app_id 字段由容器侧 `resolve_userapp_dev` 消费；
/// 3. **URI 折叠**：剥掉门面段 `/api/v1/userapp/{app_id}/{app_stage}` 还原容器平铺
///    契约路径（file-server-userapp 端点零改动），query 原样保留。
async fn fold_env_forward(
    state: Arc<AppState>,
    path: axum::extract::Path<(String, String)>,
    req: Request,
    target_path: &'static str,
) -> Response {
    use shared_types::UserappStage;
    let (path_app_id, app_stage) = path.0;
    let Some(app_stage) = UserappStage::parse(&app_stage) else {
        return HttpResultError::bad_request("path segment `app_stage` must be `dev` or `prod`")
            .into_response();
    };
    if app_stage != UserappStage::Dev {
        return HttpResultError::bad_request(format!(
            "`{target_path}` is a dev (build-chain) capability: pass app_stage=dev"
        ))
        .into_response();
    }
    let Some(app_id) = require_app_id(&req) else {
        return missing_app_id_response();
    };
    // 门面段一致性：path 与 X-App-Id 不一致直接拒（防错把请求打进别的开发容器）
    if path_app_id != app_id {
        return HttpResultError::bad_request(format!(
            "path app_id '{path_app_id}' != header `X-App-Id` '{app_id}'"
        ))
        .into_response();
    }

    let mut req = req;
    let rebuilt = match rebuild_uri_with(req.uri(), target_path) {
        Ok(uri) => uri,
        Err(e) => return HttpResultError::system(e).into_response(),
    };
    *req.uri_mut() = rebuilt;

    info!(
        "[USERAPP_FORWARD] {} {} -> dev container (folded app_stage, app_id={app_id})",
        req.method(),
        req.uri().path()
    );
    // 门面 body 携 user_id 但流式不解析（multipart 族不可解）——owner 走
    // metadata 链（create-workspace 前置注册）
    forward_to_dev(&state, &app_id, req, None).await
}

/// 以 `target_path` 替换原 URI 的 path 部分、拼接原 query，重建 [`Uri`]。
fn rebuild_uri_with(uri: &Uri, target_path: &'static str) -> Result<Uri, String> {
    let pq = match uri.query() {
        Some(q) => format!("{target_path}?{q}"),
        None => target_path.to_string(),
    };
    Uri::try_from(pq).map_err(|e| format!("rebuild forwarded uri: {e:?}"))
}

/// 探测开发容器内的项目类型
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/{app_stage}/projects/detect",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：仅支持 `dev`（构建链为开发阶段能力）")
    ),
    request_body(
        content = serde_json::Value,
        description = "同容器平铺契约 `{appId/app_id, userId/user_id}`；结构详见 file-server 文档同路径"
    ),
    description = r#"
分析开发容器 workspace 的文件结构，推断项目类型（Node/Python/Java…）与推荐配置，
作为 confirm 的输入。**仅 dev**——构建链是开发阶段能力，传 prod 返回 400。

定位沿用透传面契约：header `X-App-Id` 指定目标开发容器（须与 path 一致）；
URI 折叠为容器内平铺路径 `/api/v1/userapp/projects/detect` 后流式转发。
"#,
    responses(
        (status = 200, description = "探测结果（HttpResult 信封，data 含类型推断与文件清单）", body = HttpResult<serde_json::Value>),
        (status = 400, description = "app_stage 非 dev / 缺或错 X-App-Id / 参数非法", body = HttpResult<String>)
    ),
    tag = "UserApp · dev · 工作区与工具链",
)]
pub(crate) async fn flat_dev_projects_detect(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    path: axum::extract::Path<(String, String)>,
    req: Request,
) -> Response {
    fold_env_forward(state, path, req, "/api/v1/userapp/projects/detect").await
}

/// 确认开发容器的项目类型
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/{app_stage}/projects/confirm",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：仅支持 `dev`（构建链为开发阶段能力）")
    ),
    request_body(
        content = serde_json::Value,
        description = "detect 结果的用户修正确认 + 项目基础信息；字段同容器平铺契约"
    ),
    description = r#"
用户在 detect 推断基础上选择/修正项目类型后提交确认（幂等附带 git init 双开关）。
**仅 dev**；定位与折叠语义同 [`flat_dev_projects_detect`]。
"#,
    responses(
        (status = 200, description = "确认结果（HttpResult 信封）", body = HttpResult<serde_json::Value>),
        (status = 400, description = "app_stage 非 dev / 缺或错 X-App-Id / 参数非法", body = HttpResult<String>)
    ),
    tag = "UserApp · dev · 工作区与工具链",
)]
pub(crate) async fn flat_dev_projects_confirm(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    path: axum::extract::Path<(String, String)>,
    req: Request,
) -> Response {
    fold_env_forward(state, path, req, "/api/v1/userapp/projects/confirm").await
}

/// 安装项目到开发容器
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/{app_stage}/install-project",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：仅支持 `dev`（构建链为开发阶段能力）")
    ),
    request_body(
        content = serde_json::Value,
        description = "安装参数（同容器平铺契约 install 表单；详见 file-server 文档同路径）"
    ),
    description = r#"
将项目安装进开发容器工作区（依赖安装等初始化动作的统一入口）。**仅 dev**；
定位与折叠语义同 [`flat_dev_projects_detect`]。
"#,
    responses(
        (status = 200, description = "安装结果（HttpResult 信封）", body = HttpResult<serde_json::Value>),
        (status = 400, description = "app_stage 非 dev / 缺或错 X-App-Id / 参数非法", body = HttpResult<String>)
    ),
    tag = "UserApp · dev · 工作区与工具链",
)]
pub(crate) async fn flat_dev_install_project(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    path: axum::extract::Path<(String, String)>,
    req: Request,
) -> Response {
    fold_env_forward(state, path, req, "/api/v1/userapp/install-project").await
}
/// `/api/computer/*` 拦截层：header `X-Service-Type: userapp` 即短路转发该 app
/// 目标容器**同路径**（TS 路径原样、body 零解析，header 随请求透传供容器内
/// computer handler 消费做 workspace 切换）；无该 header 落本地移植 handler。
/// `X-App-Stage` 同样生效（缺省 dev，与 /api/v1/userapp/* 分派一致）。
pub(crate) async fn computer_intercept(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let is_userapp = req
        .headers()
        .get(SERVICE_TYPE_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(shared_types::is_userapp_service_type_value);
    if !is_userapp {
        return next.run(req).await;
    }
    let Some(app_id) = require_app_id(&req) else {
        return missing_app_id_response();
    };
    let stage = match parse_app_stage(&req) {
        Ok(stage) => stage,
        Err(resp) => return *resp,
    };
    match stage {
        UserappStage::Dev => {
            info!(
                "[USERAPP_FORWARD] intercepted computer request {} -> dev container (app_id={app_id})",
                req.uri().path()
            );
            // TS 老族 body 携 user_id（camelCase 契约）但流式不解析——metadata 链
            forward_to_dev(&state, &app_id, req, None).await
        }
        UserappStage::Prod => {
            info!(
                "[USERAPP_FORWARD] intercepted computer request {} -> prod runtime container (app_id={app_id})",
                req.uri().path()
            );
            forward_to_prod(&state, &app_id, req).await
        }
    }
}

// ── HttpResult 错误响应（透传层自身错误；上游业务响应原样透传不重包装） ──────────

/// 轻量错误值（Result 大 Err 侧禁用 Response 本体；测试 unwrap 需 Debug）。
#[derive(Debug)]
struct HttpResultError {
    status: axum::http::StatusCode,
    message: String,
    /// 503 唤醒类错误的 Retry-After 秒数（对齐 proxy_http 流量唤醒面）。
    retry_after_secs: Option<u32>,
}

impl HttpResultError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: axum::http::StatusCode::BAD_REQUEST,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    fn bad_gateway(message: impl Into<String>) -> Self {
        Self {
            status: axum::http::StatusCode::BAD_GATEWAY,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: axum::http::StatusCode::NOT_FOUND,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    fn service_unavailable(message: impl Into<String>, retry_after_secs: u32) -> Self {
        Self {
            status: axum::http::StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
            retry_after_secs: Some(retry_after_secs),
        }
    }

    fn system(message: impl Into<String>) -> Self {
        Self {
            status: axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    /// 装箱响应（`Result` 的 Err 侧按指针传播——`Response` >128B，
    /// resolve_dev_addr 等深链函数避免按值 move）。
    fn into_boxed_response(self) -> Box<Response> {
        Box::new(self.into_response())
    }
}

impl IntoResponse for HttpResultError {
    fn into_response(self) -> Response {
        // 与 shared_types::HttpResult 同形态(code=字符串错误码/message/data/tid/success),
        // 但保留真实 HTTP 状态码(400/404/502/503 对代理与客户端有语义; HttpResult 的
        // IntoResponse 恒 200, 不适用于透传层的传输级错误)
        let payload = serde_json::json!({
            "code": error_code_for(self.status),
            "message": self.message,
            "data": serde_json::Value::Null,
            "success": false,
        });
        let mut response = (self.status, axum::Json(payload)).into_response();
        if let Some(secs) = self.retry_after_secs
            && let Ok(value) = axum::http::HeaderValue::from_str(&secs.to_string())
        {
            response.headers_mut().insert("retry-after", value);
        }
        response
    }
}

/// HTTP 状态码 → 全站字符串错误码(对齐 shared_types::error_codes 词表)。
fn error_code_for(status: axum::http::StatusCode) -> &'static str {
    match status {
        axum::http::StatusCode::BAD_REQUEST => shared_types::error_codes::ERR_VALIDATION,
        axum::http::StatusCode::NOT_FOUND => shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
        axum::http::StatusCode::SERVICE_UNAVAILABLE | axum::http::StatusCode::BAD_GATEWAY => {
            shared_types::error_codes::ERR_BACKEND_ERROR
        }
        _ => shared_types::error_codes::ERR_INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify_kind(path: &str) -> &'static str {
        match classify_dev_absent(path) {
            DevAbsentAction::Ensure => "ensure",
            DevAbsentAction::SkipSuccess(SkipKind::CancelTask(_)) => "cancel-ok",
            DevAbsentAction::SkipSuccess(SkipKind::DevStop) => "dev-stop-ok",
            DevAbsentAction::Unavailable => "unavailable",
        }
    }

    /// 路径语义分派全模式：tasks 族四形态 + dev server 短路两条 + 默认 ensure。
    #[test]
    fn classify_dev_absent_covers_all_semantics() {
        assert_eq!(classify_kind("/api/v1/userapp/tasks/t-1"), "unavailable");
        assert_eq!(
            classify_kind("/api/v1/userapp/tasks/t-1/logs"),
            "unavailable"
        );
        assert_eq!(
            classify_kind("/api/v1/userapp/tasks/t-1/logs/stream"),
            "unavailable"
        );
        assert_eq!(
            classify_kind("/api/v1/userapp/tasks/t-1/cancel"),
            "cancel-ok"
        );
        assert_eq!(classify_kind("/api/v1/userapp/dev/stop"), "dev-stop-ok");
        assert_eq!(classify_kind("/api/v1/userapp/dev/list"), "unavailable");
        // 使用语义默认 ensure（起容器）
        for path in [
            "/api/v1/userapp/build",
            "/api/v1/userapp/dev/start",
            "/api/v1/userapp/dev/restart",
            "/api/v1/userapp/dev/logs",
            "/api/v1/userapp/ensure-workspace",
            "/api/v1/userapp/static/app-1",
            "/api/v1/userapp/get-file-list",
        ] {
            assert_eq!(classify_kind(path), "ensure", "{path}");
        }
        // 未知 tasks 子路径兜底 ensure（容器自答 404，不在此拦截）
        assert_eq!(classify_kind("/api/v1/userapp/tasks/t-1/unknown"), "ensure");
    }

    #[test]
    fn query_param_extracts_single_value() {
        let q = Some("app_id=app-1&from_seq=3&service=web");
        assert_eq!(query_param(q, "app_id"), Some("app-1"));
        assert_eq!(query_param(q, "from_seq"), Some("3"));
        assert_eq!(query_param(q, "missing"), None);
        assert_eq!(query_param(None, "app_id"), None);
        // 前缀相似键不误匹配
        assert_eq!(query_param(Some("app_idx=1"), "app_id"), None);
        // 无值键
        assert_eq!(query_param(Some("app_id"), "app_id"), None);
    }

    #[test]
    fn require_query_app_id_validates() {
        assert_eq!(require_query_app_id(Some("app_id=app-1")).unwrap(), "app-1");
        // 缺失 / 空串 / 非法字符（含路径穿越）→ Err（400 响应）
        assert!(require_query_app_id(None).is_err());
        assert!(require_query_app_id(Some("user_id=u1")).is_err());
        assert!(require_query_app_id(Some("app_id=")).is_err());
        assert!(require_query_app_id(Some("app_id=../evil")).is_err());
    }

    #[test]
    fn static_user_id_required_only_on_static_paths() {
        // 非 static 路径不要求（None）
        assert_eq!(
            require_static_user_id("/api/v1/userapp/build", None).unwrap(),
            None
        );
        // static 路径必填
        assert!(require_static_user_id("/api/v1/userapp/static/app-1", None).is_err());
        assert_eq!(
            require_static_user_id(
                "/api/v1/userapp/static/app-1",
                Some("release_id=r1&user_id=u1")
            )
            .unwrap(),
            Some("u1".to_string())
        );
        // 白名单校验（含 / 即拒）
        assert!(
            require_static_user_id("/api/v1/userapp/static/app-1", Some("user_id=../evil"))
                .is_err()
        );
    }

    /// 短路信封形状：cancel 幂等终态（与容器侧 CancelData 同构）。
    #[tokio::test]
    async fn cancel_skip_response_matches_container_shape() {
        let resp = cancel_skip_response("t-1");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["code"], shared_types::error_codes::SUCCESS);
        assert_eq!(v["success"], true);
        assert_eq!(v["data"]["task_id"], "t-1");
        assert_eq!(v["data"]["status"], serde_json::Value::Null);
        assert_eq!(v["data"]["already_terminal"], true);
    }

    /// 报错信封：HTTP 200 + CONTAINER_NOT_FOUND + message 只陈述事实。
    #[tokio::test]
    async fn unavailable_response_is_enveloped_container_not_found() {
        let resp = unavailable_response("app-1");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v["code"],
            shared_types::error_codes::ERR_CONTAINER_NOT_FOUND
        );
        assert_eq!(v["success"], false);
        assert_eq!(v["data"], serde_json::Value::Null);
        assert_eq!(
            v["message"],
            "userApp dev container not running: app_id=app-1"
        );
    }

    /// dev/stop 短路信封（与容器侧 UserappDevStopped 同构）。
    #[tokio::test]
    async fn dev_stop_skip_response_matches_container_shape() {
        let resp = dev_stop_skip_response("app-1");
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["code"], shared_types::error_codes::SUCCESS);
        assert_eq!(v["data"]["message"], "No running process found");
        assert_eq!(v["data"]["app_id"], "app-1");
        assert_eq!(v["data"]["pid"], serde_json::Value::Null);
        assert_eq!(v["data"]["killed_pids"], serde_json::json!([]));
    }

    /// 透传清单 ↔ 语义分类闭包：tasks 族必须命中短路/报错类（query app_id
    /// 定位的前提），dev/stop、dev/list 必须命中各自类——路径改形/新增 tasks
    /// 子族忘同步 classify 当场报红。
    #[test]
    fn pass_through_paths_have_expected_absent_semantics() {
        use crate::userapp_forward::CONTAINER_PASS_THROUGH_PATHS;
        for pattern in CONTAINER_PASS_THROUGH_PATHS {
            // 模式串占位符替换为样例值后分类
            let sample = pattern
                .replace("{task_id}", "t-1")
                .replace("{app_id}", "app-1");
            let kind = classify_kind(&sample);
            let expected = match *pattern {
                "/api/v1/userapp/tasks/{task_id}" => "unavailable",
                "/api/v1/userapp/tasks/{task_id}/logs" => "unavailable",
                "/api/v1/userapp/tasks/{task_id}/logs/stream" => "unavailable",
                "/api/v1/userapp/tasks/{task_id}/cancel" => "cancel-ok",
                "/api/v1/userapp/dev/stop" => "dev-stop-ok",
                "/api/v1/userapp/dev/list" => "unavailable",
                _ => "ensure",
            };
            assert_eq!(kind, expected, "{pattern} 语义分类漂移");
        }
    }
}
