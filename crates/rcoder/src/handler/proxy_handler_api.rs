//! Pingora 代理 API 处理函数
//!
//! 提供 Pingora 代理相关的 API 接口，主要用于文档展示和状态查询。

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};
use serde::Deserialize;
use std::sync::Arc;
use chrono::Utc;
use tracing::{info, debug, warn};

use crate::router::AppState;
use super::proxy_api::*;

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
    if state.config.proxy_config.is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProxyErrorResponse {
                error: "PROXY_DISABLED".to_string(),
                message: "Pingora 代理服务未启用".to_string(),
                target_port: 0,
                timestamp: Utc::now().to_rfc3339(),
            }),
        ));
    }

    let proxy_config = state.config.proxy_config.as_ref().unwrap();

    let status = ProxyStatus {
        status: "running".to_string(),
        listen_port: proxy_config.listen_port,
        default_backend_port: proxy_config.default_backend_port,
        default_backend_host: proxy_config.backend_host.clone(),
        backends: vec![], // 实际实现中可以从 Pingora 服务器获取
        load_balancer: LoadBalancerInfo {
            algorithm: "round-robin".to_string(),
            health_check_enabled: true,
            backend_count: 0,
        },
    };

    info!("查询代理状态: 端口 {}, 默认后端: {}:{}",
          status.listen_port, status.default_backend_host, status.default_backend_port);

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
    if state.config.proxy_config.is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProxyErrorResponse {
                error: "PROXY_DISABLED".to_string(),
                message: "Pingora 代理服务未启用".to_string(),
                target_port: 0,
                timestamp: Utc::now().to_rfc3339(),
            }),
        ));
    }

    // 模拟统计数据，实际实现中可以从 Pingora 服务器获取
    let stats = ProxyStats {
        total_requests: 15420,
        successful_requests: 15200,
        failed_requests: 220,
        avg_response_time_ms: 35.5,
        active_connections: 12,
        port_stats: vec![
            PortStats {
                port: 3000,
                requests: 8560,
                success_rate: 0.987,
                avg_response_time_ms: 28.3,
            },
            PortStats {
                port: 8080,
                requests: 4320,
                success_rate: 0.992,
                avg_response_time_ms: 31.2,
            },
            PortStats {
                port: 9000,
                requests: 2540,
                success_rate: 0.978,
                avg_response_time_ms: 45.8,
            },
        ],
    };

    info!("查询代理统计: 总请求 {}, 成功率 {:.2}%",
          stats.total_requests,
          (stats.successful_requests as f64 / stats.total_requests as f64) * 100.0);

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
    if state.config.proxy_config.is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProxyErrorResponse {
                error: "PROXY_DISABLED".to_string(),
                message: "Pingora 代理服务未启用".to_string(),
                target_port: 0,
                timestamp: Utc::now().to_rfc3339(),
            }),
        ));
    }

    let proxy_config = state.config.proxy_config.as_ref().unwrap();

    let config = ProxyConfig {
        listen_port: proxy_config.listen_port,
        default_backend_port: proxy_config.default_backend_port,
        default_backend_host: proxy_config.backend_host.clone(),
        load_balancing_algorithm: "round-robin".to_string(),
        health_check: HealthCheckConfig {
            enabled: true,
            interval_seconds: 5,
            timeout_seconds: 3,
            healthy_threshold: 2,
            unhealthy_threshold: 3,
        },
    };

    info!("查询代理配置: 监听端口 {}, 默认后端: {}:{}",
          config.listen_port, config.default_backend_host, config.default_backend_port);

    Ok(Json(config))
}

/// 代理到指定端口
#[utoipa::path(
    get,
    path = "/proxy/{port}",
    tag = "proxy",
    summary = "代理到指定端口（无路径）",
    description = "将请求代理到指定端口的服务，无额外路径",
    params(
        ("port" = u16, Path, description = "目标端口号")
    ),
    responses(
        (status = 200, description = "代理成功", body = ProxyResponse),
        (status = 404, description = "后端服务未找到", body = ProxyErrorResponse),
        (status = 502, description = "代理错误", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_to_port(
    State(state): State<Arc<AppState>>,
    Path(port): Path<u16>,
) -> Result<Json<ProxyResponse>, (StatusCode, Json<ProxyErrorResponse>)> {
    proxy_request_handler(state, port, Some("/".to_string())).await
}

/// 代理到指定端口和路径
#[utoipa::path(
    get,
    path = "/proxy/{port}/{*path}",
    tag = "proxy",
    summary = "代理到指定端口和路径",
    description = "将请求代理到指定端口的服务，包含完整路径信息",
    params(
        ("port" = u16, Path, description = "目标端口号"),
        ("path" = String, Path, description = "目标路径")
    ),
    responses(
        (status = 200, description = "代理成功", body = ProxyResponse),
        (status = 404, description = "后端服务未找到", body = ProxyErrorResponse),
        (status = 502, description = "代理错误", body = ProxyErrorResponse)
    )
)]
pub async fn proxy_to_port_with_path(
    State(state): State<Arc<AppState>>,
    Path((port, path)): Path<(u16, String)>,
) -> Result<Json<ProxyResponse>, (StatusCode, Json<ProxyErrorResponse>)> {
    let target_path = if path.is_empty() || path == "/" {
        Some("/".to_string())
    } else {
        Some(format!("/{}", path))
    };

    proxy_request_handler(state, port, target_path).await
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

    let proxy_config = state.config.proxy_config.as_ref().unwrap();
    let target_host = &proxy_config.backend_host;
    let target_path = path.unwrap_or_else(|| "/".to_string());
    let target_url = format!("http://{}:{}{}", target_host, port, target_path);

    debug!("模拟代理请求: {} -> {}", port, target_url);

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

    info!("代理请求文档演示: 端口 {}, 路径 {}, 目标: {}", port, target_path, target_url);

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
        (StatusCode::BAD_REQUEST, Json(ProxyErrorResponse {
            error: "MISSING_PORT".to_string(),
            message: "缺少端口号参数".to_string(),
            target_port: 0,
            timestamp: Utc::now().to_rfc3339(),
        }))
    })?;

    let path = params.path.clone().unwrap_or_else(|| "/".to_string());
    warn!("使用了过时的查询参数代理方式，建议使用路径格式: /proxy/{}/{}",
          port, path);

    proxy_request_handler(state, port, Some(path)).await
}