//! 健康检查处理器
//!
//! 由 binary 端的 router 注册，lib 内不直接调用。
//!
//! 健康检查逻辑：
//! - HTTP 服务：本端点正常响应即表示就绪
//! - gRPC 服务（可选）：当启用 grpc-server feature 时，会检查本地 gRPC 端口
//!
//! 只有当所有启用的服务都就绪时，才返回 "healthy" 状态。

#![allow(dead_code)]

use axum::Json;
use axum::http::StatusCode;
use shared_types::HttpResult;
use tracing::info;

/// 健康检查端点
///
/// 检查服务是否完全就绪：
/// - HTTP 服务：本端点正常响应即表示就绪
/// - gRPC 服务（可选）：当启用 grpc-server feature 时，检查本地 50051 端口
///
/// 只有当所有启用的服务都就绪时，才返回 "healthy" 状态。
/// 这样 RCoder 只需检查 HTTP /health 端点即可确保服务完全可用。
///
/// 重要：当 gRPC 未就绪时，返回 HTTP 503 状态码，这样 K8s readinessProbe
/// 会认为 Pod 不是 Ready 的，避免 rcoder 过早连接 gRPC。
#[utoipa::path(
    get,
    path = "/health",
    responses(
        (status = 200, description = "服务健康状态", body = HttpResult<shared_types::HealthCheckResponse>),
        (status = 503, description = "服务未就绪", body = HttpResult<shared_types::HealthCheckResponse>)
    ),
    tag = "system"
)]
pub async fn health_check() -> (StatusCode, Json<HttpResult<shared_types::HealthCheckResponse>>) {
    // HTTP 服务：本端点正常响应即表示就绪
    let http_ready = true;

    // 当启用 grpc-server feature 时，检查 gRPC 端口是否就绪
    #[cfg(feature = "grpc-server")]
    let grpc_ready = check_local_grpc_port().await;

    // 未启用 grpc-server feature 时，不需要检查 gRPC
    #[cfg(not(feature = "grpc-server"))]
    let grpc_ready = true;

    // 输出详细的健康检查日志
    info!(
        "🏥 [HEALTH] Health check: http_ready={}, grpc_ready={}, status={}",
        http_ready, grpc_ready, if grpc_ready { "healthy" } else { "starting" }
    );

    // 构建健康检查响应
    let health_response = shared_types::HealthCheckResponse::new("agent-runner", http_ready, grpc_ready);

    // 根据 gRPC 就绪状态返回不同的 HTTP 状态码
    if grpc_ready {
        (StatusCode::OK, Json(HttpResult::success(health_response)))
    } else {
        // 服务未就绪，返回 HTTP 503 状态码
        // 这样 K8s readinessProbe 会认为 Pod 不是 Ready 的
        (StatusCode::SERVICE_UNAVAILABLE, Json(HttpResult {
            code: "SERVICE_NOT_READY".to_string(),
            message: "Service is starting, gRPC not ready".to_string(),
            data: Some(health_response),
            tid: None,
            success: false,
        }))
    }
}

/// 检查本地 gRPC 端口是否可连接
///
/// 使用 TCP 连接检查，快速验证 gRPC 服务是否启动。
/// 仅在启用 grpc-server feature 时使用。
#[cfg(feature = "grpc-server")]
pub async fn check_local_grpc_port() -> bool {
    use tokio::net::TcpStream;
    use tokio::time::{Duration, timeout};
    use tracing::debug;

    let grpc_port = shared_types::GRPC_DEFAULT_PORT;
    let addr = format!("127.0.0.1:{}", grpc_port);

    match timeout(Duration::from_secs(1), TcpStream::connect(&addr)).await {
        Ok(Ok(_)) => {
            debug!("[HEALTH] Local gRPC port check passed: {}", addr);
            true
        }
        Ok(Err(e)) => {
            debug!("[HEALTH] Local gRPC port check failed {}: {}", addr, e);
            false
        }
        Err(_) => {
            debug!("[HEALTH] Local gRPC port check timeout: {}", addr);
            false
        }
    }
}

/// 检查本地 gRPC 端口是否可连接（简化版本，用于轻量健康检查）
///
/// 使用 TCP 连接检查，快速验证 gRPC 服务是否启动。
/// 不检查 feature flag，直接检查端口。
pub async fn check_grpc_port_simple() -> bool {
    use tokio::net::TcpStream;
    use tokio::time::{Duration, timeout};

    let grpc_port = shared_types::GRPC_DEFAULT_PORT;
    let addr = format!("127.0.0.1:{}", grpc_port);

    match timeout(Duration::from_secs(1), TcpStream::connect(&addr)).await {
        Ok(Ok(_)) => true,
        Ok(Err(_)) => false,
        Err(_) => false,
    }
}

/// 构建健康检查响应
///
/// 统一的健康检查响应构建函数，供所有健康检查端点使用。
pub fn build_health_response(service_name: &str, http_ready: bool, grpc_ready: bool) -> HttpResult<shared_types::HealthCheckResponse> {
    let health_response = shared_types::HealthCheckResponse::new(service_name, http_ready, grpc_ready);

    if grpc_ready {
        HttpResult::success(health_response)
    } else {
        HttpResult {
            code: "SERVICE_NOT_READY".to_string(),
            message: "Service is starting, gRPC not ready".to_string(),
            data: Some(health_response),
            tid: None,
            success: false,
        }
    }
}
