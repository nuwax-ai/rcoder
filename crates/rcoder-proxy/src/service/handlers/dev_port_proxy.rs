//! 开发阶段端口代理处理函数
//!
//! 处理 `/proxy/devapps/{user_id}/{app_id}/{port}/{*path}` 路径的反向代理。
//! upstream 动态解析到**该 app 的开发容器**（UserAppBuilder，per-app）的同端口——
//! 与部署后 `/proxy/apps/*`（app_backends 注册表 → app 运行容器）对称的开发预览入口。
//!
//! 零注册零状态：app_id 经 `ContainerLookup::find_by_project_id`（AppState.projects
//! 内存表 O(1)）解析开发容器 IP；user_id 不参与解析，仅用于日志排障与未来归属
//! 鉴权的锚点。开发容器内自装 pingap 的场景代理 9080 一个端口即整应用入口。

use matchit::Params;
use pingora_core::Result as PingoraResult;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error};

use crate::service::types::{ProxyMetrics, TrackingCtx};
use crate::service::utils;

/// 处理 devapps 代理请求
///
/// 路径格式: `/proxy/devapps/{user_id}/{app_id}/{port}/{*path}` —— 提取三参数，
/// 重写 URI 去掉前缀，设置代理标识头。
pub async fn handle_dev_port_proxy_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
) -> PingoraResult<()> {
    let user_id = params.get("user_id").ok_or_else(|| {
        error!("devapps proxy route missing user_id params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;
    let app_id = params.get("app_id").ok_or_else(|| {
        error!("devapps proxy route missing app_id params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;
    let port_str = params.get("port").ok_or_else(|| {
        error!("devapps proxy route missing port params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;
    let port: u16 = port_str.parse().map_err(|_| {
        error!("devapps parse port failed: {port_str}");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    // strip /proxy/devapps/{user_id}/{app_id}/{port}，保留尾斜杠
    let original_path = original_uri.path();
    let prefix = format!("/proxy/devapps/{user_id}/{app_id}/{port}");
    let target_path = if original_path.len() <= prefix.len() {
        "/".to_string()
    } else {
        original_path[prefix.len()..].to_string()
    };

    debug!(
        "devapps proxy request: user_id={}, app_id={}, port={}, target_path={}",
        user_id, app_id, port, target_path
    );

    upstream_request.insert_header("Host", "127.0.0.1")?;
    let new_uri = utils::rewrite_uri(original_uri, target_path)?;
    upstream_request.set_uri(new_uri);
    utils::set_common_headers(upstream_request)?;
    upstream_request.insert_header("X-Port-Proxy", "pingora-devapps-proxy")?;
    upstream_request.insert_header("X-Target-Port", port.to_string())?;

    Ok(())
}

/// 处理 devapps 代理的上游连接选择
///
/// `find_by_project_id(app_id, UserAppBuilder)` 动态解析该 app 开发容器 IP
/// （trait 校验 service_type 防串用——与 UserApp 运行容器隔离）；无容器 → 502
/// （日志带 user_id/app_id 便于排障）。
pub async fn handle_dev_port_proxy_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    metrics: &Arc<ProxyMetrics>,
    container_lookup: &Option<Arc<dyn shared_types::ContainerLookup>>,
) -> PingoraResult<Box<HttpPeer>> {
    let user_id = params.get("user_id").ok_or_else(|| {
        error!("devapps proxy upstream missing user_id params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;
    let app_id = params.get("app_id").ok_or_else(|| {
        error!("devapps proxy upstream missing app_id params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;
    let port_str = params.get("port").ok_or_else(|| {
        error!("devapps proxy upstream missing port params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;
    let target_port: u16 = port_str.parse().map_err(|_| {
        error!("devapps parse port failed: {port_str}");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    // 端口黑名单：开发容器内跑着 PG/file-server/agent_runner 等基础设施服务，
    // 任意端口直达等于从边缘暴露数据库与命令执行面——只放行开发预览流量。
    if DEVAPPS_BLOCKED_PORTS.contains(&target_port) {
        error!(
            "devapps proxy blocked infrastructure port: user_id={}, app_id={}, port={}",
            user_id, app_id, target_port
        );
        return Err(pingora_core::Error::new(
            pingora_core::ErrorType::HTTPStatus(403),
        ));
    }

    ctx.target_port = Some(target_port);
    metrics.record_request();
    metrics.record_request_port(target_port);

    let dev_container_ip = container_lookup
        .as_ref()
        .and_then(|lookup| {
            lookup.find_by_project_id(app_id, &shared_types::ServiceType::UserAppBuilder)
        })
        .ok_or_else(|| {
            error!(
                "devapps dev container not found: user_id={}, app_id={}, port={}",
                user_id, app_id, target_port
            );
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(502))
        })?;

    // inc_active 放在 peer 构造前（成功路径）：lookup 失败的 502 不会进
    // response_filter（dec_active 只在那里执行），提前 inc 会造成 gauge 单调虚增
    metrics.inc_active();

    debug!(
        "devapps route: user_id={}, app_id={}, {}:{}",
        user_id, app_id, dev_container_ip, target_port
    );

    // 与 app 代理同款 peer（长连接，支持 WebSocket / HMR）
    let mut peer = HttpPeer::new(
        (dev_container_ip.as_str(), target_port),
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

/// devapps 代理的端口黑名单：开发容器内的基础设施端口（PG/file-server/
/// agent_runner HTTP/gRPC/ttyd/VNC/ime），从边缘直达即暴露数据库与命令执行面。
/// 开发预览（dev server 端口池/自装 pingap 9080）不受影响；归属校验为后续项
/// （需调用方身份信息，当前 user_id 仅日志）。
const DEVAPPS_BLOCKED_PORTS: [u16; 7] = [5432, 50051, 60000, 8086, 6080, 17681, 7681];
