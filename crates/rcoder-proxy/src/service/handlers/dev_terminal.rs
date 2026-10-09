//! userApp 开发域工具代理（`/api/v1/userapp/proxy/{ttyd,vnc,audio,ime}/dev/{user_id}/{app_id}` 族；prod 工具族与本族同形态）。
//!
//! 与 computer 族（`/computer/*`，按 user_id 定位沙箱）对称的开发场景入口：
//! 按 **app_id** 定位该 app 的 UserappBuilder 开发容器（镜像同款——内含
//! ttyd 7681 / noVNC 6080 / 音频 6089+6090 / IME 6091，以及 agent_runner
//! ws_terminal 中间层 17681）。
//!
//! 定位统一走 `find_by_project_id(app_id, UserappBuilder)`（state.projects
//! 注册表，create-workspace/chat/publish 均注册）——**不走 vnc_backends 注册**
//! （其键空间是 user_id，混用 app_id 存在撞键路由错容器风险）；miss 即 404
//! （提示先创建 workspace）。
//!
//! ttyd 上游同 computer 族经 ws_terminal（17681）：协商 `tty` 子协议后由
//! agent_runner 连容器内 ttyd，并按 `X-Ttyd-Service-Type: user-app-builder`
//! 把终端 cwd 落到开发卷 `{USERAPP_WORKSPACE_ROOT}/{app_id}`。

use std::time::Duration;

use matchit::Params;
use pingora_core::Result as PingoraResult;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

use crate::service::types::{ProxyMetrics, TrackingCtx};
use crate::service::utils;

/// 按 app_id 解析 UserappBuilder 开发容器 IP（app_id 先过 identifier 白名单，
/// 防 header 注入与路径拼接逃逸）。
///
/// dev 工具族代理共享依赖（ttyd/vnc/audio/ime/dbx/app 六族一致）。
///
/// dispatch 构造一次、各 handler 以单引用收拢传参——替代
/// `metrics + container_lookup + dev_ensure` 三件逐参传递的散弹式签名
///（新依赖入此结构，不再逐函数改签名）。
pub struct DevProxyDeps<'a> {
    pub metrics: &'a Arc<ProxyMetrics>,
    pub container_lookup: &'a Option<Arc<dyn shared_types::ContainerLookup>>,
    pub dev_ensure: &'a arc_swap::ArcSwapOption<Arc<dyn shared_types::UserappDevEnsure>>,
}

/// 权威定位 + 懒启动：经 `UserappDevEnsure` 解析该 app 开发容器当前地址。
///
/// F1 语义（V2-01 fail-closed）：
/// - **权威定位优先**——运行时观测的当前地址是唯一可信值，内存注册表
///   候选不再参与（残影/IP 被其他应用复用的跨应用污染形态由权威值淘汰，
///   也不再用 TCP 端口探测替代身份核验）；端口级可用性交由调用方的
///   连接恢复（P0）处理，本函数只保证"地址正确"。
/// - **依赖未注入即暂不可用**（装配窗口 `AppState` 晚于 Pingora 启动）：
///   返回 503，绝不回退注册表+端口探测。
/// - 权威查询失败（`ObserveFailed`）重试一次后诚实失败——不当作不存在、
///   不借 ensure 绕过保护；仅确认不存在（`Ok(None)`）才走懒启动 ensure。
/// - ensure 失败为类型化 [`shared_types::DevEnsureError`]，按三族合同映射
///   （`BuilderAbsent`→404 指引，围栏/未就绪/故障→503 带原因）。
pub(crate) async fn find_dev_container(
    dev_ensure: &arc_swap::ArcSwapOption<Arc<dyn shared_types::UserappDevEnsure>>,
    app_id: &str,
) -> Result<String, Box<pingora_core::Error>> {
    if let Err(e) = shared_types::validate_identifier(app_id, "app_id") {
        warn!("[DEV_TERMINAL] invalid app_id: {}", e);
        return Err(pingora_core::Error::new(
            pingora_core::ErrorType::HTTPStatus(400),
        ));
    }
    let Some(ensurer) = dev_ensure.load_full() else {
        warn!("[DEV_TERMINAL] dev ensure callback not injected: app_id={app_id}");
        return Err(dev_unavailable_error(
            app_id,
            "dev dependency is not ready (assembly window); retry shortly",
        ));
    };
    let mut locate = ensurer.locate_dev_builder(app_id).await;
    if matches!(
        locate,
        Err(shared_types::DevEnsureError::ObserveFailed { .. })
    ) {
        locate = ensurer.locate_dev_builder(app_id).await;
    }
    match locate {
        Ok(None) => ensure_dev_address(&ensurer, app_id).await,
        Ok(Some(instance)) => Ok(instance.address),
        Err(error) => {
            warn!(
                "[DEV_TERMINAL] locate dev container failed: app_id={app_id}: {}",
                error.brief()
            );
            Err(map_dev_ensure_error(app_id, &error))
        }
    }
}

/// 确认不存在后的懒启动（开终端是使用语义；owner 走 metadata 链——
/// 浏览器终端 URL 无入参携带能力）。
async fn ensure_dev_address(
    ensurer: &Arc<dyn shared_types::UserappDevEnsure>,
    app_id: &str,
) -> Result<String, Box<pingora_core::Error>> {
    match ensurer.ensure_dev_container(app_id).await {
        Ok(info) if !info.container_ip.is_empty() => {
            info!("[DEV_TERMINAL] dev container ensured on demand: app_id={app_id}");
            Ok(info.container_ip)
        }
        Ok(info) => {
            warn!("[DEV_TERMINAL] ensured dev container has no ip: app_id={app_id}, info={info:?}");
            Err(dev_unavailable_error(
                app_id,
                "ensured dev container has no address yet",
            ))
        }
        Err(error) => {
            warn!(
                "[DEV_TERMINAL] ensure dev container failed: app_id={app_id}: {}",
                error.brief()
            );
            Err(map_dev_ensure_error(app_id, &error))
        }
    }
}

/// 类型化失败 → HTTP（三族合同的中期形态：T5 引入用户面分档页前的
/// 统一映射；真实语义不吞不改——404 指引/503 带原因）。
fn map_dev_ensure_error(
    app_id: &str,
    error: &shared_types::DevEnsureError,
) -> Box<pingora_core::Error> {
    match error {
        shared_types::DevEnsureError::BuilderAbsent { .. } => not_found_error(app_id),
        shared_types::DevEnsureError::OperationInFlight { .. }
        | shared_types::DevEnsureError::NotReady { .. }
        | shared_types::DevEnsureError::ObserveFailed { .. }
        | shared_types::DevEnsureError::EnsureFailed { .. } => {
            dev_unavailable_error(app_id, &error.brief())
        }
    }
}

fn dev_unavailable_error(app_id: &str, reason: &str) -> Box<pingora_core::Error> {
    pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(503)).more_context(format!(
        "userapp dev container for app {app_id} unavailable: {reason}"
    ))
}

fn not_found_error(app_id: &str) -> Box<pingora_core::Error> {
    pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404)).more_context(format!(
        "userapp dev container for app {app_id} not found, please create workspace first"
    ))
}

/// 提取并校验 app_id 路径参数。
/// 路由参数 user_id 提取（工具族新形态 `{user_id}/{app_id}` 双段）。
/// 用户占位段存在性检查（R07）：路由 pattern 携带该段（非空即可匹配），
/// **不校验值**（identifier 白名单对占位值不适用——`legacy.user` 等任意
/// 非空合法 URL 段必须到同实例；缺段仍 400——路由本身需要该段存在）。
pub(crate) fn accept_placeholder_user_id(
    params: &Params<'_, '_>,
) -> Result<(), Box<pingora_core::Error>> {
    params
        .get("user_id")
        .is_none()
        .then(|| {
            error!("[DEV_TERMINAL] route missing user_id placeholder segment");
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })
        .map_or(Ok(()), Err)
}

pub(crate) fn require_app_id(params: &Params<'_, '_>) -> Result<String, Box<pingora_core::Error>> {
    let app_id = params.get("app_id").ok_or_else(|| {
        error!("[DEV_TERMINAL] route missing app_id param");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;
    Ok(app_id.to_owned())
}

/// 通配剩余路径 → 目标路径（空归一 "/"，其余补前导 /）。
fn target_path_of(params: &Params<'_, '_>) -> String {
    match params.get("path") {
        Some(p) if !p.is_empty() => format!("/{p}"),
        _ => "/".to_string(),
    }
}

// ── ttyd ────────────────────────────────────────────────────────────────────────

/// `/userapp/ttyd/{app_id}/{*path}` 请求重写。
pub async fn handle_dev_ttyd_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
    ctx: &TrackingCtx,
) -> PingoraResult<()> {
    let app_id = require_app_id(&params)?;
    let target_path = target_path_of(&params);
    debug!("[DEV_TTYD] app_id={}, target_path={}", app_id, target_path);

    // 终端 query 参数（浏览器 WS 无 header 能力，query 是唯一载体）。
    // dev 链定位键恒为 app_id、服务类型恒为 UserappBuilder——service_type
    // 不允许客户端改写，出现即 400；cwd 走多平台绝对路径归一后注入。
    let query_params = super::ttyd_params::parse_terminal_query(original_uri.query())
        .map_err(|message| dev_ttyd_bad_request(&message))?;
    if let Some(service_type) = query_params.service_type.as_deref() {
        return Err(dev_ttyd_bad_request(&format!(
            "service_type query parameter is not supported on this route: {service_type}"
        )));
    }
    let explicit_cwd = super::ttyd_params::resolve_terminal_cwd(query_params.cwd.as_deref())
        .map_err(|message| dev_ttyd_bad_request(&message))?;
    upstream_request.remove_header("X-Ttyd-Cwd");
    if let Some(cwd) = explicit_cwd.as_deref() {
        upstream_request.insert_header("X-Ttyd-Cwd", cwd)?;
    }

    let host = ctx.vnc_target_ip.as_deref().unwrap_or("127.0.0.1");
    upstream_request.insert_header("Host", host)?;
    let new_uri = utils::rewrite_uri(original_uri, target_path)?;
    upstream_request.set_uri(new_uri);
    utils::set_common_headers(upstream_request)?;
    upstream_request.insert_header("X-Ttyd-Proxy", "pingora-dev")?;
    // ws_terminal 的 cwd 解析三元组：service_type=UserappBuilder → 开发卷 {根}/{project_id}
    upstream_request.insert_header("X-Ttyd-Project-Id", &app_id)?;
    upstream_request.insert_header(
        "X-Ttyd-Service-Type",
        shared_types::ServiceType::UserappBuilder.to_string(),
    )?;
    Ok(())
}

/// dev 终端 query 参数非法 → 400（带原因，fail fast）。
fn dev_ttyd_bad_request(message: &str) -> pingora_core::BError {
    warn!("[DEV_TTYD] rejected query params: {}", message);
    pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        .more_context(message.to_string())
}

/// ttyd 上游：ws_terminal 中间层（17681，agent_runner 协商 tty 子协议后连 ttyd 本体）。
pub async fn handle_dev_ttyd_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    deps: &DevProxyDeps<'_>,
) -> PingoraResult<Box<HttpPeer>> {
    let app_id = require_app_id(&params)?;
    accept_placeholder_user_id(&params)?;
    let container_ip = find_dev_container(deps.dev_ensure, &app_id).await?;

    if !ctx.dev_request_recorded {
        deps.metrics.record_request();
        ctx.dev_request_recorded = true;
    }
    if !ctx.dev_metrics_counted {
        deps.metrics.inc_active();
        ctx.dev_metrics_counted = true;
    }
    ctx.vnc_target_ip = Some(container_ip.clone());
    debug!(
        "[DEV_TTYD] app_id={} -> {}:{}",
        app_id,
        container_ip,
        shared_types::WS_TERMINAL_PORT
    );

    // 与 computer 族同款 peer（WebSocket 长连接优化）
    let mut peer = HttpPeer::new(
        super::super::upstream::dial_peer(&container_ip, shared_types::WS_TERMINAL_PORT)?,
        false,
        "".to_string(),
    );
    super::streaming_peer_options(&mut peer, Duration::from_secs(3600));
    Ok(Box::new(peer))
}

// ── VNC（noVNC） ────────────────────────────────────────────────────────────────

/// `/userapp/vnc/{app_id}/{*path}` 请求重写。
pub async fn handle_dev_vnc_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
    ctx: &TrackingCtx,
) -> PingoraResult<()> {
    let app_id = require_app_id(&params)?;
    let target_path = target_path_of(&params);
    debug!("[DEV_VNC] app_id={}, target_path={}", app_id, target_path);

    let host = ctx.vnc_target_ip.as_deref().unwrap_or("127.0.0.1");
    upstream_request.insert_header("Host", host)?;
    let new_uri = utils::rewrite_uri(original_uri, target_path)?;
    upstream_request.set_uri(new_uri);
    utils::set_common_headers(upstream_request)?;
    upstream_request.insert_header("X-Vnc-Proxy", "pingora-dev")?;
    upstream_request.insert_header("X-Dev-App-Id", &app_id)?;
    Ok(())
}

/// VNC 上游：容器内 noVNC（NOVNC_PORT=6080，HTTP+WebSocket 同端口）。
pub async fn handle_dev_vnc_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    deps: &DevProxyDeps<'_>,
) -> PingoraResult<Box<HttpPeer>> {
    let app_id = require_app_id(&params)?;
    accept_placeholder_user_id(&params)?;
    let container_ip = find_dev_container(deps.dev_ensure, &app_id).await?;

    if !ctx.dev_request_recorded {
        deps.metrics.record_request();
        ctx.dev_request_recorded = true;
    }
    if !ctx.dev_metrics_counted {
        deps.metrics.inc_active();
        ctx.dev_metrics_counted = true;
    }
    ctx.vnc_target_ip = Some(container_ip.clone());
    debug!(
        "[DEV_VNC] app_id={} -> {}:{}",
        app_id,
        container_ip,
        shared_types::NOVNC_PORT
    );

    let mut peer = HttpPeer::new(
        super::super::upstream::dial_peer(&container_ip, shared_types::NOVNC_PORT)?,
        false,
        "".to_string(),
    );
    super::streaming_peer_options(&mut peer, Duration::from_secs(3600));
    Ok(Box::new(peer))
}

// ── 音频 ────────────────────────────────────────────────────────────────────────

/// `/userapp/audio/{app_id}/{*path}` 请求重写（ws → 6089 流，其余 → 6090 静态；
/// 分流规则与 computer 族 audio 一致）。
pub async fn handle_dev_audio_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
    ctx: &mut TrackingCtx,
    deps: &DevProxyDeps<'_>,
) -> PingoraResult<()> {
    let app_id = require_app_id(&params)?;
    let remaining = params.get("path").unwrap_or("");
    let is_ws = remaining == "ws" || remaining.starts_with("ws/");
    let target_port = if is_ws {
        crate::service::types::AUDIO_WS_PORT
    } else {
        crate::service::types::AUDIO_HTTP_PORT
    };
    let target_path = if remaining.is_empty() {
        "/".to_string()
    } else {
        format!("/{remaining}")
    };

    accept_placeholder_user_id(&params)?;
    let container_ip = find_dev_container(deps.dev_ensure, &app_id).await?;
    deps.metrics.record_request();
    deps.metrics.record_request_port(target_port);
    ctx.target_port = Some(target_port);
    ctx.upstream_host = Some(super::super::upstream::dial_addr(
        &container_ip,
        target_port,
    )?);
    info!(
        "[DEV_AUDIO] app_id={}, path={}, target={}:{}",
        app_id, remaining, container_ip, target_port
    );

    upstream_request.insert_header("Host", &container_ip)?;
    let new_uri = utils::rewrite_uri(original_uri, target_path)?;
    upstream_request.set_uri(new_uri);
    utils::set_common_headers(upstream_request)?;
    upstream_request.insert_header("X-Audio-Proxy", "pingora-dev")?;
    upstream_request.insert_header("X-Dev-App-Id", &app_id)?;
    Ok(())
}

/// 音频上游（由 request 阶段写入的 ctx.upstream_host 直连；音频流可持续数小时）。
pub async fn handle_dev_audio_upstream(
    ctx: &mut TrackingCtx,
    metrics: &Arc<ProxyMetrics>,
) -> PingoraResult<Box<HttpPeer>> {
    let host = ctx.upstream_host.clone().ok_or_else(|| {
        error!("[DEV_AUDIO] upstream_host missing (request phase failed?)");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(502))
    })?;
    let addr = host.parse::<std::net::SocketAddr>().map_err(|e| {
        error!("[DEV_AUDIO] parse upstream_host {host}: {e}");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(502))
    })?;
    if !ctx.dev_metrics_counted {
        metrics.inc_active();
        ctx.dev_metrics_counted = true;
    }

    let mut peer = HttpPeer::new(addr, false, "".to_string());
    super::streaming_peer_options(&mut peer, Duration::from_secs(3600));
    Ok(Box::new(peer))
}

// ── IME ─────────────────────────────────────────────────────────────────────────

/// `/userapp/ime/{app_id}/{*path}` 请求重写。
pub async fn handle_dev_ime_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
    ctx: &mut TrackingCtx,
    deps: &DevProxyDeps<'_>,
) -> PingoraResult<()> {
    let app_id = require_app_id(&params)?;
    accept_placeholder_user_id(&params)?;
    let target_path = target_path_of(&params);

    let container_ip = find_dev_container(deps.dev_ensure, &app_id).await?;
    deps.metrics.record_request();
    deps.metrics.record_request_port(shared_types::IME_PORT);
    ctx.target_port = Some(shared_types::IME_PORT);
    ctx.upstream_host = Some(super::super::upstream::dial_addr(
        &container_ip,
        shared_types::IME_PORT,
    )?);
    debug!(
        "[DEV_IME] app_id={} -> {}:{}",
        app_id,
        container_ip,
        shared_types::IME_PORT
    );

    upstream_request.insert_header("Host", &container_ip)?;
    let new_uri = utils::rewrite_uri(original_uri, target_path)?;
    upstream_request.set_uri(new_uri);
    utils::set_common_headers(upstream_request)?;
    upstream_request.insert_header("X-Ime-Proxy", "pingora-dev")?;
    upstream_request.insert_header("X-Dev-App-Id", &app_id)?;
    Ok(())
}

/// IME 上游（WebSocket，由 ctx.upstream_host 直连）。
pub async fn handle_dev_ime_upstream(
    ctx: &mut TrackingCtx,
    metrics: &Arc<ProxyMetrics>,
) -> PingoraResult<Box<HttpPeer>> {
    let host = ctx.upstream_host.clone().ok_or_else(|| {
        error!("[DEV_IME] upstream_host missing (request phase failed?)");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(502))
    })?;
    let addr = host.parse::<std::net::SocketAddr>().map_err(|e| {
        error!("[DEV_IME] parse upstream_host {host}: {e}");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(502))
    })?;
    if !ctx.dev_metrics_counted {
        metrics.inc_active();
        ctx.dev_metrics_counted = true;
    }

    let mut peer = HttpPeer::new(addr, false, "".to_string());
    super::streaming_peer_options(&mut peer, Duration::from_secs(3600));
    Ok(Box::new(peer))
}

// ── 运行容器（部署后的生产环境）───────────────────────────────────────────────
//
// `/api/v1/userapp/proxy/{ttyd,...}/prod/{user_id}/{app_id}/{*path}`：与上面的开发域工具族对称，
// 但目标是 `ServiceType::Userapp` 运行容器（app-runtime 镜像）。两处关键差异：
// 1. 定位走 `find_app_runtime_addr`（确定性命名构造）——运行容器不进 projects
//    注册表（project_to_container[app_id] 单值键被 builder 占用）；
// 2. **不经 ws_terminal（17681）**——运行容器没有 agent_runner，ttyd 直连本体
//    TTYD_PORT=7681（WebSocket upgrade 由 Pingora HTTP 代理直接透传）。
// app 未部署 → 构造地址连接失败 502（语义见 trait 文档）；已停止（scale0）由
// request_filter 的工具族流量唤醒拉起（不 touch，见 proxy_http）。

/// 解析运行容器地址（app_id 先过 identifier 白名单）。
///
/// Docker 模式优先经 `AppRuntimeIpResolver` 实时取容器 **IPv4**——dual-stack
/// 网络下容器名 DNS 的 AAAA 记录会被 pingora 选中，而 app-runtime 的 ttyd 只
/// bind IPv4（7681 ConnectRefused）；resolver 未注入/未命中时回退确定性命名构造
/// （K8s = Service FQDN，Docker = 容器名）。
pub(crate) async fn find_runtime_addr(
    ip_slot: &arc_swap::ArcSwapOption<Arc<dyn shared_types::AppRuntimeIpResolver>>,
    container_lookup: &Option<Arc<dyn shared_types::ContainerLookup>>,
    app_id: &str,
) -> Result<String, Box<pingora_core::Error>> {
    if let Err(e) = shared_types::validate_identifier(app_id, "app_id") {
        warn!("[RUNTIME_TERMINAL] invalid app_id: {}", e);
        return Err(pingora_core::Error::new(
            pingora_core::ErrorType::HTTPStatus(400),
        ));
    }
    if !shared_types::is_kubernetes_runtime()
        && let Some(resolver) = ip_slot.load_full()
        && let Some(ip) = resolver.resolve_runtime_container_ip(app_id).await
    {
        return Ok(ip);
    }
    // lookup 未注入是装配缺陷（基础设施问题，所有 app 一致失败），与
    // "app 未部署"（连接失败 502，由确定性命名构造不查存在性保证）语义不同——
    // 前者 503 明示，避免误导排障方向。
    let lookup = container_lookup.as_ref().ok_or_else(|| {
        error!("[RUNTIME_TERMINAL] container lookup not configured (proxy assembly defect)");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(503))
            .more_context("container lookup not configured for runtime terminal proxy".to_string())
    })?;
    lookup.find_app_runtime_addr(app_id).ok_or_else(|| {
        info!("[RUNTIME_TERMINAL] runtime addr unavailable: app_id={app_id}");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
            .more_context(format!("runtime address for app {app_id} unavailable"))
    })
}

/// `/api/v1/userapp/proxy/ttyd/prod/{app_id}/{*path}` 请求重写（直连 ttyd 本体 7681）。
///
/// 定位在 upstream 阶段完成（pingora 生命周期 `upstream_peer` 先于
/// `upstream_request_filter`——与开发域 ttyd/vnc 同构），此处只重写 URI/Host。
pub async fn handle_runtime_ttyd_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
    ctx: &TrackingCtx,
) -> PingoraResult<()> {
    let app_id = require_app_id(&params)?;
    let target_path = runtime_target_path_of(&params);
    debug!(
        "[RUNTIME_TTYD] app_id={}, target_path={}",
        app_id, target_path
    );

    let host = ctx.vnc_target_ip.as_deref().unwrap_or("127.0.0.1");
    upstream_request.insert_header("Host", host)?;
    let new_uri = utils::rewrite_uri(original_uri, target_path)?;
    upstream_request.set_uri(new_uri);
    utils::set_common_headers(upstream_request)?;
    // 直连 ttyd 本体：X-Ttyd-Proxy 族头是 ws_terminal(17681) 中间层的契约，
    // 运行容器没有该层——不注入（ttyd 忽略未知头，注入反而误导排障）。
    Ok(())
}

/// 运行态 ttyd 上游：定位运行容器 + 直连 ttyd 本体（TTYD_PORT=7681，WebSocket）。
pub async fn handle_runtime_ttyd_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    metrics: &Arc<ProxyMetrics>,
    container_lookup: &Option<Arc<dyn shared_types::ContainerLookup>>,
    ip_slot: &arc_swap::ArcSwapOption<Arc<dyn shared_types::AppRuntimeIpResolver>>,
) -> PingoraResult<Box<HttpPeer>> {
    let app_id = require_app_id(&params)?;
    let container_addr = find_runtime_addr(ip_slot, container_lookup, &app_id).await?;

    if !ctx.prod_metrics_counted {
        metrics.record_request();
        metrics.inc_active();
        ctx.prod_metrics_counted = true;
    }
    ctx.vnc_target_ip = Some(container_addr.clone());
    debug!(
        "[RUNTIME_TTYD] app_id={} -> {}:{}",
        app_id,
        container_addr,
        shared_types::TTYD_PORT
    );

    let mut peer = HttpPeer::new(
        super::super::upstream::dial_peer(&container_addr, shared_types::TTYD_PORT)?,
        false,
        "".to_string(),
    );
    // 终端会话可长开；与开发域 ttyd 同档（idle 1 小时）
    super::streaming_peer_options(&mut peer, Duration::from_secs(3600));
    Ok(Box::new(peer))
}

/// 剩余路径 → 目标路径（空归一 "/"）。工具族路由 `/api/v1/userapp/proxy/{tool}/{stage}/{user_id}/{app_id}/{*path}`
/// 中 tool 为静态段，剩余 path 可为空；与开发域 `target_path_of` 语义一致。
pub(crate) fn runtime_target_path_of(params: &Params<'_, '_>) -> String {
    match params.get("path") {
        Some(p) if !p.is_empty() => format!("/{p}"),
        _ => "/".to_string(),
    }
}
