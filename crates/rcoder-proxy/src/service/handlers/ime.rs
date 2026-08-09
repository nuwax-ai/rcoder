//! IME 输入法代理处理函数
//!
//! 处理 `/computer/ime/{user_id}/{project_id}/{*path}` 路径的 IME WebSocket 代理。

use dashmap::DashMap;
use matchit::Params;
use pingora_core::Result as PingoraResult;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::service::types::{IME_PORT, ProxyMetrics, TrackingCtx};
use crate::service::utils;

/// 处理 IME 输入法代理请求
///
/// 路径格式: `/computer/ime/{user_id}/{project_id}/{*path}`
///
/// 功能:
/// - 提取 user_id 和 project_id 参数
/// - 查找用户容器 IP
/// - 重写 URI，去掉路径前缀
/// - 设置代理标识头
pub async fn handle_ime_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
    ctx: &mut TrackingCtx,
    vnc_backends: &Arc<DashMap<String, String>>,
) -> PingoraResult<()> {
    let user_id = params.get("user_id").ok_or_else(|| {
        error!("IME route missing user_id param");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    let project_id = params.get("project_id").ok_or_else(|| {
        error!("IME route missing project_id param");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    let remaining_path = params.get("path").unwrap_or("");
    let target_path = format!("/{}", remaining_path.trim_start_matches('/'));

    // 获取容器 IP
    let container_ip = vnc_backends
        .get(user_id)
        .map(|entry| entry.value().clone())
        .ok_or_else(|| {
            warn!("[IME] container not found: user_id={}", user_id);
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404)).more_context(
                format!(
                    "IME backend for user {} not found, please create container first",
                    user_id
                ),
            )
        })?;

    ctx.target_port = Some(IME_PORT);
    ctx.upstream_host = Some(format!("{}:{}", container_ip, IME_PORT));

    info!(
        "IME proxy: user_id={}, project_id={}, path={}, target={}:{}",
        user_id, project_id, remaining_path, container_ip, IME_PORT
    );

    upstream_request.insert_header("Host", &container_ip)?;

    let new_uri = utils::rewrite_uri(original_uri, target_path)?;
    upstream_request.set_uri(new_uri);

    utils::set_common_headers(upstream_request)?;
    upstream_request.insert_header("X-IME-Proxy", "pingora")?;
    upstream_request.insert_header("X-IME-User-Id", user_id)?;
    upstream_request.insert_header("X-IME-Project-Id", project_id)?;

    Ok(())
}

/// 处理 IME 上游连接选择
///
/// 功能:
/// - 根据 user_id 查找容器 IP
/// - 创建到容器 IME 端口的 HTTP Peer
/// - 配置 WebSocket 长连接优化参数
pub async fn handle_ime_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    vnc_backends: &Arc<DashMap<String, String>>,
    metrics: &Arc<ProxyMetrics>,
) -> PingoraResult<Box<HttpPeer>> {
    let user_id = params.get("user_id").ok_or_else(|| {
        error!("IME route missing user_id param");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    let container_ip = vnc_backends
        .get(user_id)
        .map(|entry| entry.value().clone())
        .ok_or_else(|| {
            warn!("[IME] container not found: user_id={}", user_id);
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
                .more_context(format!("IME backend for user {} not found", user_id))
        })?;

    metrics.record_request();
    metrics.record_request_port(IME_PORT);
    metrics.inc_active();

    // 保存目标 IP 到上下文（用于响应过滤）
    ctx.vnc_target_ip = Some(container_ip.clone());

    let peer_addr = format!("{}:{}", container_ip, IME_PORT);
    let mut peer = Box::new(HttpPeer::new(peer_addr.clone(), false, "".to_string()));

    // IME WebSocket 长连接优化配置
    peer.options.connection_timeout = Some(Duration::from_secs(10));
    peer.options.read_timeout = None; // 无限等待（WebSocket 持续流）
    peer.options.write_timeout = None; // 无限等待（WebSocket 双向流）
    peer.options.total_connection_timeout = Some(Duration::from_secs(15));
    peer.options.idle_timeout = Some(Duration::from_secs(3600)); // 1小时空闲超时

    debug!("IME connection from: {}", peer_addr);

    Ok(peer)
}
