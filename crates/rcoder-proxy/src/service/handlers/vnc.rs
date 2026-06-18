//! VNC 代理处理函数
//!
//! 处理 `/computer/vnc/{user_id}/{project_id}/{*path}` 路径的 VNC WebSocket 代理。

use dashmap::DashMap;
use matchit::Params;
use pingora_core::Result as PingoraResult;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info};

use crate::service::types::{NOVNC_PORT, ProxyMetrics, TrackingCtx};
use crate::service::utils;

/// 处理 VNC WebSocket 代理请求
///
/// 路径格式: `/computer/vnc/{user_id}/{project_id}/{*path}`
///
/// 功能:
/// - 提取 user_id 和 project_id 参数
/// - 重写 URI，去掉路径前缀
/// - 设置代理标识头
pub async fn handle_vnc_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
    ctx: &TrackingCtx,
) -> PingoraResult<()> {
    // 从路径参数中提取 user_id 和 project_id
    let user_id = params.get("user_id").ok_or_else(|| {
        error!("VNC route missing user_id param");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    let project_id = params.get("project_id").ok_or_else(|| {
        error!("VNC route missing project_id param");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    // 提取剩余路径（通配符部分）
    let remaining_path = params.get("path").unwrap_or("");
    let target_path = if remaining_path.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", remaining_path)
    };

    debug!(
        "VNC request: user_id={}, project_id={}, target_path={}",
        user_id, project_id, target_path
    );

    // 设置 Host 头
    let host = ctx.vnc_target_ip.as_deref().unwrap_or("127.0.0.1");
    upstream_request.insert_header("Host", host)?;

    // 重写 URI
    let new_uri = utils::rewrite_uri(original_uri, target_path)?;
    upstream_request.set_uri(new_uri);

    // 设置代理标识头
    utils::set_common_headers(upstream_request)?;
    upstream_request.insert_header("X-VNC-Proxy", "pingora")?;
    upstream_request.insert_header("X-VNC-User-Id", user_id)?;
    upstream_request.insert_header("X-VNC-Project-Id", project_id)?;

    Ok(())
}

/// 处理 VNC 上游连接选择
///
/// 功能:
/// - 根据 user_id 查找容器 IP
/// - 创建到容器 noVNC 端口的 HTTP Peer
/// - 配置 WebSocket 长连接优化参数
pub async fn handle_vnc_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    vnc_backends: &Arc<DashMap<String, String>>,
    metrics: &Arc<ProxyMetrics>,
) -> PingoraResult<Box<HttpPeer>> {
    // 从路径参数中提取 user_id
    let user_id = params.get("user_id").ok_or_else(|| {
        error!("VNC route missing user_id param");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    let project_id = params.get("project_id").unwrap_or("");

    debug!(
        "VNC proxy request: user_id={}, project_id={}",
        user_id, project_id
    );

    // 查找用户容器 IP
    let container_ip = match vnc_backends.get(user_id) {
        Some(ip_ref) => ip_ref.value().clone(),
        None => {
            info!("routing {} to VNC", user_id);
            return Err(
                pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
                    .more_context(format!(
                        "VNC backend for user {} not found, please create container first",
                        user_id
                    )),
            );
        }
    };

    // 记录指标
    metrics.record_request();
    metrics.inc_active();

    // 保存 VNC 目标 IP 到上下文（用于响应过滤）
    ctx.vnc_target_ip = Some(container_ip.clone());

    debug!(
        "VNC proxy: user_id={}, project_id={} -> {}:{}",
        user_id, project_id, container_ip, NOVNC_PORT
    );

    // 创建 HTTP Peer 到容器的 noVNC 端口
    // Pingora 会自动处理 WebSocket upgrade
    let mut peer = HttpPeer::new(
        (container_ip.as_str(), NOVNC_PORT),
        false,          // 不使用 TLS
        "".to_string(), // SNI
    );

    // VNC WebSocket 长连接优化配置
    peer.options.connection_timeout = Some(Duration::from_secs(10));
    peer.options.read_timeout = None; // 无限等待（VNC 持续流）
    peer.options.write_timeout = None; // 无限等待（WebSocket 双向流）
    peer.options.total_connection_timeout = Some(Duration::from_secs(15));
    peer.options.idle_timeout = Some(Duration::from_secs(3600)); // 1小时空闲超时

    Ok(Box::new(peer))
}
