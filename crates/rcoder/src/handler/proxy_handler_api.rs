//! Pingora 代理 API 处理函数
//!
//! 提供 Pingora 代理相关的 API 接口，主要用于文档展示和状态查询。

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
                message: "Pingora 代理服务未启用或不可用".to_string(),
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
                message: "Pingora 代理服务实例不可用".to_string(),
                target_port: 0,
                timestamp: Utc::now().to_rfc3339(),
            }),
        )
    })?;
    let conf = svc.config().clone();

    // 收集后端列表
    let backends_arc = svc.backends();
    let backends_map = backends_arc.read().await;
    let backend_count = backends_map.len();
    // 收集后端列表（从缓存快照）
    let health_map = svc.health_snapshot().await;
    let backends = backends_map
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
        "查询代理状态: 端口 {}, 默认后端: {}:{} (后端数: {})",
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
                message: "Pingora 代理服务未启用或不可用".to_string(),
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
                message: "Pingora 代理服务实例不可用".to_string(),
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
        "查询代理统计: 总请求 {}, 成功 {}, 失败 {}, 平均耗时 {:.2}ms",
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
                message: "Pingora 代理服务未启用或不可用".to_string(),
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
                message: "Pingora 代理服务实例不可用".to_string(),
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
                    message: "Pingora 代理配置不可用".to_string(),
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
        "查询代理配置: 监听端口 {}, 默认后端: {}:{}，LB算法: {}",
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
    summary = "重定向到 Pingora 代理（无路径）",
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
                message: "Pingora 代理服务未启用".to_string(),
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
                message: "Pingora 代理配置不可用".to_string(),
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
                    message: format!("构建响应失败: {}", e),
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
    summary = "重定向到 Pingora 代理（含路径）",
    description = r#"
重定向请求到 Pingora 代理服务，包含完整路径信息。

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
                message: "Pingora 代理服务未启用".to_string(),
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
                message: "Pingora 代理配置不可用".to_string(),
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
                    message: format!("构建响应失败: {}", e),
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
                message: "Pingora 代理服务未启用".to_string(),
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
                message: "Pingora 代理配置不可用".to_string(),
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
        "代理请求文档演示: 端口 {}, 路径 {}, 目标: {}",
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
                message: "缺少端口号参数".to_string(),
                target_port: 0,
                timestamp: Utc::now().to_rfc3339(),
            }),
        )
    })?;

    let path = params.path.clone().unwrap_or_else(|| "/".to_string());
    warn!(
        "使用了过时的查询参数代理方式，建议使用路径格式: /proxy/{}/{}",
        port, path
    );

    proxy_request_handler(state, port, Some(path)).await
}
