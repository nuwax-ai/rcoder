//! 端口反代文档接口（/proxy/{port} 族）。

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use chrono::Utc;
use std::sync::Arc;

use crate::handler::proxy_api::*;
use crate::router::AppState;

/// 代理到指定端口（重定向到 Pingora）
#[utoipa::path(
    get,
    path = "/proxy/{port}",
    tag = "proxy",
    summary = "按端口访问部署的应用服务",
    description = r#"
重定向请求到 Pingora 代理服务，无额外路径。

## 工作原理
此接口会返回 307 重定向，将请求转发到 Pingora 代理服务的实际端口。

## 实际代理路径
真正的代理由 Pingora 处理，路径格式为：
```
GET /proxy/{port}/
```

## 使用示例
```bash
# 访问此接口
GET /proxy/3000

# 返回 307 重定向到：
# http://127.0.0.1:{pingora_port}/proxy/3000/

# Pingora 代理到：
# 127.0.0.1:3000/
```
"#,
    params(
        ("port" = u16, Path, description = "目标端口号")
    ),
    responses(
        (status = 307, description = "重定向到 Pingora 代理服务", body = String),
        (status = 503, description = "代理服务未启用", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_to_port(
    State(state): State<Arc<AppState>>,
    Path(port): Path<u16>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    if state.config.proxy_config.is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProxyErrorResponse {
                error: "PROXY_DISABLED".to_string(),
                message: "Pingora proxy service not enabled".to_string(),
                target_port: port,
                timestamp: Utc::now().to_rfc3339(),
            }),
        ));
    }

    let proxy_config = state.config.proxy_config.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProxyErrorResponse {
                error: "PROXY_CONFIG_UNAVAILABLE".to_string(),
                message: "Pingora proxy config unavailable".to_string(),
                target_port: port,
                timestamp: Utc::now().to_rfc3339(),
            }),
        )
    })?;
    let listen_port = proxy_config.listen_port;

    // 重定向到 Pingora 真实代理端口
    let location = format!("http://127.0.0.1:{}/proxy/{}", listen_port, port);

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
                    target_port: port,
                    timestamp: Utc::now().to_rfc3339(),
                }),
            )
        })?;

    Ok(resp)
}

/// 代理到指定端口和路径（重定向到 Pingora）
#[utoipa::path(
    get,
    path = "/proxy/{port}/{*path}",
    tag = "proxy",
    summary = "本机端口代理（含路径）",
    description = r#"
重定向请求到 Pingora 代理服务，包含完整路径信息。

## 通用端口代理（本机调试用）
`/proxy/{port}` 按端口号代理到 **rcoder 容器本机** 的 `127.0.0.1:{port}`（本机服务调试入口）。
**不是** userApp 应用的访问入口——应用访问走免端口专用路由
`/api/v1/userapp/proxy/app/prod/{user_id}/{app_id}/{*path}`（见 `proxy_to_app_with_path`），
容器内控制台（ttyd/dbx）走 `/api/v1/userapp/proxy/{tool}/{dev,prod}/{user_id}/{app_id}`。

## 工作原理
此接口会返回 307 重定向，将请求转发到 Pingora 代理服务的实际端口和路径。

## 实际代理路径
真正的代理由 Pingora 处理，路径格式为：
```
GET /proxy/{port}/{path}
```

## 使用示例
```bash
# 访问此接口
GET /proxy/8080/api/users

# 返回 307 重定向到：
# http://127.0.0.1:{pingora_port}/proxy/8080/api/users

# Pingora 代理到：
# 127.0.0.1:8080/api/users
```
"#,
    params(
        ("port" = u16, Path, description = "目标端口号"),
        ("path" = String, Path, description = "目标路径")
    ),
    responses(
        (status = 307, description = "重定向到 Pingora 代理服务", body = String),
        (status = 503, description = "代理服务未启用", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_to_port_with_path(
    State(state): State<Arc<AppState>>,
    Path((port, path)): Path<(u16, String)>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    if state.config.proxy_config.is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProxyErrorResponse {
                error: "PROXY_DISABLED".to_string(),
                message: "Pingora proxy service not enabled".to_string(),
                target_port: port,
                timestamp: Utc::now().to_rfc3339(),
            }),
        ));
    }

    let proxy_config = state.config.proxy_config.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProxyErrorResponse {
                error: "PROXY_CONFIG_UNAVAILABLE".to_string(),
                message: "Pingora proxy config unavailable".to_string(),
                target_port: port,
                timestamp: Utc::now().to_rfc3339(),
            }),
        )
    })?;
    let listen_port = proxy_config.listen_port;

    let target_path = if path.is_empty() || path == "/" {
        "/".to_string()
    } else {
        format!("/{}", path)
    };

    // 重定向到 Pingora 真实代理端口（保持相同的路径）
    let location = format!(
        "http://127.0.0.1:{}/proxy/{}{}",
        listen_port, port, target_path
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
                    target_port: port,
                    timestamp: Utc::now().to_rfc3339(),
                }),
            )
        })?;

    Ok(resp)
}
