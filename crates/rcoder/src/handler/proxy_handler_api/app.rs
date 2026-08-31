//! 应用流量代理文档接口（/proxy/userapp 族）。

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use chrono::Utc;
use std::sync::Arc;

use crate::handler::proxy_api::*;
use crate::router::AppState;

/// Pingora 代理 - 访问部署的应用服务（免端口，按 app_id 路由，含路径）
#[utoipa::path(
    get,
    path = "/proxy/userapp/prod/{user_id}/{app_id}/{*path}",
    tag = "Userapp · 访问入口",
    summary = "按 app_id 访问部署的应用服务",
    description = r#"
访问 `POST /api/v1/userapp` 部署的应用。`access.external.http` 返回 `/proxy/userapp/prod/{user_id}/{app_id}`，即本接口。
**免端口**：代理内部固定拨 pingap 统一入口 `APP_ENTRY_PORT`(9080)——按 (app_id, 9080) 查
`app_backends` 注册表路由到应用后端（K8s→`{app_id}-svc`，Docker→container_ip），**多 app 同端口不冲突**；
未注册 9080 且该 app 恰只有一个已注册 HTTP 端口时回退用之。

> 例（归属 u6）：`GET /apps/{id}` 返回 `access.external.http = "/proxy/userapp/prod/u6/{app_id}"`，
> 访问该应用：`GET /proxy/userapp/prod/u6/{app_id}/api/users` → Pingora 代理到应用 `:9080/api/users`。
> user_id 不参与后端解析，仅与开发预览 `/proxy/userapp/dev/{user_id}/...` 同构——切环境只改 `dev→prod` 一段。
> host（Pingora 入口）由调用方持有，详见应用管理手册 §1.7。

（此 axum 接口返回 307 重定向到 Pingora 端口；生产建议直接访问 Pingora 的同路径）
"#,
    params(
        ("user_id" = String, Path, description = "归属用户 ID（不参与后端解析，与 dev 流量同构）"),
        ("app_id" = String, Path, description = "应用 ID"),
        ("path" = String, Path, description = "应用内的路径")
    ),
    responses(
        (status = 307, description = "重定向到 Pingora 代理服务", body = String),
        (status = 503, description = "代理服务未启用", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_to_app_with_path(
    State(state): State<Arc<AppState>>,
    Path((_user_id, app_id, path)): Path<(String, String, String)>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    let Some(proxy_config) = state.config.proxy_config.as_ref() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProxyErrorResponse {
                error: "PROXY_DISABLED".to_string(),
                message: "Pingora proxy service not enabled".to_string(),
                target_port: shared_types::APP_ENTRY_PORT,
                timestamp: Utc::now().to_rfc3339(),
            }),
        ));
    };
    let listen_port = proxy_config.listen_port;
    let target_path = if path.is_empty() || path == "/" {
        "/".to_string()
    } else {
        format!("/{}", path)
    };
    // 重定向到 Pingora 的 app 流量路径（免端口; user_id 不参与解析, 同构锚点）
    let location = format!(
        "http://127.0.0.1:{}/proxy/userapp/prod/{}/{}/{}",
        listen_port, _user_id, app_id, target_path
    );
    let resp = axum::http::Response::builder()
        .status(StatusCode::TEMPORARY_REDIRECT)
        .header(axum::http::header::LOCATION, location)
        .body(axum::body::Body::empty())
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ProxyErrorResponse {
                    error: "RESPONSE_BUILD_ERROR".to_string(),
                    message: format!("failed to build response: {}", e),
                    target_port: shared_types::APP_ENTRY_PORT,
                    timestamp: Utc::now().to_rfc3339(),
                }),
            )
        })?;
    Ok(resp)
}

/// Pingora 代理 - 开发阶段预览（app 开发容器 dev server，按 app_id 动态解析）
#[utoipa::path(
    get,
    path = "/proxy/userapp/dev/{user_id}/{app_id}/{*path}",
    tag = "Userapp · 访问入口",
    summary = "开发容器预览入口",
    description = r#"
访问开发阶段该 app 开发容器（UserappBuilder，per-app）内的应用。与部署访问
`/proxy/userapp/prod/{user_id}/{app_id}/{*path}` 同构——**开发切部署前端只改 `dev→prod` 一段**。

- **免端口**：代理内部固定拨 pingap 统一入口 `APP_ENTRY_PORT`(9080)——开发容器
  manifest 流程（`POST /api/v1/userapp/dev/start`）恒起 app-cli+pingap，9080 即整应用入口。
- upstream 动态解析到该 app 的开发容器（UserappBuilder），**零注册零状态**：
  Java 用 `user_id` + `app_id` 直接拼 URL。
- `user_id` 不参与解析（开发容器 per-app 定位），用于日志排障与未来归属鉴权。
- 多 app 并行：每 app 独立开发容器，URL 各拼各的 app_id。
- 长连接支持（HMR/WebSocket）；无该 app 开发容器 → 502；容器重建后有短窗口旧 IP（下次 ensure 修正）。

> 例：`GET /proxy/userapp/dev/u6/app-order-svc/api/users` → 开发容器 `:9080/api/users`。
> host（Pingora 入口）由调用方持有，详见应用管理手册 §12。
"#,
    params(
        ("user_id" = String, Path, description = "用户 ID（不参与解析；日志排障/鉴权锚点）"),
        ("app_id" = String, Path, description = "应用 ID（定位其 per-app 开发容器）"),
        ("path" = String, Path, description = "应用内的路径")
    ),
    responses(
        (status = 307, description = "重定向到 Pingora 代理服务", body = String),
        (status = 503, description = "代理服务未启用", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_to_devapp_with_path(
    State(state): State<Arc<AppState>>,
    Path((user_id, app_id, path)): Path<(String, String, String)>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    let Some(proxy_config) = state.config.proxy_config.as_ref() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProxyErrorResponse {
                error: "PROXY_DISABLED".to_string(),
                message: "Pingora proxy service not enabled".to_string(),
                target_port: shared_types::APP_ENTRY_PORT,
                timestamp: Utc::now().to_rfc3339(),
            }),
        ));
    };
    let listen_port = proxy_config.listen_port;
    let target_path = if path.is_empty() || path == "/" {
        "/".to_string()
    } else {
        format!("/{}", path)
    };
    let location = format!(
        "http://127.0.0.1:{}/proxy/userapp/dev/{}/{}/{}",
        listen_port, user_id, app_id, target_path
    );
    let resp = axum::http::Response::builder()
        .status(StatusCode::TEMPORARY_REDIRECT)
        .header(axum::http::header::LOCATION, location)
        .body(axum::body::Body::empty())
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ProxyErrorResponse {
                    error: "RESPONSE_BUILD_ERROR".to_string(),
                    message: format!("failed to build response: {}", e),
                    target_port: shared_types::APP_ENTRY_PORT,
                    timestamp: Utc::now().to_rfc3339(),
                }),
            )
        })?;
    Ok(resp)
}
