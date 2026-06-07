//! Computer Agent VNC Status Handler
//!
//! 处理 GET /computer/agent/vnc-status 请求

use axum::{Json, extract::State};
use serde::Serialize;
use std::sync::Arc;

use crate::http_server::router::AppState;
use shared_types::{AppError, HttpResult};

/// VNC 状态响应
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct VncStatusResponse {
    pub vnc_available: bool,
    pub port: Option<u16>,
    pub message: String,
}

/// 获取 VNC 连接状态
pub async fn handle_computer_vnc_status(
    State(_state): State<Arc<AppState>>,
) -> Result<Json<HttpResult<VncStatusResponse>>, AppError> {
    let vnc_available = is_vnc_listening();

    let response = VncStatusResponse {
        vnc_available,
        port: if vnc_available { Some(6080) } else { None },
        message: if vnc_available {
            "VNC server is running".to_string()
        } else {
            "VNC server not available".to_string()
        },
    };

    Ok(Json(HttpResult::success(response)))
}

fn is_vnc_listening() -> bool {
    use std::net::TcpStream;
    TcpStream::connect("127.0.0.1:6080").is_ok()
}
