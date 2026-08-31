//! userApp 开发应用流量代理（免端口）
//!
//! 处理 `/proxy/userapp/dev/{user_id}/{app_id}/{*path}` 路径的反向代理：
//! upstream 动态解析到**该 app 的开发容器**（UserappBuilder，per-app）的
//! pingap 统一入口 `APP_ENTRY_PORT`(9080)——与部署后 `/proxy/userapp/prod/*`
//! （app_backends 注册表 → app 运行容器）对称的开发预览入口，切环境只改
//! `dev→prod` 一段，调用方无需传端口。
//!
//! 零注册零状态：app_id 经 `ContainerLookup::find_by_project_id`（AppState.projects
//! 内存表 O(1)）解析开发容器；user_id 不参与解析，仅日志/归属锚点。
//! 开发容器 manifest 流程（file-server `start_dev_manifest`）恒起 pingap 9080。

use matchit::Params;
use pingora_core::Result as PingoraResult;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error};

use crate::service::types::{ProxyMetrics, TrackingCtx};
use crate::service::utils;

/// 处理开发应用流量代理请求
///
/// 路径格式: `/proxy/userapp/dev/{user_id}/{app_id}/{*path}` —— 提取参数，
/// 重写 URI 去掉前缀（免端口：固定 APP_ENTRY_PORT），设置代理标识头。
pub async fn handle_dev_app_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
) -> PingoraResult<()> {
    let user_id = params.get("user_id").ok_or_else(|| {
        error!("dev app proxy route missing user_id params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;
    let app_id = params.get("app_id").ok_or_else(|| {
        error!("dev app proxy route missing app_id params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    // strip /proxy/userapp/dev/{user_id}/{app_id}，保留尾斜杠
    let original_path = original_uri.path();
    let prefix = format!("/proxy/userapp/dev/{user_id}/{app_id}");
    let target_path = if original_path.len() <= prefix.len() {
        "/".to_string()
    } else {
        original_path[prefix.len()..].to_string()
    };

    debug!(
        "dev app proxy request: user_id={}, app_id={}, target_path={}",
        user_id, app_id, target_path
    );

    upstream_request.insert_header("Host", "127.0.0.1")?;
    let new_uri = utils::rewrite_uri(original_uri, target_path)?;
    upstream_request.set_uri(new_uri);
    utils::set_common_headers(upstream_request)?;
    upstream_request.insert_header("X-Port-Proxy", "pingora-userapp-dev")?;
    Ok(())
}

/// 处理开发应用流量代理的上游连接选择
///
/// `find_by_project_id(app_id, UserappBuilder)` 动态解析该 app 开发容器 IP
/// （trait 校验 service_type 防串用——与 Userapp 运行容器隔离），固定拨
/// `APP_ENTRY_PORT`(9080)；无容器 → 502（日志带 user_id/app_id 便于排障）。
pub async fn handle_dev_app_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    metrics: &Arc<ProxyMetrics>,
    container_lookup: &Option<Arc<dyn shared_types::ContainerLookup>>,
) -> PingoraResult<Box<HttpPeer>> {
    let user_id = params.get("user_id").ok_or_else(|| {
        error!("dev app proxy upstream missing user_id params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;
    let app_id = params.get("app_id").ok_or_else(|| {
        error!("dev app proxy upstream missing app_id params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    ctx.target_port = Some(shared_types::APP_ENTRY_PORT);
    metrics.record_request();
    metrics.record_request_port(shared_types::APP_ENTRY_PORT);

    let dev_container_ip = container_lookup
        .as_ref()
        .and_then(|lookup| {
            lookup.find_by_project_id(app_id, &shared_types::ServiceType::UserappBuilder)
        })
        .ok_or_else(|| {
            error!(
                "dev app container not found: user_id={}, app_id={}",
                user_id, app_id
            );
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(502))
        })?;

    // inc_active 放在 peer 构造前（成功路径）：lookup 失败的 502 不会进
    // response_filter（dec_active 只在那里执行），提前 inc 会造成 gauge 单调虚增
    metrics.inc_active();

    debug!(
        "dev app route: user_id={}, app_id={}, {}:{}",
        user_id,
        app_id,
        dev_container_ip,
        shared_types::APP_ENTRY_PORT
    );

    // 与 prod app 代理同款 peer（长连接，支持 WebSocket / HMR）
    let mut peer = HttpPeer::new(
        (dev_container_ip.as_str(), shared_types::APP_ENTRY_PORT),
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
