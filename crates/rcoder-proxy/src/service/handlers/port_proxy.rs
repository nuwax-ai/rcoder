//! 端口代理处理函数
//!
//! 处理 `/proxy/{port}/{*path}` 路径的端口反向代理。

use matchit::Params;
use pingora_core::Result as PingoraResult;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error};

use crate::service::types::{ProxyMetrics, TrackingCtx};
use crate::service::utils;

/// 处理端口代理请求
///
/// 路径格式: `/proxy/{port}/{*path}`
///
/// 功能:
/// - 从路径参数中提取目标端口
/// - 从原始 URI 中提取剩余路径（保留尾斜杠）
/// - 重写 URI，去掉 `/proxy/{port}` 前缀
/// - 设置代理标识头
pub async fn handle_port_proxy_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
    use_round_robin: bool,
) -> PingoraResult<()> {
    // 从路径参数中提取端口
    let port_str = params.get("port").ok_or_else(|| {
        error!("port proxy route missing port params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    let port: u16 = port_str.parse().map_err(|_| {
        error!(" parse port failed: {}", port_str);
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    // 从原始 URI 提取剩余路径（保留尾斜杠）
    // 不使用 params.get("path")，因为它来自规范化后的 URI，尾斜杠已被去掉
    let original_path = original_uri.path();
    let prefix = format!("/proxy/{}", port);
    let target_path = if original_path.len() <= prefix.len() {
        "/".to_string()
    } else {
        original_path[prefix.len()..].to_string()
    };

    debug!(
        "portproxyrequest: port={}, target_path={}",
        port, target_path
    );

    // 设置 Host 头
    upstream_request.insert_header("Host", "127.0.0.1")?;

    // 重写 URI
    let new_uri = utils::rewrite_uri(original_uri, target_path)?;
    upstream_request.set_uri(new_uri);

    // 设置代理标识头
    utils::set_common_headers(upstream_request)?;
    upstream_request.insert_header("X-Port-Proxy", "pingora-proxy")?;
    upstream_request.insert_header("X-Target-Port", port.to_string())?;
    upstream_request.insert_header(
        "X-Load-Balancer",
        if use_round_robin {
            "round-robin"
        } else {
            "ketama"
        },
    )?;

    Ok(())
}

/// 处理端口代理的上游连接选择
///
/// 功能:
/// - 根据端口参数查找后端服务
/// - 动态注册未见过的端口到后端映射
/// - 创建到目标端口的 HTTP Peer
/// - 配置长连接优化参数（支持 WebSocket、Vite HMR 等）
pub async fn handle_port_proxy_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    backends: &Arc<RwLock<HashMap<u16, String>>>,
    backend_host: &str,
    metrics: &Arc<ProxyMetrics>,
) -> PingoraResult<Box<HttpPeer>> {
    // 从路径参数中提取端口
    let port_str = params.get("port").ok_or_else(|| {
        error!("port proxy route missing port params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    let target_port: u16 = port_str.parse().map_err(|_| {
        error!(" parse port failed: {}", port_str);
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    ctx.target_port = Some(target_port);
    metrics.record_request();
    metrics.record_request_port(target_port).await;
    metrics.inc_active();

    // 如果端口不在后端映射中，动态添加
    if !backends.read().await.contains_key(&target_port) {
        backends
            .write()
            .await
            .insert(target_port, backend_host.to_string());
        debug!(" routing: {} -> {}", target_port, backend_host);
    }

    // 获取后端主机地址
    let resolved_host = {
        let backends_read = backends.read().await;
        backends_read
            .get(&target_port)
            .cloned()
            .unwrap_or_else(|| backend_host.to_string())
    };

    debug!("route: {}:{}", resolved_host, target_port);

    // 创建 HTTP Peer
    let mut peer = HttpPeer::new(
        (resolved_host.as_str(), target_port),
        false,          // 不使用 TLS
        "".to_string(), // SNI
    );

    // 端口代理长连接优化配置（支持 WebSocket、Vite HMR 等）
    // 与音频/IME WebSocket 场景保持一致
    peer.options.connection_timeout = Some(Duration::from_secs(10));
    peer.options.read_timeout = None; // 无限等待（WebSocket/Vite HMR 需要长连接）
    peer.options.write_timeout = None; // 无限等待（WebSocket 双向流）
    peer.options.total_connection_timeout = Some(Duration::from_secs(15));
    peer.options.idle_timeout = Some(Duration::from_secs(3600)); // 1小时空闲超时

    Ok(Box::new(peer))
}
