//! Computer Agent VNC Status Handler
//!
//! 处理 GET /computer/agent/vnc-status 请求

use axum::{Json, extract::State, http::HeaderMap};
use std::sync::Arc;

use crate::grpc::utils::probe_vnc_readiness;
use crate::http_server::router::AppState;
use shared_types::{AppError, HttpResult, VncStatusResponse};

use super::locale_from_headers;

/// 获取 VNC 连接状态
///
/// 返回与 rcoder `/computer/pod/vnc-status` 相同的 `VncStatusResponse` 结构，
/// 复用共享探测逻辑（noVNC 6080 + Xvnc 5900 RFB），保证两端 vnc_ready/novnc_ready
/// 语义一致。agent_runner 在容器内直接探测，无法提供 `container_id` /
/// `uptime_seconds`（填 None）。
pub async fn handle_computer_vnc_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<HttpResult<VncStatusResponse>>, AppError> {
    let locale = locale_from_headers(&headers);
    let timeout_millis = state
        .config
        .grpc_timeouts
        .as_ref()
        .map(|t| t.port_check_timeout_millis)
        .unwrap_or(500);

    let probed = probe_vnc_readiness(timeout_millis, locale).await;

    let response = VncStatusResponse {
        vnc_ready: probed.vnc_ready,
        novnc_ready: probed.novnc_ready,
        message: probed.message,
        uptime_seconds: None,
        container_id: None,
    };

    Ok(Json(HttpResult::success(response)))
}
