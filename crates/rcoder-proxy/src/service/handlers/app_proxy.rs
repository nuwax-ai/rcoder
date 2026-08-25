//! userApp 生产应用流量代理（免端口）
//!
//! 处理 `/proxy/userapp/prod/{user_id}/{app_id}/{*path}` 路径的反向代理：
//! 按 app_id 查 `app_backends` 注册表（app_manager 部署时注册），内部固定拨
//! pingap 统一入口 `APP_ENTRY_PORT`(9080)——调用方无需传端口。
//!
//! 回退：未注册 9080 且该 app 恰只有一个已注册 HTTP 端口时用之（防御直接
//! REST create 声明自定义端口的 app；release 流程恒 pin 9080，正常路径不走此分支）。
//! user_id 不参与后端解析（app_backends 按 (app_id, port)），仅日志/归属锚点。

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

/// 处理生产应用流量代理请求
///
/// 路径格式: `/proxy/userapp/prod/{user_id}/{app_id}/{*path}` —— 提取 app_id，
/// 重写 URI 去掉前缀（免端口：上游端口在 upstream 阶段解析），设置代理标识头。
pub async fn handle_prod_app_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
) -> PingoraResult<()> {
    let user_id = params.get("user_id").unwrap_or("");
    let app_id = params.get("app_id").ok_or_else(|| {
        error!("prod app proxy route missing app_id params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    // 从原始 URI 提取剩余路径（strip /proxy/userapp/prod/{user_id}/{app_id}，保留尾斜杠）
    let original_path = original_uri.path();
    let prefix = format!("/proxy/userapp/prod/{user_id}/{app_id}");
    let target_path = if original_path.len() <= prefix.len() {
        "/".to_string()
    } else {
        original_path[prefix.len()..].to_string()
    };

    debug!(
        "prod app proxy request: user_id={}, app_id={}, target_path={}",
        user_id, app_id, target_path
    );

    upstream_request.insert_header("Host", "127.0.0.1")?;
    let new_uri = utils::rewrite_uri(original_uri, target_path)?;
    upstream_request.set_uri(new_uri);
    utils::set_common_headers(upstream_request)?;
    upstream_request.insert_header("X-Port-Proxy", "pingora-userapp-prod")?;
    Ok(())
}

/// 处理生产应用流量代理的上游连接选择
///
/// 按 (app_id, APP_ENTRY_PORT) 查 `app_backends`；未命中且该 app 恰只有一个
/// 已注册端口时回退用之；再未命中 → 502（Fail Fast——app 专用路径不能像通用
/// /proxy/{port} 那样猜 host，否则会路由到错误 app）。
pub async fn handle_prod_app_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    app_backends: &Arc<DashMap<(String, u16), String>>,
    metrics: &Arc<ProxyMetrics>,
) -> PingoraResult<Box<HttpPeer>> {
    let app_id = params.get("app_id").ok_or_else(|| {
        error!("prod app proxy upstream missing app_id params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    metrics.record_request();

    // 主路径：pingap 统一入口；回退：该 app 唯一已注册端口
    let (resolved_host, resolved_port) = if let Some(host) =
        app_backends.get(&(app_id.to_string(), shared_types::APP_ENTRY_PORT))
    {
        (host.value().clone(), shared_types::APP_ENTRY_PORT)
    } else {
        let mut candidates = app_backends.iter().filter(|e| e.key().0 == app_id);
        let sole = match (candidates.next(), candidates.next()) {
            (Some(e), None) => Some((e.value().clone(), e.key().1)),
            _ => None,
        };
        match sole {
            Some((host, port)) => {
                debug!(
                    "prod app fallback to sole registered port: app_id={}, port={}",
                    app_id, port
                );
                (host, port)
            }
            None => {
                error!(
                    "prod app backend not registered: app_id={} (expected APP_ENTRY_PORT={})",
                    app_id,
                    shared_types::APP_ENTRY_PORT
                );
                return Err(pingora_core::Error::new(
                    pingora_core::ErrorType::HTTPStatus(502),
                ));
            }
        }
    };

    ctx.target_port = Some(resolved_port);
    metrics.record_request_port(resolved_port);
    // inc_active 放在解析成功后（对齐 dev_app_proxy：502 不进 response_filter，
    // 提前 inc 会造成 gauge 单调虚增）
    metrics.inc_active();

    debug!(
        "prod app route: app_id={}, {}:{}",
        app_id, resolved_host, resolved_port
    );

    // 创建 HTTP Peer（长连接配置，支持 WebSocket / HMR）
    let mut peer = HttpPeer::new(
        (resolved_host.as_str(), resolved_port),
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
