//! userApp 开发域终端/桌面代理（`/userapp/{ttyd,vnc,audio,ime}/{app_id}` 族，与 /proxy/apps、/proxy/devapps 同前缀）。
//!
//! 与 computer 族（`/computer/*`，按 user_id 定位沙箱）对称的开发场景入口：
//! 按 **app_id** 定位该 app 的 UserAppBuilder 开发容器（镜像同款——内含
//! ttyd 7681 / noVNC 6080 / 音频 6089+6090 / IME 6091，以及 agent_runner
//! ws_terminal 中间层 17681）。
//!
//! 定位统一走 `find_by_project_id(app_id, UserAppBuilder)`（state.projects
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

/// 按 app_id 解析 UserAppBuilder 开发容器 IP（app_id 先过 identifier 白名单，
/// 防 header 注入与路径拼接逃逸）。
fn find_dev_container(
    container_lookup: &Option<Arc<dyn shared_types::ContainerLookup>>,
    app_id: &str,
) -> Result<String, Box<pingora_core::Error>> {
    if let Err(e) = shared_types::validate_identifier(app_id, "app_id") {
        warn!("[DEV_TERMINAL] invalid app_id: {}", e);
        return Err(pingora_core::Error::new(
            pingora_core::ErrorType::HTTPStatus(400),
        ));
    }
    container_lookup
        .as_ref()
        .and_then(|lookup| {
            lookup.find_by_project_id(app_id, &shared_types::ServiceType::UserAppBuilder)
        })
        .ok_or_else(|| {
            info!(
                "[DEV_TERMINAL] dev container not found: app_id={app_id} (create workspace first)"
            );
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404)).more_context(
                format!(
                    "userapp dev container for app {app_id} not found, please create workspace first"
                ),
            )
        })
}

/// 提取并校验 app_id 路径参数。
fn require_app_id(params: &Params<'_, '_>) -> Result<String, Box<pingora_core::Error>> {
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

    let host = ctx.vnc_target_ip.as_deref().unwrap_or("127.0.0.1");
    upstream_request.insert_header("Host", host)?;
    let new_uri = utils::rewrite_uri(original_uri, target_path)?;
    upstream_request.set_uri(new_uri);
    utils::set_common_headers(upstream_request)?;
    upstream_request.insert_header("X-Ttyd-Proxy", "pingora-dev")?;
    // ws_terminal 的 cwd 解析三元组：service_type=UserAppBuilder → 开发卷 {根}/{project_id}
    upstream_request.insert_header("X-Ttyd-Project-Id", &app_id)?;
    upstream_request.insert_header(
        "X-Ttyd-Service-Type",
        shared_types::ServiceType::UserAppBuilder.to_string(),
    )?;
    Ok(())
}

/// ttyd 上游：ws_terminal 中间层（17681，agent_runner 协商 tty 子协议后连 ttyd 本体）。
pub async fn handle_dev_ttyd_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    metrics: &Arc<ProxyMetrics>,
    container_lookup: &Option<Arc<dyn shared_types::ContainerLookup>>,
) -> PingoraResult<Box<HttpPeer>> {
    let app_id = require_app_id(&params)?;
    let container_ip = find_dev_container(container_lookup, &app_id)?;

    metrics.record_request();
    metrics.inc_active();
    ctx.vnc_target_ip = Some(container_ip.clone());
    debug!(
        "[DEV_TTYD] app_id={} -> {}:{}",
        app_id,
        container_ip,
        shared_types::WS_TERMINAL_PORT
    );

    // 与 computer 族同款 peer（WebSocket 长连接优化）
    let mut peer = HttpPeer::new(
        (container_ip.as_str(), shared_types::WS_TERMINAL_PORT),
        false,
        "".to_string(),
    );
    peer.options.connection_timeout = Some(Duration::from_secs(10));
    peer.options.read_timeout = None;
    peer.options.write_timeout = None;
    peer.options.total_connection_timeout = Some(Duration::from_secs(15));
    peer.options.idle_timeout = Some(Duration::from_secs(3600));
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
    metrics: &Arc<ProxyMetrics>,
    container_lookup: &Option<Arc<dyn shared_types::ContainerLookup>>,
) -> PingoraResult<Box<HttpPeer>> {
    let app_id = require_app_id(&params)?;
    let container_ip = find_dev_container(container_lookup, &app_id)?;

    metrics.record_request();
    metrics.inc_active();
    ctx.vnc_target_ip = Some(container_ip.clone());
    debug!(
        "[DEV_VNC] app_id={} -> {}:{}",
        app_id,
        container_ip,
        shared_types::NOVNC_PORT
    );

    let mut peer = HttpPeer::new(
        (container_ip.as_str(), shared_types::NOVNC_PORT),
        false,
        "".to_string(),
    );
    peer.options.connection_timeout = Some(Duration::from_secs(10));
    peer.options.read_timeout = None;
    peer.options.write_timeout = None;
    peer.options.total_connection_timeout = Some(Duration::from_secs(15));
    peer.options.idle_timeout = Some(Duration::from_secs(3600));
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
    metrics: &Arc<ProxyMetrics>,
    container_lookup: &Option<Arc<dyn shared_types::ContainerLookup>>,
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

    let container_ip = find_dev_container(container_lookup, &app_id)?;
    metrics.record_request();
    metrics.record_request_port(target_port);
    ctx.target_port = Some(target_port);
    ctx.upstream_host = Some(format!("{}:{}", container_ip, target_port));
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
    ctx: &TrackingCtx,
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
    metrics.inc_active();

    let mut peer = HttpPeer::new(addr, false, "".to_string());
    peer.options.connection_timeout = Some(Duration::from_secs(10));
    peer.options.read_timeout = None;
    peer.options.write_timeout = None;
    peer.options.total_connection_timeout = Some(Duration::from_secs(15));
    peer.options.idle_timeout = Some(Duration::from_secs(3600));
    Ok(Box::new(peer))
}

// ── IME ─────────────────────────────────────────────────────────────────────────

/// `/userapp/ime/{app_id}/{*path}` 请求重写。
pub async fn handle_dev_ime_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
    ctx: &mut TrackingCtx,
    metrics: &Arc<ProxyMetrics>,
    container_lookup: &Option<Arc<dyn shared_types::ContainerLookup>>,
) -> PingoraResult<()> {
    let app_id = require_app_id(&params)?;
    let target_path = target_path_of(&params);

    let container_ip = find_dev_container(container_lookup, &app_id)?;
    metrics.record_request();
    metrics.record_request_port(crate::service::types::IME_PORT);
    ctx.target_port = Some(crate::service::types::IME_PORT);
    ctx.upstream_host = Some(format!(
        "{}:{}",
        container_ip,
        crate::service::types::IME_PORT
    ));
    debug!(
        "[DEV_IME] app_id={} -> {}:{}",
        app_id,
        container_ip,
        crate::service::types::IME_PORT
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
    ctx: &TrackingCtx,
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
    metrics.inc_active();

    let mut peer = HttpPeer::new(addr, false, "".to_string());
    peer.options.connection_timeout = Some(Duration::from_secs(10));
    peer.options.read_timeout = None;
    peer.options.write_timeout = None;
    peer.options.total_connection_timeout = Some(Duration::from_secs(15));
    peer.options.idle_timeout = Some(Duration::from_secs(3600));
    Ok(Box::new(peer))
}
