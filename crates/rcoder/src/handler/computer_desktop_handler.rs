//! Computer Agent Runner VNC 桌面处理器
//!
//! 提供 VNC 桌面访问功能，允许用户通过 WebSocket 连接到容器内的 noVNC 服务。
//!
//! ## 端口说明
//! - noVNC WebSocket: 6080 (容器内)
//!
//! ## 实现状态
//!
//! **当前版本已实现 Pingora WebSocket 透明代理。**
//!
//! 客户端应使用 Pingora 代理路径访问 VNC：
//! - VNC 页面: `http://{proxy_host}/computer/vnc/{user_id}/{project_id}/vnc.html`
//! - WebSocket: `ws://{proxy_host}/computer/vnc/{user_id}/{project_id}/websockify`
//!
//! Pingora 会自动将请求透明代理到对应用户容器的 noVNC 服务（端口 6080）。
//!
//! ## 安全说明
//! - 生产环境中，客户端只通过代理地址访问，不直接暴露容器内部 IP
//! - user_id 到 container_ip 的映射在 Pingora 内部管理

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, info, instrument, warn};
use utoipa::ToSchema;

use crate::{router::AppState, service::ComputerContainerManager, AppError, HttpResult};

/// VNC 桌面路径参数
#[derive(Debug, Deserialize, ToSchema)]
pub struct DesktopPathParams {
    /// 用户 ID
    #[schema(example = "user_123")]
    pub user_id: String,
    /// 项目 ID
    #[schema(example = "proj_456")]
    pub project_id: String,
}

/// VNC 桌面访问响应
#[derive(Debug, Serialize, ToSchema)]
pub struct DesktopAccessResponse {
    /// 操作是否成功
    pub success: bool,

    /// Pingora 代理的 VNC 访问 URL（推荐使用）
    #[schema(example = "/computer/vnc/user_123/proj_456/vnc.html")]
    pub proxy_vnc_url: String,

    /// Pingora 代理的 WebSocket 连接 URL（推荐使用）
    #[schema(example = "/computer/vnc/user_123/proj_456/websockify")]
    pub proxy_websocket_url: String,

    /// 直接访问的 noVNC URL（仅开发/测试使用）
    #[schema(example = "http://172.17.0.5:6080/vnc.html")]
    pub direct_vnc_url: String,

    /// 直接访问的 WebSocket URL（仅开发/测试使用）
    #[schema(example = "ws://172.17.0.5:6080/websockify")]
    pub direct_websocket_url: String,

    /// 容器 ID
    pub container_id: String,

    /// 容器 IP 地址（内部 IP，不应直接暴露给外部客户端）
    pub container_ip: String,

    /// 用户 ID
    pub user_id: String,

    /// 项目 ID
    pub project_id: String,

    /// 访问提示
    #[schema(example = "请使用 proxy_vnc_url 或 proxy_websocket_url 访问 VNC 桌面")]
    pub message: String,
}

/// 错误响应
#[derive(Debug, Serialize, ToSchema)]
pub struct DesktopErrorResponse {
    /// 错误代码
    pub error: String,
    /// 错误消息
    pub message: String,
    /// 用户 ID
    pub user_id: String,
    /// 项目 ID
    pub project_id: String,
}

/// noVNC 默认端口
const NOVNC_PORT: u16 = 6080;

/// 获取 VNC 桌面访问信息
///
/// 返回 VNC 桌面的访问 URL。
/// 推荐使用 Pingora 代理路径（proxy_vnc_url / proxy_websocket_url）访问，
/// 这样可以避免暴露容器内部 IP。
///
/// ## 访问方式
/// - **推荐**: 使用 `proxy_vnc_url` 通过 Pingora 代理访问
/// - **开发测试**: 可以使用 `direct_vnc_url` 直接访问（需要网络互通）
#[utoipa::path(
    get,
    path = "/computer/desktop/{user_id}/{project_id}",
    params(
        ("user_id" = String, Path, description = "用户 ID"),
        ("project_id" = String, Path, description = "项目 ID")
    ),
    responses(
        (
            status = 200,
            description = "成功获取 VNC 访问信息",
            body = HttpResult<DesktopAccessResponse>,
            example = json!({
                "success": true,
                "data": {
                    "success": true,
                    "proxy_vnc_url": "/computer/vnc/user_123/proj_456/vnc.html",
                    "proxy_websocket_url": "/computer/vnc/user_123/proj_456/websockify",
                    "direct_vnc_url": "http://172.17.0.5:6080/vnc.html",
                    "direct_websocket_url": "ws://172.17.0.5:6080/websockify",
                    "container_id": "abc123def456",
                    "container_ip": "172.17.0.5",
                    "user_id": "user_123",
                    "project_id": "proj_456",
                    "message": "请使用 proxy_vnc_url 访问 VNC 桌面"
                },
                "error": null
            })
        ),
        (
            status = 404,
            description = "找不到用户容器",
            body = HttpResult<DesktopErrorResponse>
        ),
        (
            status = 500,
            description = "服务器内部错误",
            body = HttpResult<String>
        )
    ),
    tag = "computer",
    operation_id = "computer_desktop_vnc",
    summary = "获取 VNC 桌面访问信息",
    description = "返回 VNC 桌面的访问 URL，推荐使用 Pingora 代理路径访问"
)]
#[axum::debug_handler]
#[instrument(skip(_state), fields(user_id = %params.user_id, project_id = %params.project_id))]
pub async fn computer_desktop_vnc(
    State(_state): State<Arc<AppState>>,
    Path(params): Path<DesktopPathParams>,
) -> Result<HttpResult<DesktopAccessResponse>, AppError> {
    let user_id = params.user_id.clone();
    let project_id = params.project_id.clone();

    // 1. 验证参数
    if user_id.trim().is_empty() {
        error!("❌ [DESKTOP_VNC] user_id 不能为空");
        return Err(AppError::validation_error("user_id 不能为空"));
    }

    if project_id.trim().is_empty() {
        error!("❌ [DESKTOP_VNC] project_id 不能为空");
        return Err(AppError::validation_error("project_id 不能为空"));
    }

    info!(
        "🖥️ [DESKTOP_VNC] 获取 VNC 访问信息: user_id={}, project_id={}",
        user_id, project_id
    );

    // 2. 查找用户容器
    let container_info = ComputerContainerManager::get_container_info(&user_id).await?;

    let container_info = match container_info {
        Some(info) => info,
        None => {
            warn!(
                "⚠️ [DESKTOP_VNC] 找不到用户容器: user_id={}",
                user_id
            );
            return Ok(HttpResult::error(
                "NOT_FOUND",
                &format!("找不到用户 {} 的容器，请先发送聊天请求创建容器", user_id),
            ));
        }
    };

    info!(
        "📦 [DESKTOP_VNC] 找到容器: container_id={}, ip={}",
        container_info.container_id, container_info.container_ip
    );

    // 3. 构建 VNC 访问 URL
    let container_ip = &container_info.container_ip;

    // Pingora 代理路径（推荐使用）
    let proxy_vnc_url = format!("/computer/vnc/{}/{}/vnc.html", user_id, project_id);
    let proxy_websocket_url = format!("/computer/vnc/{}/{}/websockify", user_id, project_id);

    // 直接访问路径（仅开发测试使用）
    let direct_vnc_url = format!("http://{}:{}/vnc.html", container_ip, NOVNC_PORT);
    let direct_websocket_url = format!("ws://{}:{}/websockify", container_ip, NOVNC_PORT);

    // 4. 返回访问信息
    let response = DesktopAccessResponse {
        success: true,
        proxy_vnc_url: proxy_vnc_url.clone(),
        proxy_websocket_url: proxy_websocket_url.clone(),
        direct_vnc_url,
        direct_websocket_url,
        container_id: container_info.container_id.clone(),
        container_ip: container_ip.clone(),
        user_id: user_id.clone(),
        project_id: project_id.clone(),
        message: "请使用 proxy_vnc_url 或 proxy_websocket_url 通过 Pingora 代理访问 VNC 桌面".to_string(),
    };

    info!(
        "✅ [DESKTOP_VNC] VNC 访问信息已生成: user_id={}, proxy_vnc_url={}",
        user_id, proxy_vnc_url
    );

    Ok(HttpResult::success(response))
}

/// VNC 桌面代理（WebSocket）
///
/// 这是一个占位实现。
/// 完整的 WebSocket 代理需要：
/// 1. HTTP Upgrade 处理
/// 2. 双向 WebSocket 帧转发
/// 3. 连接生命周期管理
///
/// 建议方案：
/// - 使用 Pingora 的 WebSocket 支持（如果有）
/// - 或在 rcoder 前面部署 Nginx 作为 VNC 代理
#[allow(dead_code)]
pub async fn computer_desktop_proxy(
    State(_state): State<Arc<AppState>>,
    Path(params): Path<DesktopPathParams>,
) -> impl IntoResponse {
    // TODO: 实现 WebSocket 代理
    // 1. HTTP Upgrade 检测
    // 2. 建立到容器 6080 端口的 WebSocket 连接
    // 3. 双向帧转发

    let error_response = DesktopErrorResponse {
        error: "NOT_IMPLEMENTED".to_string(),
        message: "WebSocket 代理功能尚未实现，请使用 GET /computer/desktop/{user_id}/{project_id} 获取直接访问 URL".to_string(),
        user_id: params.user_id,
        project_id: params.project_id,
    };

    (
        StatusCode::NOT_IMPLEMENTED,
        Json(error_response),
    )
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 检查 VNC 服务是否可用
///
/// 尝试连接到容器的 noVNC 端口，验证服务是否运行
#[allow(dead_code)]
async fn check_vnc_available(container_ip: &str) -> bool {
    use tokio::net::TcpStream;
    use tokio::time::{Duration, timeout};

    let addr = format!("{}:{}", container_ip, NOVNC_PORT);

    match timeout(Duration::from_secs(2), TcpStream::connect(&addr)).await {
        Ok(Ok(_)) => {
            info!("✅ [DESKTOP_VNC] VNC 服务可用: {}", addr);
            true
        }
        Ok(Err(e)) => {
            warn!("⚠️ [DESKTOP_VNC] VNC 连接失败: {} - {}", addr, e);
            false
        }
        Err(_) => {
            warn!("⚠️ [DESKTOP_VNC] VNC 连接超时: {}", addr);
            false
        }
    }
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vnc_url_format() {
        let ip = "172.17.0.5";
        let user_id = "user_123";
        let project_id = "proj_456";

        // Pingora 代理路径
        let proxy_vnc_url = format!("/computer/vnc/{}/{}/vnc.html", user_id, project_id);
        let proxy_ws_url = format!("/computer/vnc/{}/{}/websockify", user_id, project_id);

        // 直接访问路径
        let direct_vnc_url = format!("http://{}:{}/vnc.html", ip, NOVNC_PORT);
        let direct_ws_url = format!("ws://{}:{}/websockify", ip, NOVNC_PORT);

        assert_eq!(proxy_vnc_url, "/computer/vnc/user_123/proj_456/vnc.html");
        assert_eq!(proxy_ws_url, "/computer/vnc/user_123/proj_456/websockify");
        assert_eq!(direct_vnc_url, "http://172.17.0.5:6080/vnc.html");
        assert_eq!(direct_ws_url, "ws://172.17.0.5:6080/websockify");
    }

    #[test]
    fn test_desktop_access_response_serialization() {
        let response = DesktopAccessResponse {
            success: true,
            proxy_vnc_url: "/computer/vnc/user_123/proj_456/vnc.html".to_string(),
            proxy_websocket_url: "/computer/vnc/user_123/proj_456/websockify".to_string(),
            direct_vnc_url: "http://172.17.0.5:6080/vnc.html".to_string(),
            direct_websocket_url: "ws://172.17.0.5:6080/websockify".to_string(),
            container_id: "abc123".to_string(),
            container_ip: "172.17.0.5".to_string(),
            user_id: "user_123".to_string(),
            project_id: "proj_456".to_string(),
            message: "Test message".to_string(),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("proxy_vnc_url"));
        assert!(json.contains("proxy_websocket_url"));
        assert!(json.contains("direct_vnc_url"));
        assert!(json.contains("user_123"));
    }
}
