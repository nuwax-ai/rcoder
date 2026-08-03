//! 音频流代理处理函数
//!
//! 处理 `/computer/audio/{user_id}/{project_id}/{*path}` 路径的音频代理。
//!
//! 音频服务采用双端口架构：
//! - **HTTP 端口 (6090)**: 静态文件服务（音频资源、配置等）
//! - **WebSocket 端口 (6089)**: 实时音频流传输
//!
//! 根据请求路径自动判断目标端口：
//! - `path == "ws"` 或 `path.starts_with("ws/")` → WebSocket 端口 6089
//! - 其他（包括空路径）→ HTTP 端口 6090

use dashmap::DashMap;
use matchit::Params;
use pingora_core::Result as PingoraResult;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::service::types::{AUDIO_HTTP_PORT, AUDIO_WS_PORT, ProxyMetrics, TrackingCtx};
use crate::service::utils;

/// 根据路径判断音频服务端口和目标路径
///
/// # 端口判断逻辑
/// - `path == "ws"` 或 `path.starts_with("ws/")` → WebSocket 端口 6089
/// - 其他（包括空路径）→ HTTP 端口 6090
///
/// # 返回
/// (目标端口, 标准化后的目标路径)
pub fn determine_audio_port_and_path(path: &str) -> (u16, String) {
    if path == "ws" || path.starts_with("ws/") {
        (AUDIO_WS_PORT, format!("/{}", path))
    } else {
        let normalized = if path.is_empty() { "/" } else { path };
        (
            AUDIO_HTTP_PORT,
            format!("/{}", normalized.trim_start_matches('/')),
        )
    }
}

/// 处理音频流代理请求
///
/// 路径格式: `/computer/audio/{user_id}/{project_id}/{*path}`
///
/// 功能:
/// - 提取 user_id 和 project_id 参数
/// - 根据路径判断目标端口（HTTP 或 WebSocket）
/// - 查找容器 IP 并设置上下文信息
/// - 重写 URI，去掉路径前缀
/// - 设置代理标识头
pub async fn handle_audio_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
    ctx: &mut TrackingCtx,
    vnc_backends: &Arc<DashMap<String, String>>,
) -> PingoraResult<()> {
    // 提取参数
    let user_id = params.get("user_id").ok_or_else(|| {
        error!("audio route missing user_id param");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    let project_id = params.get("project_id").ok_or_else(|| {
        error!("audio route missing project_id param");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    // 标准化路径
    let remaining_path = match params.get("path") {
        Some(p) if !p.is_empty() => p,
        _ => "",
    };

    // 判断目标端口和路径（使用辅助函数）
    let (target_port, target_path) = determine_audio_port_and_path(remaining_path);

    // 获取容器 IP（复用 VNC 的容器 IP 解析机制）
    let container_ip = vnc_backends
        .get(user_id)
        .map(|entry| entry.value().clone())
        .ok_or_else(|| {
            warn!("[AUDIO] container not found: user_id={}", user_id);
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404)).more_context(
                format!(
                    "audio backend for user {} not found, please create container first",
                    user_id
                ),
            )
        })?;

    // 记录上下文
    ctx.target_port = Some(target_port);
    ctx.upstream_host = Some(format!("{}:{}", container_ip, target_port));

    info!(
        "[AUDIO] Audio proxy: user_id={}, project_id={}, path={}, target={}:{}",
        user_id, project_id, remaining_path, container_ip, target_port
    );

    // 设置 Host 头
    upstream_request.insert_header("Host", &container_ip)?;

    // 重写 URI
    let new_uri = utils::rewrite_uri(original_uri, target_path)?;
    upstream_request.set_uri(new_uri);

    // 设置代理标识头
    utils::set_common_headers(upstream_request)?;
    upstream_request.insert_header("X-Audio-Proxy", "pingora")?;
    upstream_request.insert_header("X-Audio-User-Id", user_id)?;
    upstream_request.insert_header("X-Audio-Project-Id", project_id)?;

    Ok(())
}

/// 处理音频流的上游连接
///
/// 功能:
/// - 根据 user_id 查找容器 IP
/// - 根据路径判断目标端口（HTTP 6090 或 WebSocket 6089）
/// - 创建到容器音频端口的 HTTP Peer
/// - 配置长连接优化参数（音频流可能持续数小时）
pub async fn handle_audio_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    vnc_backends: &Arc<DashMap<String, String>>,
    metrics: &Arc<ProxyMetrics>,
) -> PingoraResult<Box<HttpPeer>> {
    let user_id = params.get("user_id").ok_or_else(|| {
        error!("audio route missing user_id param");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    let remaining_path = match params.get("path") {
        Some(p) if !p.is_empty() => p,
        _ => "",
    };

    // 判断目标端口（使用辅助函数与 request 阶段保持一致）
    let (target_port, _) = determine_audio_port_and_path(remaining_path);

    let container_ip = vnc_backends
        .get(user_id)
        .map(|entry| entry.value().clone())
        .ok_or_else(|| {
            warn!("[AUDIO] container not found: user_id={}", user_id);
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404))
                .more_context(format!("audio backend for user {} not found", user_id))
        })?;

    // 记录指标
    metrics.record_request();
    metrics.record_request_port(target_port);
    metrics.inc_active();

    // 保存音频目标 IP 到上下文（用于响应过滤）
    ctx.vnc_target_ip = Some(container_ip.clone());

    let peer_addr = format!("{}:{}", container_ip, target_port);
    let mut peer = Box::new(HttpPeer::new(
        (container_ip.as_str(), target_port),
        false,          // 不使用 TLS
        "".to_string(), // SNI
    ));

    // 音频流长连接优化配置
    // 音频流可能持续数小时，需要宽松的超时设置
    peer.options.connection_timeout = Some(Duration::from_secs(10));
    peer.options.read_timeout = None; // 无限等待（音频流可能持续数小时）
    peer.options.write_timeout = None; // 无限等待（WebSocket 双向流）
    peer.options.total_connection_timeout = Some(Duration::from_secs(15));
    peer.options.idle_timeout = Some(Duration::from_secs(3600)); // 1 小时空闲超时

    debug!("[AUDIO] connection to: {}", peer_addr);

    Ok(peer)
}
