//! app 专用端口代理处理函数
//!
//! 处理 `/proxy/apps/{app_id}/{port}/{*path}` 路径的端口反向代理。
//! 按 (app_id, port) 路由到 app_manager 部署的应用后端，解决多 app 同端口冲突。

use dashmap::DashMap;
use matchit::Params;
use pingora_core::Result as PingoraResult;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error};

use crate::service::types::{ProxyMetrics, TrackingCtx};
use crate::service::utils;

/// 处理 app 端口代理请求
///
/// 路径格式: `/proxy/apps/{app_id}/{port}/{*path}` —— 提取 app_id + port，
/// 重写 URI 去掉前缀，设置代理标识头。
pub async fn handle_app_port_proxy_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
) -> PingoraResult<()> {
    let app_id = params.get("app_id").ok_or_else(|| {
        error!("app port proxy route missing app_id params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;
    let port_str = params.get("port").ok_or_else(|| {
        error!("app port proxy route missing port params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;
    let port: u16 = port_str.parse().map_err(|_| {
        error!(" parse port failed: {}", port_str);
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    // 从原始 URI 提取剩余路径（strip /proxy/apps/{app_id}/{port}，保留尾斜杠）
    let original_path = original_uri.path();
    let prefix = format!("/proxy/apps/{}/{}", app_id, port);
    let target_path = if original_path.len() <= prefix.len() {
        "/".to_string()
    } else {
        original_path[prefix.len()..].to_string()
    };

    debug!(
        "app portproxyrequest: app_id={}, port={}, target_path={}",
        app_id, port, target_path
    );

    upstream_request.insert_header("Host", "127.0.0.1")?;
    let new_uri = utils::rewrite_uri(original_uri, target_path)?;
    upstream_request.set_uri(new_uri);
    utils::set_common_headers(upstream_request)?;
    upstream_request.insert_header("X-Port-Proxy", "pingora-app-proxy")?;
    upstream_request.insert_header("X-Target-Port", port.to_string())?;

    Ok(())
}

/// 处理 app 端口代理的上游连接选择
///
/// 按 (app_id, port) 查 `app_backends`。**无兜底**：未注册 → 502（Fail Fast，
/// app 专用路径不能像通用 /proxy/{port} 那样猜 host，否则会路由到错误 app）。
pub async fn handle_app_port_proxy_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    app_backends: &Arc<DashMap<(String, u16), String>>,
    metrics: &Arc<ProxyMetrics>,
) -> PingoraResult<Box<HttpPeer>> {
    let app_id = params.get("app_id").ok_or_else(|| {
        error!("app port proxy upstream missing app_id params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;
    let port_str = params.get("port").ok_or_else(|| {
        error!("app port proxy upstream missing port params");
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

    // 按 (app_id, port) 查 backend；未注册 → 502（无兜底，Fail Fast）
    let key = (app_id.to_string(), target_port);
    let resolved_host = app_backends
        .get(&key)
        .map(|v| v.value().clone())
        .ok_or_else(|| {
            error!(
                " app backend not registered: app_id={}, port={}",
                app_id, target_port
            );
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(502))
        })?;

    debug!(
        "app route: app_id={}, {}:{}",
        app_id, resolved_host, target_port
    );

    // 创建 HTTP Peer（长连接配置，支持 WebSocket / HMR）
    let mut peer = HttpPeer::new((resolved_host.as_str(), target_port), false, "".to_string());
    peer.options.connection_timeout = Some(Duration::from_secs(10));
    peer.options.read_timeout = None;
    peer.options.write_timeout = None;
    peer.options.total_connection_timeout = Some(Duration::from_secs(15));
    peer.options.idle_timeout = Some(Duration::from_secs(3600));

    Ok(Box::new(peer))
}
