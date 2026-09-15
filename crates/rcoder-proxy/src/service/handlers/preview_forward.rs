//! Custom Page 预览转发宿主侧入口（`/internal/preview-forward/*`）。
//!
//! 校验与短路在 `request_filter`（见 proxy_http.rs——404/410/503 需要短路能力，
//! `upstream_request_filter` 只能出错 502）；本模块只做校验通过后的
//! 路径重写与上游选择：剥离内部前缀 → 剩余路径原样（query/base/HMR ws
//! 语义与单跳 `/proxy/{port}` 完全一致）→ `127.0.0.1:{port}`。

use matchit::Params;
use pingora_core::Result as PingoraResult;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;
use std::sync::Arc;
use std::time::Duration;

use crate::service::types::TrackingCtx;
use crate::service::utils;

/// 内部前缀剥除 + URI 重写（校验已过；port 来自 ctx——request_filter 已记录）。
pub(crate) fn handle_preview_forward_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: &Params<'_, '_>,
    ctx: &mut TrackingCtx,
) -> PingoraResult<()> {
    let Some(vite_port) = ctx.preview_forward_port else {
        // request_filter 未放行却到达此处的路径防御（不可达；错误码 502）
        return Err(pingora_core::Error::new(
            pingora_core::ErrorType::HTTPStatus(503),
        ));
    };
    // 从原始 URI 提取剩余路径（保留尾斜杠与 query；与 port_proxy 同规则）
    let instance = params.get("instance_id").unwrap_or_default();
    let port_seg = params.get("port").unwrap_or_default();
    let prefix = format!("/internal/preview-forward/{instance}/{port_seg}");
    let original_path = original_uri.path();
    let rest = if original_path.len() <= prefix.len() {
        "/".to_string()
    } else {
        original_path[prefix.len()..].to_string()
    };
    let new_uri = utils::rewrite_uri(original_uri, rest)?;
    upstream_request.set_uri(new_uri);
    // 终跳 Host 对齐既有 port_proxy 行为（vite 不感知内部转发）
    upstream_request.insert_header("Host", "127.0.0.1")?;
    ctx.target_port = Some(vite_port);
    Ok(())
}

/// 上游选择：本机 vite（长连接配置与 port_proxy 一致——HMR ws 依赖）。
pub(crate) async fn handle_preview_forward_upstream(
    ctx: &mut TrackingCtx,
) -> PingoraResult<Box<HttpPeer>> {
    let Some(vite_port) = ctx.preview_forward_port else {
        return Err(pingora_core::Error::new(
            pingora_core::ErrorType::HTTPStatus(503),
        ));
    };
    let mut peer = HttpPeer::new(("127.0.0.1", vite_port), false, "".to_string());
    peer.options.connection_timeout = Some(Duration::from_secs(10));
    peer.options.read_timeout = None;
    peer.options.write_timeout = None;
    peer.options.total_connection_timeout = Some(Duration::from_secs(15));
    peer.options.idle_timeout = Some(Duration::from_secs(3600));
    ctx.upstream_host = Some("127.0.0.1".to_string());
    Ok(Box::new(peer))
}

/// 供 Arc 槽类型推断的占位（保持模块导入整洁）。
#[allow(unused)]
fn _anchor(_: Option<Arc<()>>) {}
