//! ttyd 终端代理处理函数
//!
//! 处理 `/computer/ttyd/{user_id}/{project_id}/{*path}` 和
//! `/web/ttyd/{user_id}/{project_id}/{*path}` 路径的 ttyd WebSocket 代理。
//!
//! ttyd 是一个基于 WebSocket 的 Web 终端工具，单端口（7681）同时服务 HTTP 和 WebSocket。
//! Pingora 默认透传所有 header（含 Connection: Upgrade），
//! ttyd 端 libwebsockets 根据 Upgrade 头自动分发到 PTY 协议。

use dashmap::DashMap;
use matchit::Params;
use pingora_core::Result as PingoraResult;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info};

use crate::service::types::{ProxyMetrics, TTYD_PORT, TrackingCtx};
use crate::service::utils;

/// 处理 ttyd Web 终端代理请求
///
/// 路径格式: `/computer/ttyd/{user_id}/{project_id}/{*path}`
///
/// 功能:
/// - 提取 user_id 和 project_id 参数
/// - 重写 URI，去掉路径前缀
/// - 注入 ttyd --url-arg 参数（设置工作目录）
/// - 设置代理标识头
pub async fn handle_ttyd_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
    ctx: &TrackingCtx,
) -> PingoraResult<()> {
    // 从路径参数中提取 user_id 和 project_id
    let user_id = params.get("user_id").ok_or_else(|| {
        error!("ttyd route missing user_id param");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    let project_id = params.get("project_id").ok_or_else(|| {
        error!("ttyd route missing project_id param");
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
        "ttyd request: user_id={}, project_id={}, target_path={}",
        user_id, project_id, target_path
    );

    // 设置 Host 头
    let host = ctx.vnc_target_ip.as_deref().unwrap_or("127.0.0.1");
    upstream_request.insert_header("Host", host)?;

    // 重写 URI
    let new_uri = utils::rewrite_uri(original_uri, target_path)?;

    // 注入 ttyd --url-arg：把 project_id 作为 --cwd 参数传给容器内 wrapper 脚本
    // 容器内挂载: computer-project-workspace/{user_id} → /home/user
    // 所以项目路径 = /home/user/{project_id}
    let new_uri = if !project_id.is_empty()
        && project_id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        let cwd = std::path::Path::new("/home/user").join(project_id);
        let uri_str = new_uri.to_string();
        let separator = if uri_str.contains('?') { '&' } else { '?' };
        let new_uri_str = format!("{}{}arg=--cwd&arg={}", uri_str, separator, cwd.display());
        new_uri_str.parse().map_err(|e| {
            error!("URI rewrite with cwd failed: {}", e);
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?
    } else {
        new_uri
    };

    upstream_request.set_uri(new_uri);

    // 设置代理标识头
    utils::set_common_headers(upstream_request)?;
    upstream_request.insert_header("X-Ttyd-Proxy", "pingora")?;
    upstream_request.insert_header("X-Ttyd-User-Id", user_id)?;
    upstream_request.insert_header("X-Ttyd-Project-Id", project_id)?;

    Ok(())
}

/// 处理 Web ttyd 终端代理请求（代理到动态创建的 RCoder 容器的 ttyd）
///
/// 路径格式: `/web/ttyd/{user_id}/{project_id}/{*path}`
///
/// 与 TtydProxy 的区别：
/// - TtydProxy: 代理到 agent-runner 容器的 ttyd（动态 IP，挂载到 /home/user）
/// - WebTtydProxy: 代理到 rcoder 容器的 ttyd（动态 IP，挂载到 /app/project_workspace）
///
/// 工作目录：/app/project_workspace/{project_id}
pub async fn handle_web_ttyd_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
) -> PingoraResult<()> {
    // 从路径参数中提取 user_id 和 project_id
    let user_id = params.get("user_id").ok_or_else(|| {
        error!("web ttyd route missing user_id param");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    let project_id = params.get("project_id").ok_or_else(|| {
        error!("web ttyd route missing project_id param");
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
        "web ttyd request: user_id={}, project_id={}, target_path={}",
        user_id, project_id, target_path
    );

    // 设置 Host 头（代理到本地）
    upstream_request.insert_header("Host", "127.0.0.1")?;

    // 重写 URI（去掉 /web/ttyd/{user_id}/{project_id} 前缀）
    let new_uri = utils::rewrite_uri(original_uri, target_path)?;

    // 注入 ttyd --url-arg：把 project_id 作为 --cwd 参数
    // rcoder 主服务的工作目录：/app/project_workspace/{project_id}
    let new_uri = if !project_id.is_empty()
        && project_id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        let cwd = std::path::Path::new("/app/project_workspace").join(project_id);
        let uri_str = new_uri.to_string();
        let separator = if uri_str.contains('?') { '&' } else { '?' };
        let new_uri_str = format!("{}{}arg=--cwd&arg={}", uri_str, separator, cwd.display());
        new_uri_str.parse().map_err(|e| {
            error!("URI rewrite with cwd failed: {}", e);
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
        })?
    } else {
        new_uri
    };

    upstream_request.set_uri(new_uri);

    // 设置代理标识头
    utils::set_common_headers(upstream_request)?;
    upstream_request.insert_header("X-Ttyd-Proxy", "pingora-web")?;
    upstream_request.insert_header("X-Ttyd-User-Id", user_id)?;
    upstream_request.insert_header("X-Ttyd-Project-Id", project_id)?;

    Ok(())
}

/// 处理 ttyd 代理的上游连接选择
///
/// 路径格式: `/computer/ttyd/{user_id}/{project_id}/{*path}`
///
/// 功能:
/// - 根据 user_id 查找容器 IP
/// - 创建到容器 ttyd 端口的 HTTP Peer
/// - 配置 WebSocket 长连接优化参数
///
/// 客户端必须传子协议 `tty`（透传到 ttyd，否则 ttyd 不会 fork bash）。
pub async fn handle_ttyd_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    vnc_backends: &Arc<DashMap<String, String>>,
    metrics: &Arc<ProxyMetrics>,
) -> PingoraResult<Box<HttpPeer>> {
    // 从路径参数中提取 user_id
    let user_id = params.get("user_id").ok_or_else(|| {
        error!("ttyd route missing user_id param");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    let project_id = params.get("project_id").unwrap_or("");

    debug!(
        "ttyd proxy request: user_id={}, project_id={}",
        user_id, project_id
    );

    // 查找用户容器 IP（复用 vnc_backends DashMap）
    let container_ip = match vnc_backends.get(user_id) {
        Some(ip_ref) => ip_ref.value().clone(),
        None => {
            info!("routing {} to ttyd", user_id);
            return Err(
                pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404)).more_context(
                    format!(
                        "ttyd backend for user {} not found, please create container first",
                        user_id
                    ),
                ),
            );
        }
    };

    // 记录指标
    metrics.record_request();
    metrics.inc_active();

    // 保存目标 IP 到上下文（用于响应过滤等）
    ctx.vnc_target_ip = Some(container_ip.clone());

    debug!(
        "ttyd proxy: user_id={}, project_id={} -> {}:{}",
        user_id, project_id, container_ip, TTYD_PORT
    );

    // 创建 HTTP Peer 到容器的 ttyd 端口
    // Pingora 会自动处理 WebSocket upgrade
    let mut peer = HttpPeer::new(
        (container_ip.as_str(), TTYD_PORT),
        false,          // 不使用 TLS
        "".to_string(), // SNI
    );

    // ttyd WebSocket 长连接优化配置（与 vnc 一致）
    peer.options.connection_timeout = Some(Duration::from_secs(10));
    peer.options.read_timeout = None; // 无限等待（持续双向流）
    peer.options.write_timeout = None; // 无限等待（WebSocket 双向流）
    peer.options.total_connection_timeout = Some(Duration::from_secs(15));
    peer.options.idle_timeout = Some(Duration::from_secs(3600)); // 1小时空闲超时

    Ok(Box::new(peer))
}

/// 处理 Web ttyd 终端的上游连接（代理到动态创建的 RCoder 容器）
///
/// 路径格式: `/web/ttyd/{user_id}/{project_id}/{*path}`
///
/// 与 TtydProxy 的区别：
/// - TtydProxy: 代理到 agent-runner 容器（动态 IP，通过 user_id 查找）
/// - WebTtydProxy: 代理到 rcoder 容器（动态 IP，通过 user_id 查找）
///
/// 两者都使用 vnc_backends 来查找动态创建的容器 IP
pub async fn handle_web_ttyd_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    vnc_backends: &Arc<DashMap<String, String>>,
    project_backends: &Arc<DashMap<String, String>>,
    metrics: &Arc<ProxyMetrics>,
) -> PingoraResult<Box<HttpPeer>> {
    let user_id = params.get("user_id").unwrap_or("unknown");
    let project_id = params.get("project_id").unwrap_or("");

    debug!(
        "web ttyd proxy: user_id={}, project_id={}",
        user_id, project_id
    );

    // 使用 project_id 查找容器 IP（优先使用 project_backends，其次使用 vnc_backends）
    let container_ip = project_backends
        .get(project_id)
        .or_else(|| vnc_backends.get(project_id))
        .map(|ip_ref| ip_ref.value().clone());

    let container_ip = match container_ip {
        Some(ip) => ip,
        None => {
            info!("routing {} to web ttyd", project_id);
            return Err(
                pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404)).more_context(
                    format!(
                        "web ttyd backend for project {} not found, please create container first",
                        project_id
                    ),
                ),
            );
        }
    };

    // 记录指标
    metrics.record_request();
    metrics.inc_active();

    // 保存目标 IP 到上下文（用于响应过滤等）
    ctx.vnc_target_ip = Some(container_ip.clone());

    debug!(
        "web ttyd proxy: user_id={}, project_id={} -> {}:{}",
        user_id, project_id, container_ip, TTYD_PORT
    );

    // 创建 HTTP Peer 到容器的 ttyd 端口
    let mut peer = HttpPeer::new(
        (container_ip.as_str(), TTYD_PORT),
        false,          // 不使用 TLS
        "".to_string(), // SNI
    );

    // ttyd WebSocket 长连接优化配置（与 agent-runner ttyd 一致）
    peer.options.connection_timeout = Some(Duration::from_secs(10));
    peer.options.read_timeout = None;
    peer.options.write_timeout = None;
    peer.options.total_connection_timeout = Some(Duration::from_secs(15));
    peer.options.idle_timeout = Some(Duration::from_secs(3600));

    Ok(Box::new(peer))
}
