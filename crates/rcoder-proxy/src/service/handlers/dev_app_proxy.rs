//! userApp 开发应用流量代理（免端口）
//!
//! 处理 `/api/v1/userapp/proxy/app/dev/{user_id}/{app_id}/{*path}` 路径的反向代理：
//! upstream 动态解析到**该 app 的开发容器**（UserappBuilder，per-app）的
//! pingap 统一入口 `APP_ENTRY_PORT`(9080)——与部署后 `/api/v1/userapp/proxy/app/prod/*`
//! （app_backends 注册表 → app 运行容器）对称的开发预览入口，切环境只改
//! `dev→prod` 一段，调用方无需传端口。
//!
//! 零注册：app_id 经 `ContainerLookup::find_by_project_id`（AppState.projects
//! 内存表 O(1)）解析开发容器；user_id 仅日志/归属锚点与懒启动 owner 档。
//! 开发容器 manifest 流程（file-server `start_dev_manifest`）恒起 pingap 9080。
//! **容器不在时懒启动**：与其他 dev 工具族（终端/ttyd/vnc/ime）同款——
//! `find_dev_container` 经 `UserappDevEnsure` 回调自动 ensure 创建后再拨，
//! 应用流量不再"无容器即 502"（访问即拉起容器；服务本体仍需 dev/start 编排）。

use matchit::Params;
use pingora_core::Result as PingoraResult;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;
use std::time::Duration;
use tracing::{debug, error};

use super::dev_terminal::find_dev_container;
use crate::service::types::TrackingCtx;
use crate::service::utils;

/// 处理开发应用流量代理请求
///
/// 路径格式: `/api/v1/userapp/proxy/app/dev/{user_id}/{app_id}/{*path}` —— 提取参数，
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

    // strip /api/v1/userapp/proxy/app/dev/{user_id}/{app_id}，保留尾斜杠
    let original_path = original_uri.path();
    let prefix = format!("/api/v1/userapp/proxy/app/dev/{user_id}/{app_id}");
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
/// `find_dev_container`（lookup + 懒启动回调）动态解析该 app 开发容器地址
/// （trait 校验 service_type 防串用——与 Userapp 运行容器隔离），固定拨
/// `APP_ENTRY_PORT`(9080)；容器不在时自动 ensure 创建（同终端族懒启动语义），
/// ensure 失败（如未建工作区）404 带指引（日志含 user_id/app_id 便于排障）。
pub async fn handle_dev_app_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    deps: &super::dev_terminal::DevProxyDeps<'_>,
) -> PingoraResult<Box<HttpPeer>> {
    let user_id = params.get("user_id").ok_or_else(|| {
        debug!("dev app proxy upstream missing user_id params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;
    let app_id = params.get("app_id").ok_or_else(|| {
        debug!("dev app proxy upstream missing app_id params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    ctx.target_port = Some(shared_types::APP_ENTRY_PORT);
    deps.metrics.record_request();
    deps.metrics
        .record_request_port(shared_types::APP_ENTRY_PORT);

    let dev_container_ip = find_dev_container(
        deps.container_lookup,
        deps.dev_ensure,
        app_id,
        user_id,
        shared_types::APP_ENTRY_PORT,
    )
    .await?;

    // inc_active 放在 peer 构造前（成功路径）：lookup 失败的 502 不会进
    // response_filter（dec_active 只在那里执行），提前 inc 会造成 gauge 单调虚增
    deps.metrics.inc_active();

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
