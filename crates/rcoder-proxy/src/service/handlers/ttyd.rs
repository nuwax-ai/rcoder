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
use shared_types::WS_TERMINAL_PORT;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::service::types::{ProxyMetrics, TrackingCtx};
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

    // 校验标识符合法性（防 header 注入：仅允许字母数字 _ -，非空且 <=64 字符）
    if let Err(e) = shared_types::validate_identifier(user_id, "user_id") {
        warn!("[TTYD] invalid user_id: {}", e);
        return Err(pingora_core::Error::new(
            pingora_core::ErrorType::HTTPStatus(400),
        ));
    }
    if let Err(e) = shared_types::validate_identifier(project_id, "project_id") {
        warn!("[TTYD] invalid project_id: {}", e);
        return Err(pingora_core::Error::new(
            pingora_core::ErrorType::HTTPStatus(400),
        ));
    }

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

    // 重写 URI（去掉 /computer/ttyd/{user_id}/{project_id} 前缀）
    //
    // 注：`arg=--cwd` 不再由 Pingora 注入。cd 改由 agent_runner 的 WS 中间层代码控制
    // （见 `agent_runner/src/ws_terminal`：每次连接含重连都 connect 本地 ttyd 并注入 arg），
    // 这样彻底摆脱了「upstream_request_filter 对 WS 只首次触发」导致的重连不 cd 问题。
    // project_id 通过下方 `X-Ttyd-Project-Id` header 传递给 agent_runner。
    let new_uri = utils::rewrite_uri(original_uri, target_path)?;
    upstream_request.set_uri(new_uri);

    // 设置代理标识头
    utils::set_common_headers(upstream_request)?;
    upstream_request.insert_header("X-Ttyd-Proxy", "pingora")?;
    upstream_request.insert_header("X-Ttyd-User-Id", user_id)?;
    upstream_request.insert_header("X-Ttyd-Project-Id", project_id)?;
    // 告知 agent_runner 业务场景（ServiceType 的 Display = kebab-case），用于显式选 cwd 前缀
    upstream_request.insert_header(
        "X-Ttyd-Service-Type",
        shared_types::ServiceType::ComputerAgentRunner.to_string(),
    )?;

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
    container_lookup: &Option<Arc<dyn shared_types::ContainerLookup>>,
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

    // 校验标识符合法性（防 header 注入：仅允许字母数字 _ -，非空且 <=64 字符）
    if let Err(e) = shared_types::validate_identifier(user_id, "user_id") {
        warn!("[WEB TTYD] invalid user_id: {}", e);
        return Err(pingora_core::Error::new(
            pingora_core::ErrorType::HTTPStatus(400),
        ));
    }
    if let Err(e) = shared_types::validate_identifier(project_id, "project_id") {
        warn!("[WEB TTYD] invalid project_id: {}", e);
        return Err(pingora_core::Error::new(
            pingora_core::ErrorType::HTTPStatus(400),
        ));
    }

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
    //
    // 注：`arg=--cwd` 不再由 Pingora 注入（同 computer ttyd）。cd 改由 agent_runner 的
    // WS 中间层代码控制（见 agent_runner/src/ws_terminal：cwd.rs 自动检测
    // `/home/user` 与 `/app/project_workspace` 两前缀）。project_id 通过下方
    // `X-Ttyd-Project-Id` header 传递给 agent_runner。
    let new_uri = utils::rewrite_uri(original_uri, target_path)?;
    upstream_request.set_uri(new_uri);

    // 设置代理标识头
    utils::set_common_headers(upstream_request)?;
    upstream_request.insert_header("X-Ttyd-Proxy", "pingora-web")?;
    upstream_request.insert_header("X-Ttyd-User-Id", user_id)?;
    upstream_request.insert_header("X-Ttyd-Project-Id", project_id)?;
    // 告知 agent_runner 业务场景（ServiceType 的 Display = kebab-case），用于显式选 cwd 前缀
    upstream_request.insert_header(
        "X-Ttyd-Service-Type",
        shared_types::ServiceType::WebAgentRunner.to_string(),
    )?;

    // 按 project_id 反查项目归属 scope，注入 tenant/space header，供 agent_runner 解析
    // 共享容器（tenant/space 隔离）的三级 cwd。反查失败（None）则不注入，agent_runner
    // 侧安全降级为单级路径（见 ws_terminal::cwd）。
    if let Some(lookup) = container_lookup {
        if let Some(scope) =
            lookup.find_project_scope(project_id, &shared_types::ServiceType::WebAgentRunner)
        {
            if let Some(tid) = scope.tenant_id {
                upstream_request.insert_header("X-Ttyd-Tenant-Id", tid)?;
            }
            if let Some(sid) = scope.space_id {
                upstream_request.insert_header("X-Ttyd-Space-Id", sid)?;
            }
        } else {
            debug!(
                "[WEB TTYD] find_project_scope miss: project_id={} (agent_runner will fall back to single-level cwd)",
                project_id
            );
        }
    }

    Ok(())
}

/// 处理 ttyd 代理的上游连接选择
///
/// 路径格式: `/computer/ttyd/{user_id}/{project_id}/{*path}`
///
/// 功能:
/// - 根据 user_id 查找容器 IP
/// - 创建到容器 agent_runner WS 终端中间层的 HTTP Peer（端口 `WS_TERMINAL_PORT`=17681）
/// - 配置 WebSocket 长连接优化参数
///
/// 注意：上游是 agent_runner 的 ws_terminal（不是 ttyd 本体）。ws_terminal 协商 `tty`
/// 子协议后，再代理到容器内 ttyd（7681）。客户端仍须传子协议 `tty`。
pub async fn handle_ttyd_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    vnc_backends: &Arc<DashMap<String, String>>,
    metrics: &Arc<ProxyMetrics>,
    container_lookup: &Option<Arc<dyn shared_types::ContainerLookup>>,
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

    // 查找用户容器 IP
    // 优先使用 ContainerLookupService（校验 service_type，防串用），回退到 vnc_backends
    let container_ip = if let Some(lookup) = container_lookup {
        lookup.find_by_user_id(user_id, &shared_types::ServiceType::ComputerAgentRunner)
    } else {
        vnc_backends.get(user_id).map(|r| r.value().clone())
    };

    let container_ip = match container_ip {
        Some(ip) => ip,
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
        user_id, project_id, container_ip, WS_TERMINAL_PORT
    );

    // 创建 HTTP Peer 到容器 agent_runner 的 WS 终端中间层端口（ttyd 本体仍 7681，由 agent_runner 内部连接）
    // Pingora 会自动处理 WebSocket upgrade
    let mut peer = HttpPeer::new(
        (container_ip.as_str(), WS_TERMINAL_PORT),
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

/// 处理 Web ttyd 终端的上游连接（代理到动态创建的 WebAgentRunner 容器）
///
/// 路径格式: `/web/ttyd/{user_id}/{project_id}/{*path}`
///
/// WebAgentRunner 容器使用 rcoder 镜像，但内部跑的也是 agent-runner 模块
/// （含 ws_terminal，监听 `WS_TERMINAL_PORT`=17681），与 ComputerAgentRunner 同构。
///
/// 与 TtydProxy 的区别仅在后端查找键：
/// - TtydProxy（computer）：按 user_id 查 vnc_backends
/// - WebTtydProxy（web）：按 project_id 优先查 project_backends，回退 vnc_backends
pub async fn handle_web_ttyd_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    vnc_backends: &Arc<DashMap<String, String>>,
    project_backends: &Arc<DashMap<String, String>>,
    metrics: &Arc<ProxyMetrics>,
    container_lookup: &Option<Arc<dyn shared_types::ContainerLookup>>,
) -> PingoraResult<Box<HttpPeer>> {
    let user_id = params.get("user_id").unwrap_or("unknown");
    let project_id = params.get("project_id").unwrap_or("");

    debug!(
        "web ttyd proxy: user_id={}, project_id={}",
        user_id, project_id
    );

    // 使用 project_id 查找容器 IP
    // 优先使用 ContainerLookupService（校验 service_type，防串用），回退到 project_backends/vnc_backends
    let container_ip = if let Some(lookup) = container_lookup {
        lookup.find_by_project_id(project_id, &shared_types::ServiceType::WebAgentRunner)
    } else {
        project_backends
            .get(project_id)
            .or_else(|| vnc_backends.get(project_id))
            .map(|ip_ref| ip_ref.value().clone())
    };

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
        user_id, project_id, container_ip, WS_TERMINAL_PORT
    );

    // 创建 HTTP Peer 到容器 agent_runner 的 WS 终端中间层端口（ttyd 本体仍 7681，由 agent_runner 内部连接）
    let mut peer = HttpPeer::new(
        (container_ip.as_str(), WS_TERMINAL_PORT),
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
