//! DBX 数据库 Web GUI 两阶段代理（`/userapp/{dev,prod}/dbx/{user_id}/{app_id}` 工具族）。
//!
//! dbx-web（60+ 数据库 GUI，supervisor 恒起 `DBX_PORT`=4224）的开发/生产双入口：
//! - **dev**：UserappBuilder 开发容器（agent-runner 镜像）——注册表
//!   `find_by_project_id(app_id, UserappBuilder)`，未建 workspace → 404（同 dev 终端族）；
//! - **prod**：Userapp 运行容器（app-runtime 镜像）——`find_app_runtime_addr`
//!   确定性命名构造，未部署/停止 → 连接失败 502（同 runtime 终端族）。
//!
//! 代理剥前缀直连 root 模式 dbx：dbx 前端 `webPath.ts` 从
//! location.pathname 运行时推断 base，index.html 全相对引用，API/WS 调用
//! 自动拼回 `/userapp/{stage}/dbx/{app_id}` 前缀——无需容器侧配置
//! `DBX_PUBLIC_BASE_PATH`。WebSocket（redis pubsub 等）由 Pingora 透传。

use std::sync::Arc;
use std::time::Duration;

use matchit::Params;
use pingora_core::Result as PingoraResult;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;
use tracing::debug;

use crate::service::handlers::dev_terminal::{
    find_dev_container, find_runtime_addr, require_app_id, require_user_id, runtime_target_path_of,
};
use crate::service::types::{ProxyMetrics, TrackingCtx};
use crate::service::utils;

/// 请求重写共用体：剥前缀 + Host + 通用头（两阶段仅日志 tag 不同）。
async fn dbx_rewrite_request(
    tag: &str,
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
    ctx: &TrackingCtx,
) -> PingoraResult<()> {
    let app_id = require_app_id(&params)?;
    let target_path = runtime_target_path_of(&params);
    debug!("[{tag}] app_id={}, target_path={}", app_id, target_path);

    let host = ctx.vnc_target_ip.as_deref().unwrap_or("127.0.0.1");
    upstream_request.insert_header("Host", host)?;
    let new_uri = utils::rewrite_uri(original_uri, target_path)?;
    upstream_request.set_uri(new_uri);
    utils::set_common_headers(upstream_request)?;
    Ok(())
}

/// 构造 dbx 上游 peer（长会话同款参数：conn 10s / 无读写超时 / idle 3600s）。
fn dbx_peer(container_addr: &str) -> Box<HttpPeer> {
    let mut peer = HttpPeer::new(
        (container_addr, shared_types::DBX_PORT),
        false,
        "".to_string(),
    );
    peer.options.connection_timeout = Some(Duration::from_secs(10));
    peer.options.read_timeout = None;
    peer.options.write_timeout = None;
    peer.options.total_connection_timeout = Some(Duration::from_secs(15));
    peer.options.idle_timeout = Some(Duration::from_secs(3600));
    Box::new(peer)
}

/// `/api/v1/userapp/proxy/dbx/dev/{user_id}/{app_id}/{*path}` 请求重写（定位在 upstream 阶段完成）。
pub async fn handle_dev_dbx_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
    ctx: &TrackingCtx,
) -> PingoraResult<()> {
    dbx_rewrite_request("DEV_DBX", upstream_request, original_uri, params, ctx).await
}

/// dev dbx 上游：注册表定位 UserappBuilder 开发容器 + 直连 DBX_PORT=4224。
pub async fn handle_dev_dbx_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    metrics: &Arc<ProxyMetrics>,
    container_lookup: &Option<Arc<dyn shared_types::ContainerLookup>>,
    dev_ensure: &arc_swap::ArcSwapOption<Arc<dyn shared_types::UserappDevEnsure>>,
) -> PingoraResult<Box<HttpPeer>> {
    let app_id = require_app_id(&params)?;
    let user_id = require_user_id(&params)?;
    let container_addr =
        find_dev_container(container_lookup, dev_ensure, &app_id, &user_id).await?;

    metrics.record_request();
    metrics.inc_active();
    ctx.vnc_target_ip = Some(container_addr.clone());
    debug!(
        "[DEV_DBX] app_id={}, user_id={} -> {}:{}",
        app_id,
        user_id,
        container_addr,
        shared_types::DBX_PORT
    );
    Ok(dbx_peer(&container_addr))
}

/// `/api/v1/userapp/proxy/dbx/prod/{user_id}/{app_id}/{*path}` 请求重写。
pub async fn handle_prod_dbx_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
    ctx: &TrackingCtx,
) -> PingoraResult<()> {
    dbx_rewrite_request("PROD_DBX", upstream_request, original_uri, params, ctx).await
}

/// prod dbx 上游：确定性命名定位 Userapp 运行容器 + 直连 DBX_PORT=4224。
pub async fn handle_prod_dbx_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    metrics: &Arc<ProxyMetrics>,
    container_lookup: &Option<Arc<dyn shared_types::ContainerLookup>>,
    ip_slot: &arc_swap::ArcSwapOption<Arc<dyn shared_types::AppRuntimeIpResolver>>,
) -> PingoraResult<Box<HttpPeer>> {
    let app_id = require_app_id(&params)?;
    let user_id = require_user_id(&params)?;
    let container_addr = find_runtime_addr(ip_slot, container_lookup, &app_id).await?;

    metrics.record_request();
    metrics.inc_active();
    ctx.vnc_target_ip = Some(container_addr.clone());
    debug!(
        "[PROD_DBX] app_id={}, user_id={} -> {}:{}",
        app_id,
        user_id,
        container_addr,
        shared_types::DBX_PORT
    );
    Ok(dbx_peer(&container_addr))
}
