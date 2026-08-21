//! Pingora 代理 API 处理函数
//!
//! 提供 Pingora 代理相关的 API 接口，主要用于文档展示和状态查询。
//!
//! 路由由 binary 端 router 注册；lib 维度看不到调用点，故抑制 dead_code。

#![allow(dead_code)]

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::sync::Arc;
use tracing::{debug, info, warn};

use super::proxy_api::*;
use crate::router::AppState;
use std::sync::atomic::Ordering;

/// Pingora 代理状态查询
#[utoipa::path(
    get,
    path = "/proxy/status",
    tag = "proxy",
    summary = "获取 Pingora 代理服务状态",
    description = "返回当前 Pingora 代理服务的运行状态和配置信息",
    responses(
        (status = 200, description = "成功获取代理状态", body = ProxyStatus),
        (status = 503, description = "代理服务未启用", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ProxyStatus>, (StatusCode, Json<ProxyErrorResponse>)> {
    if state.config.proxy_config.is_none() || state.pingora_service.is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProxyErrorResponse {
                error: "PROXY_DISABLED".to_string(),
                message: "Pingora proxy service is not enabled or unavailable".to_string(),
                target_port: 0,
                timestamp: Utc::now().to_rfc3339(),
            }),
        ));
    }

    let svc = state.pingora_service.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProxyErrorResponse {
                error: "PROXY_SERVICE_UNAVAILABLE".to_string(),
                message: "Pingora proxy service instance is unavailable".to_string(),
                target_port: 0,
                timestamp: Utc::now().to_rfc3339(),
            }),
        )
    })?;
    let conf = svc.config().clone();

    // 收集后端列表
    let backends_arc = svc.backends();
    let backend_snapshot = backends_arc
        .load()
        .iter()
        .map(|(port, host)| (*port, host.clone()))
        .collect::<Vec<_>>();
    let backend_count = backend_snapshot.len();
    // 收集后端列表（从缓存快照）
    let health_map = svc.health_snapshot().await;
    let backends = backend_snapshot
        .iter()
        .map(|(port, host)| {
            if let Some(health) = health_map.get(port) {
                let last_check_str = DateTime::<Utc>::from(health.last_check).to_rfc3339();
                BackendInfo {
                    port: *port,
                    host: host.clone(),
                    health_status: health.status.as_str().to_string(),
                    last_check: last_check_str,
                }
            } else {
                BackendInfo {
                    port: *port,
                    host: host.clone(),
                    health_status: "unknown".to_string(),
                    last_check: Utc::now().to_rfc3339(),
                }
            }
        })
        .collect::<Vec<_>>();

    let status = ProxyStatus {
        status: "running".to_string(),
        listen_port: conf.listen_port,
        default_backend_port: conf.default_backend_port,
        default_backend_host: conf.backend_host.clone(),
        backends,
        load_balancer: LoadBalancerInfo {
            algorithm: if svc.use_round_robin {
                "round-robin".to_string()
            } else {
                "ketama".to_string()
            },
            health_check_enabled: true,
            backend_count,
        },
    };

    info!(
        "Query proxy status: port {}, default backend: {}:{} (backend count: {})",
        status.listen_port, status.default_backend_host, status.default_backend_port, backend_count
    );

    Ok(Json(status))
}

/// Pingora 代理统计信息
#[utoipa::path(
    get,
    path = "/proxy/stats",
    tag = "proxy",
    summary = "获取 Pingora 代理统计信息",
    description = "返回代理服务的请求统计和性能指标",
    responses(
        (status = 200, description = "成功获取统计信息", body = ProxyStats),
        (status = 503, description = "代理服务未启用", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_stats(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ProxyStats>, (StatusCode, Json<ProxyErrorResponse>)> {
    // 需要代理配置启用且服务可用
    if state.config.proxy_config.is_none() || state.pingora_service.is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProxyErrorResponse {
                error: "PROXY_DISABLED".to_string(),
                message: "Pingora proxy service is not enabled or unavailable".to_string(),
                target_port: 0,
                timestamp: Utc::now().to_rfc3339(),
            }),
        ));
    }

    let svc = state.pingora_service.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProxyErrorResponse {
                error: "PROXY_SERVICE_UNAVAILABLE".to_string(),
                message: "Pingora proxy service instance is unavailable".to_string(),
                target_port: 0,
                timestamp: Utc::now().to_rfc3339(),
            }),
        )
    })?;
    let m = &svc.metrics;

    let total_requests = m.total_requests.load(Ordering::Relaxed);
    let successful_requests = m.successful_responses.load(Ordering::Relaxed);
    let failed_requests = m.failed_responses.load(Ordering::Relaxed);
    let avg_response_time_ms = m.avg_response_time_ms();

    // 按端口统计
    let snaps = m.port_snapshots();
    let port_stats = snaps
        .into_iter()
        .map(|ps| {
            let total = ps.successes + ps.failures;
            let success_rate = if total == 0 {
                0.0
            } else {
                (ps.successes as f64) / (total as f64)
            };
            let avg_ms = if total == 0 {
                0.0
            } else {
                (ps.total_response_time_ns as f64) / 1_000_000.0 / (total as f64)
            };
            PortStats {
                port: ps.port,
                requests: ps.requests,
                success_rate,
                avg_response_time_ms: avg_ms,
            }
        })
        .collect::<Vec<_>>();

    let stats = ProxyStats {
        total_requests,
        successful_requests,
        failed_requests,
        avg_response_time_ms,
        active_connections: m.active_connections.load(Ordering::Relaxed) as u32,
        port_stats,
    };

    info!(
        "Query proxy stats: total requests {}, success {}, failed {}, avg response time {:.2}ms",
        stats.total_requests,
        stats.successful_requests,
        stats.failed_requests,
        stats.avg_response_time_ms
    );

    Ok(Json(stats))
}

/// Pingora 代理配置查询
#[utoipa::path(
    get,
    path = "/proxy/config",
    tag = "proxy",
    summary = "获取 Pingora 代理配置",
    description = "返回当前代理服务的配置信息",
    responses(
        (status = 200, description = "成功获取配置信息", body = ProxyConfig),
        (status = 503, description = "代理服务未启用", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_config(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ProxyConfig>, (StatusCode, Json<ProxyErrorResponse>)> {
    if state.config.proxy_config.is_none() || state.pingora_service.is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProxyErrorResponse {
                error: "PROXY_DISABLED".to_string(),
                message: "Pingora proxy service is not enabled or unavailable".to_string(),
                target_port: 0,
                timestamp: Utc::now().to_rfc3339(),
            }),
        ));
    }

    let svc = state.pingora_service.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProxyErrorResponse {
                error: "PROXY_SERVICE_UNAVAILABLE".to_string(),
                message: "Pingora proxy service instance is unavailable".to_string(),
                target_port: 0,
                timestamp: Utc::now().to_rfc3339(),
            }),
        )
    })?;
    let conf = svc.config();
    let app_conf = &state.config;
    let hc_conf = &app_conf
        .proxy_config
        .as_ref()
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ProxyErrorResponse {
                    error: "PROXY_CONFIG_UNAVAILABLE".to_string(),
                    message: "Pingora proxy config unavailable".to_string(),
                    target_port: 0,
                    timestamp: Utc::now().to_rfc3339(),
                }),
            )
        })?
        .health_check;

    let config = ProxyConfig {
        listen_port: conf.listen_port,
        default_backend_port: conf.default_backend_port,
        default_backend_host: conf.backend_host.clone(),
        load_balancing_algorithm: if svc.use_round_robin {
            "round-robin".to_string()
        } else {
            "ketama".to_string()
        },
        health_check: HealthCheckConfig {
            enabled: hc_conf.enabled,
            interval_seconds: hc_conf.interval_seconds as u32,
            timeout_seconds: hc_conf.timeout_seconds as u32,
            healthy_threshold: hc_conf.healthy_threshold,
            unhealthy_threshold: hc_conf.unhealthy_threshold,
        },
    };

    info!(
        "Query proxy config: listen port {}, default backend: {}:{}, LB algorithm: {}",
        config.listen_port,
        config.default_backend_host,
        config.default_backend_port,
        config.load_balancing_algorithm
    );

    Ok(Json(config))
}

/// 代理到指定端口（重定向到 Pingora）
#[utoipa::path(
    get,
    path = "/proxy/{port}",
    tag = "proxy",
    summary = "Pingora 代理 - 访问部署的应用服务（无路径，重定向）",
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
    summary = "Pingora 代理 - 访问部署的应用服务（含路径）",
    description = r#"
重定向请求到 Pingora 代理服务，包含完整路径信息。

## 访问部署的应用服务（重点）
通过 `POST /api/v1/apps` 部署的应用（HTTP 端口），其访问入口 `access.external.http` 返回
`/proxy/{port}`，即本接口。用法：`GET /proxy/{应用的 HTTP 端口}/{路径}` → 经 Pingora 代理到应用后端
（K8s 转发到 `{app_id}-svc:{port}`，Docker 转发到 container_ip:{port}）。

> 例：app HTTP 端口 8080，`GET /apps/{id}` 返回 `access.external.http = "/proxy/8080"`，
> 访问该应用：`GET /proxy/8080/api/users` → Pingora 代理到应用 `:8080/api/users`。
> host（RCoder 入口）由调用方持有，详见应用管理手册 §1.7。

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

/// Pingora 代理 - 访问部署的应用服务（按 app_id+port 路由，含路径）
#[utoipa::path(
    get,
    path = "/proxy/apps/{user_id}/{app_id}/{port}/{*path}",
    tag = "应用管理",
    summary = "Pingora 代理 - 访问部署的应用服务（按 app_id 路由，含路径）",
    description = r#"
访问 `POST /api/v1/apps` 部署的应用（HTTP 端口）。`access.external.http` 返回 `/proxy/apps/{user_id}/{app_id}/{port}`，即本接口。
按 (app_id, port) 路由到应用后端（K8s→`{app_id}-svc:{port}`，Docker→container_ip:{port}），**多 app 同端口不冲突**。

> 例：app HTTP 端口 8080（归属 u6），`GET /apps/{id}` 返回 `access.external.http = "/proxy/apps/u6/{app_id}/8080"`，
> 访问该应用：`GET /proxy/apps/u6/{app_id}/8080/api/users` → Pingora 代理到应用 `:8080/api/users`。
> user_id 不参与后端解析，仅与开发预览 `/proxy/devapps/{user_id}/...` 统一四段形态。
> host（Pingora 入口）由调用方持有，详见应用管理手册 §1.7。

（此 axum 接口返回 307 重定向到 Pingora 端口；生产建议直接访问 Pingora 的 `/proxy/apps/{app_id}/{port}/{path}`）
"#,
    params(
        ("user_id" = String, Path, description = "归属用户 ID（不参与后端解析，与 devapps 统一四段形态）"),
        ("app_id" = String, Path, description = "应用 ID"),
        ("port" = u16, Path, description = "应用的 HTTP 端口"),
        ("path" = String, Path, description = "应用内的路径")
    ),
    responses(
        (status = 307, description = "重定向到 Pingora 代理服务", body = String),
        (status = 503, description = "代理服务未启用", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_to_app_with_path(
    State(state): State<Arc<AppState>>,
    Path((_user_id, app_id, port, path)): Path<(String, String, u16, String)>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    let Some(proxy_config) = state.config.proxy_config.as_ref() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProxyErrorResponse {
                error: "PROXY_DISABLED".to_string(),
                message: "Pingora proxy service not enabled".to_string(),
                target_port: port,
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
    // 重定向到 Pingora 的 app 专用代理路径（四段; user_id 不参与解析, 统一形态）
    let location = format!(
        "http://127.0.0.1:{}/proxy/apps/{}/{}/{}{}",
        listen_port, _user_id, app_id, port, target_path
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

/// Pingora 代理 - 开发阶段预览（app 开发容器 dev server，按 app_id 动态解析）
#[utoipa::path(
    get,
    path = "/proxy/devapps/{user_id}/{app_id}/{port}/{*path}",
    tag = "应用管理",
    summary = "Pingora 代理 - 开发阶段预览（app 开发容器 dev server，零注册动态解析）",
    description = r#"
访问开发阶段该 app 开发容器（UserAppBuilder，per-app）里的 dev server（`POST /api/userapp/dev/start`
启动，PortPool 分配端口），或容器内自装 pingap/app-cli 的统一入口（端口 9080）。与部署访问
`/proxy/apps/{user_id}/{app_id}/{port}/{*path}` 同构四段——**开发切部署前端只改 `devapps`→`apps` 一段**。

- upstream 动态解析到该 app 的开发容器（UserAppBuilder）同端口，**零注册零状态**：
  Java 用 `dev/start` 响应的 `port` + `user_id` + `app_id` 三元组直接拼 URL。
- `user_id` 不参与解析（开发容器 per-app 定位），用于日志排障与未来归属鉴权。
- 多 app 并行：每 app 独立开发容器独立端口池，app-a（4000）/app-b（4000）各拼各的 URL。
- 长连接支持（HMR/WebSocket）；无该 app 开发容器 → 502；容器重建后有短窗口旧 IP（下次 ensure 修正）。

> 例：`GET /proxy/devapps/u6/app-order-svc/4000/api/users` → 开发容器 `:4000/api/users`。
> host（Pingora 入口）由调用方持有，详见应用管理手册 §12。
"#,
    params(
        ("user_id" = String, Path, description = "用户 ID（不参与解析；日志排障/鉴权锚点）"),
        ("app_id" = String, Path, description = "应用 ID（定位其 per-app 开发容器）"),
        ("port" = u16, Path, description = "开发容器内端口（dev server 的 PortPool 端口或 pingap 的 9080）"),
        ("path" = String, Path, description = "应用内的路径")
    ),
    responses(
        (status = 307, description = "重定向到 Pingora 代理服务", body = String),
        (status = 503, description = "代理服务未启用", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_to_devapp_with_path(
    State(state): State<Arc<AppState>>,
    Path((user_id, app_id, port, path)): Path<(String, String, u16, String)>,
) -> Result<axum::response::Response, (StatusCode, Json<ProxyErrorResponse>)> {
    let Some(proxy_config) = state.config.proxy_config.as_ref() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProxyErrorResponse {
                error: "PROXY_DISABLED".to_string(),
                message: "Pingora proxy service not enabled".to_string(),
                target_port: port,
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
        "http://127.0.0.1:{}/proxy/devapps/{}/{}/{}{}",
        listen_port, user_id, app_id, port, target_path
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

/// 通用代理请求处理器
async fn proxy_request_handler(
    state: Arc<AppState>,
    port: u16,
    path: Option<String>,
) -> Result<Json<ProxyResponse>, (StatusCode, Json<ProxyErrorResponse>)> {
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
    let target_host = &proxy_config.backend_host;
    let target_path = path.unwrap_or_else(|| "/".to_string());
    let target_url = format!("http://{}:{}{}", target_host, port, target_path);

    debug!("Mock proxy request: {} -> {}", port, target_url);

    // 这里只是用于文档展示，实际的代理由 Pingora 服务器处理
    // 如果用户访问这些接口，我们会返回信息，说明实际的代理在 Pingora 服务器端口

    let response = ProxyResponse {
        success: true,
        target_port: port,
        target_host: target_host.clone(),
        target_url: target_url.clone(),
        response_time_ms: Some(35),
        load_balancer: LoadBalancerInfo {
            algorithm: "round-robin".to_string(),
            health_check_enabled: true,
            backend_count: 1,
        },
    };

    info!(
        "Proxy request documentation demo: port {}, path {}, target: {}",
        port, target_path, target_url
    );

    Ok(Json(response))
}

/// 查询参数
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct ProxyQueryParams {
    /// 端口号（用于向后兼容）
    #[param(example = 3000)]
    pub port: Option<u16>,
    /// 路径（可选）
    #[param(example = "/api/users")]
    pub path: Option<String>,
}

/// 使用查询参数的代理方式（向后兼容）
#[utoipa::path(
    get,
    path = "/proxy",
    tag = "proxy",
    summary = "使用查询参数代理（向后兼容）",
    description = "通过查询参数指定目标端口和路径，保持向后兼容性",
    params(
        ProxyQueryParams
    ),
    responses(
        (status = 200, description = "代理成功", body = ProxyResponse),
        (status = 400, description = "缺少端口参数", body = ProxyErrorResponse),
        (status = 503, description = "代理服务未启用", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_with_query_params(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ProxyQueryParams>,
) -> Result<Json<ProxyResponse>, (StatusCode, Json<ProxyErrorResponse>)> {
    let port = params.port.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(ProxyErrorResponse {
                error: "MISSING_PORT".to_string(),
                message: "Missing port parameter".to_string(),
                target_port: 0,
                timestamp: Utc::now().to_rfc3339(),
            }),
        )
    })?;

    let path = params.path.clone().unwrap_or_else(|| "/".to_string());
    warn!(
        "Using deprecated query parameter proxy method, recommended path format: /proxy/{}/{}",
        port, path
    );

    proxy_request_handler(state, port, Some(path)).await
}
