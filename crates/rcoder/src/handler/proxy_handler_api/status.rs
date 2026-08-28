//! 代理状态/统计/配置查询文档接口。

use axum::{extract::State, response::Json};
use chrono::{DateTime, Utc};
use shared_types::error_codes;
use shared_types::{AppError, HttpResult};
use std::sync::Arc;
use tracing::info;

use crate::handler::proxy_api::*;
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
        (status = 200, description = "成功获取代理状态", body = HttpResult<ProxyStatus>),
        (status = 503, description = "代理服务未启用", body = HttpResult<String>)
    )
)]
pub async fn proxy_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<HttpResult<ProxyStatus>>, AppError> {
    if state.config.proxy_config.is_none() || state.pingora_service.is_none() {
        return Err(AppError::with_message(
            error_codes::ERR_PROXY_DISABLED,
            "Pingora proxy service is not enabled or unavailable",
        ));
    }

    let svc = state.pingora_service.as_ref().ok_or_else(|| {
        AppError::with_message(
            error_codes::ERR_PROXY_SERVICE_UNAVAILABLE,
            "Pingora proxy service instance is unavailable",
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

    Ok(Json(HttpResult::success(status)))
}

/// Pingora 代理统计信息
#[utoipa::path(
    get,
    path = "/proxy/stats",
    tag = "proxy",
    summary = "获取 Pingora 代理统计信息",
    description = "返回代理服务的请求统计和性能指标",
    responses(
        (status = 200, description = "成功获取统计信息", body = HttpResult<ProxyStats>),
        (status = 503, description = "代理服务未启用", body = HttpResult<String>)
    )
)]
pub async fn proxy_stats(
    State(state): State<Arc<AppState>>,
) -> Result<Json<HttpResult<ProxyStats>>, AppError> {
    // 需要代理配置启用且服务可用
    if state.config.proxy_config.is_none() || state.pingora_service.is_none() {
        return Err(AppError::with_message(
            error_codes::ERR_PROXY_DISABLED,
            "Pingora proxy service is not enabled or unavailable",
        ));
    }

    let svc = state.pingora_service.as_ref().ok_or_else(|| {
        AppError::with_message(
            error_codes::ERR_PROXY_SERVICE_UNAVAILABLE,
            "Pingora proxy service instance is unavailable",
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

    Ok(Json(HttpResult::success(stats)))
}

/// Pingora 代理配置查询
#[utoipa::path(
    get,
    path = "/proxy/config",
    tag = "proxy",
    summary = "获取 Pingora 代理配置",
    description = "返回当前代理服务的配置信息",
    responses(
        (status = 200, description = "成功获取配置信息", body = HttpResult<ProxyConfig>),
        (status = 503, description = "代理服务未启用", body = HttpResult<String>)
    )
)]
pub async fn proxy_config(
    State(state): State<Arc<AppState>>,
) -> Result<Json<HttpResult<ProxyConfig>>, AppError> {
    if state.config.proxy_config.is_none() || state.pingora_service.is_none() {
        return Err(AppError::with_message(
            error_codes::ERR_PROXY_DISABLED,
            "Pingora proxy service is not enabled or unavailable",
        ));
    }

    let svc = state.pingora_service.as_ref().ok_or_else(|| {
        AppError::with_message(
            error_codes::ERR_PROXY_SERVICE_UNAVAILABLE,
            "Pingora proxy service instance is unavailable",
        )
    })?;
    let conf = svc.config();
    let app_conf = &state.config;
    let hc_conf = &app_conf
        .proxy_config
        .as_ref()
        .ok_or_else(|| {
            AppError::with_message(
                error_codes::ERR_PROXY_SERVICE_UNAVAILABLE,
                "Pingora proxy config unavailable",
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

    Ok(Json(HttpResult::success(config)))
}
