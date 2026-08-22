//! computer 桌面 vnc 代理（从 computer_desktop_handler.rs 拆出）。

use shared_types::NOVNC_PORT;

use axum::extract::State;
use axum::http::HeaderMap;
use serde::Deserialize;
use std::sync::Arc;
use tracing::{error, info, instrument, warn};
use utoipa::ToSchema;

use super::super::utils::{I18nPath, get_locale_from_headers};
use super::proxy::{DesktopAccessResponse, DesktopErrorResponse, DesktopPathParams};
use crate::router::AppState;
use crate::service::ComputerContainerManager;
use crate::{AppError, HttpResult};

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
            status = 400,
            description = "参数错误（user_id/project_id 为空）",
            body = HttpResult<String>
        ),
        (
            status = 401,
            description = "API Key 鉴权失败",
            body = HttpResult<String>
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
#[instrument(skip(state), fields(user_id = %params.user_id, project_id = %params.project_id))]
pub async fn computer_desktop_vnc(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    I18nPath(params): I18nPath<DesktopPathParams>,
) -> Result<HttpResult<DesktopAccessResponse>, AppError> {
    let locale = get_locale_from_headers(&headers);
    let user_id = params.user_id.clone();
    let project_id = params.project_id.clone();

    // 1. 验证参数（校验失败返回 Err(AppError)，由 status_from_code 映射为 400/404，
    //    与 utoipa 声明对齐；不再用 Ok(HttpResult::error) 返回 HTTP 200）
    // 字符集校验：user_id/project_id 会被拼进 Pingora 代理 URL（见下方 format!），
    // 复用 shared_types::validate_identifier（字母/数字/下划线/连字符，含长度≤64），
    // 拒绝 `/`、`..`、空格等可能拼出非预期路径的字符（纵深防御）。
    if user_id.trim().is_empty() {
        error!("[DESKTOP_VNC] user_id is required");
        return Err(AppError::with_message(
            shared_types::error_codes::ERR_VALIDATION,
            shared_types::get_i18n_message("error.user_id_required", locale),
        ));
    } else {
        shared_types::validate_identifier(&user_id, "user_id").map_err(|e| {
            error!("[DESKTOP_VNC] invalid user_id: {e}");
            AppError::with_message(shared_types::error_codes::ERR_VALIDATION, e)
        })?;
    }

    if project_id.trim().is_empty() {
        error!("[DESKTOP_VNC] project_id is required");
        return Err(AppError::with_message(
            shared_types::error_codes::ERR_VALIDATION,
            shared_types::get_i18n_message("error.project_id_required", locale),
        ));
    } else {
        shared_types::validate_identifier(&project_id, "project_id").map_err(|e| {
            error!("[DESKTOP_VNC] invalid project_id: {e}");
            AppError::with_message(shared_types::error_codes::ERR_VALIDATION, e)
        })?;
    }

    info!(
        "🖥️ [DESKTOP_VNC] Getting VNC access info: user_id={}, project_id={}",
        user_id, project_id
    );

    // 2. 查找用户容器
    let container_info =
        ComputerContainerManager::get_container_info(&user_id, state.runtime()).await?;

    let container_info = match container_info {
        Some(info) => info,
        None => {
            warn!("[DESKTOP_VNC] Container not found: user_id={}", user_id);
            return Err(AppError::with_message(
                shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
                shared_types::get_i18n_message("error.container_not_found", locale),
            ));
        }
    };

    info!(
        "📦 [DESKTOP_VNC] Found container: container_id={}, ip={}",
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
        message: "请使用 proxy_vnc_url 或 proxy_websocket_url 通过 Pingora 代理访问 VNC 桌面"
            .to_string(),
    };

    info!(
        "✅ [DESKTOP_VNC] VNC access info generated: user_id={}, proxy_vnc_url={}",
        user_id, proxy_vnc_url
    );

    Ok(HttpResult::success(response))
}

/// VNC 桌面代理路径参数（用于 Pingora 代理）
#[derive(Debug, Deserialize, ToSchema)]
#[allow(dead_code)] // 字段由 axum 框架自动提取使用
pub struct VncProxyPathParams {
    /// 用户 ID
    #[schema(example = "user_123")]
    pub user_id: String,
    /// 项目 ID
    #[schema(example = "proj_456")]
    pub project_id: String,
    /// 剩余路径（可选）
    #[schema(example = "vnc.html", nullable = true)]
    pub path: Option<String>,
}
