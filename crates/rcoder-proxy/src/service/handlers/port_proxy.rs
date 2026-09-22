//! 端口代理处理函数
//!
//! 处理 `/proxy/{port}/{*path}` 路径的端口反向代理。

use arc_swap::{ArcSwap, ArcSwapOption};
use matchit::Params;
use pingora_core::Result as PingoraResult;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error};

use crate::service::types::{PreviewRouteDeps, ProxyMetrics, TrackingCtx};
use crate::service::utils;
use shared_types::PreviewRouteResolution;

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
    ctx: &mut TrackingCtx,
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

    // Custom Page 预览转发：决策已在本请求更早的 upstream_peer 阶段完成
    // （pingora 阶段序 upstream_peer 先于本函数所在的 upstream_request_filter，
    // peer 选择必须先行——历史上 resolve 放在本地导致非宿主副本在 peer 阶段
    // 连 127.0.0.1:4000 即 502，Forward 永无执行机会）。此处仅消费决策：
    // 重写为内部入口 + 令牌头（take 一次性消费+token 由 peer 阶段写 ctx，
    // 消除 ArcSwap 二次 load）；无决策走下方既有本机路径（行为与现状一致）。
    if let Some(token) = ctx.preview_internal_token.take()
        && let Some((instance_id, preview_port)) = ctx.preview_rewrite.take()
    {
        let internal_path =
            format!("/internal/preview-forward/{instance_id}/{preview_port}{target_path}");
        let internal_uri = utils::rewrite_uri(original_uri, internal_path)?;
        upstream_request.set_uri(internal_uri);
        // 令牌头覆盖（客户端伪造值被替换为进程持有令牌）
        upstream_request.insert_header("x-preview-internal-token", &token)?;
        upstream_request.insert_header("Host", "127.0.0.1")?;
        utils::set_common_headers(upstream_request)?;
        return Ok(());
    }

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
/// - 显式配置的端口使用配置主机，其他端口直接使用默认主机
/// - 创建到目标端口的 HTTP Peer
/// - 配置长连接优化参数（支持 WebSocket、Vite HMR 等）
pub async fn handle_port_proxy_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    backends: &Arc<ArcSwap<HashMap<u16, String>>>,
    backend_host: &str,
    metrics: &Arc<ProxyMetrics>,
    preview_slot: &Arc<ArcSwapOption<Arc<PreviewRouteDeps>>>,
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

    // Custom Page 预览路由决策（必须在 upstream_peer 阶段完成——pingora 先选
    // 上游再改写请求；决策挪到此地前，非宿主副本在 peer 阶段连 127.0.0.1:4000
    // 即 ConnectRefused，Forward 重写永无执行机会）。命中远端宿主 → 记
    // preview_peer/preview_rewrite 并返回宿主 Pingora 面 peer；本机/未命中/
    // 存储不可用 → 落到下方既有本机路径（行为与现状一致）。
    if let Some(deps) = preview_slot.load().as_ref()
        && let PreviewRouteResolution::Forward {
            instance_id,
            port: preview_port,
            host_ip,
        } = deps.coordination.resolve_route(target_port).await
    {
        // /internal/preview-forward 注册在宿主的 Pingora 代理面（非 axum 主
        // API）——转发上游必须用对等副本的 proxy 端口（实测 8086 会 404→502）
        let peer_port = deps.peer_proxy_port;
        ctx.preview_peer = Some((host_ip.clone(), peer_port));
        ctx.preview_rewrite = Some((instance_id, preview_port));
        // token 一并写入 ctx——request 阶段直接消费，免 ArcSwap 二次 load
        ctx.preview_internal_token = Some(deps.internal_token.clone());
        ctx.preview_origin_port = Some(target_port);
        ctx.target_port = Some(peer_port);
        // host_ip 是协调器解析出的对等副本 Pod IP（IP 字面量，非 agent 注册
        // 表键）——直连不经 dial_peer（误接会把 Pod IP 当键查表回退
        // 127.0.0.1，preview_forward_tests 实测暴露）
        let mut peer = HttpPeer::new((host_ip.as_str(), peer_port), false, "".to_string());
        peer.options.connection_timeout = Some(Duration::from_secs(10));
        peer.options.read_timeout = None;
        peer.options.write_timeout = None;
        peer.options.total_connection_timeout = Some(Duration::from_secs(15));
        peer.options.idle_timeout = Some(Duration::from_secs(3600));
        ctx.upstream_host = Some(host_ip);
        // 与 legacy 分支同款指标——response 阶段无条件 dec_active，此处不补
        // 则 Forward 流量的 dec 会吃掉 legacy 的 inc（active_connections 失真）
        metrics.record_request();
        metrics.record_request_port(target_port);
        metrics.inc_active();
        return Ok(Box::new(peer));
    }

    ctx.target_port = Some(target_port);
    metrics.record_request();
    metrics.record_request_port(target_port);
    metrics.inc_active();

    // 未显式配置的端口直接走默认主机，不把请求参数写回全局映射。这样热路径只有一次
    // lock-free 快照读取，也避免大量不同端口请求触发 copy-on-write 放大。
    let resolved_host = backends
        .load()
        .get(&target_port)
        .cloned()
        .unwrap_or_else(|| backend_host.to_string());

    debug!("route: {}:{}", resolved_host, target_port);

    // 创建 HTTP Peer
    let mut peer = HttpPeer::new(
        super::super::upstream::dial_peer(&resolved_host, target_port),
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
