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
use chrono::Utc;
use shared_types::HttpResult;
use tracing::info;
use utoipa::ToSchema;

/// 健康检查响应结构
#[derive(serde::Serialize, ToSchema)]
pub struct HealthResponse {
    /// 服务状态：healthy（完全就绪）、starting（启动中）
    pub status: String,
    /// 时间戳
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// 服务名称
    pub service: String,
    /// HTTP 服务是否就绪
    pub http_ready: bool,
    /// gRPC 服务是否就绪（仅在启用 grpc-server feature 时有效）
    pub grpc_ready: bool,
}

/// 健康检查端点
///
/// 检查服务是否完全就绪：
/// - HTTP 服务：本端点正常响应即表示就绪
/// - gRPC 服务（可选）：当启用 grpc-server feature 时，检查本地 50051 端口
///
/// 只有当所有启用的服务都就绪时，才返回 "healthy" 状态。
/// 这样 RCoder 只需检查 HTTP /health 端点即可确保服务完全可用。
#[utoipa::path(
    get,
    path = "/health",
    responses(
        (status = 200, description = "服务健康状态", body = HttpResult<HealthResponse>)
    ),
    tag = "system"
)]
pub async fn health_check() -> Json<HttpResult<HealthResponse>> {
    // HTTP 服务：本端点正常响应即表示就绪
    let http_ready = true;

    // 当启用 grpc-server feature 时，检查 gRPC 端口是否就绪
    #[cfg(feature = "grpc-server")]
    let grpc_ready = check_local_grpc_port().await;

    // 未启用 grpc-server feature 时，不需要检查 gRPC
    #[cfg(not(feature = "grpc-server"))]
    let grpc_ready = true;

    let status = if grpc_ready {
        "healthy".to_string()
    } else {
        "starting".to_string()
    };

    // 输出详细的健康检查日志
    info!(
        "🏥 [HEALTH] Health check: http_ready={}, grpc_ready={}, status={}",
        http_ready, grpc_ready, status
    );

    // 构建健康检查响应
    let health_response = HealthResponse {
        status,
        timestamp: Utc::now(),
        service: "rcoder-ai-service".to_string(),
        http_ready,
        grpc_ready,
    };

    // 根据 gRPC 就绪状态返回不同的结果
    if grpc_ready {
        Json(HttpResult::success(health_response))
    } else {
        // 服务未就绪，返回带 data 的错误响应
        // 这样 RCoder 端可以获取 status、http_ready、grpc_ready 字段
        Json(HttpResult {
            code: "SERVICE_NOT_READY".to_string(),
            message: "Service is starting, gRPC not ready".to_string(),
            data: Some(health_response),
            tid: None,
            success: false,
        })
    }
}

/// 检查本地 gRPC 端口是否可连接
///
/// 使用 TCP 连接检查，快速验证 gRPC 服务是否启动。
/// 仅在启用 grpc-server feature 时使用。
#[cfg(feature = "grpc-server")]
async fn check_local_grpc_port() -> bool {
    use tokio::net::TcpStream;
    use tokio::time::{Duration, timeout};
    use tracing::debug;

    let grpc_port = shared_types::GRPC_DEFAULT_PORT;
    let addr = format!("127.0.0.1:{}", grpc_port);

    match timeout(Duration::from_secs(1), TcpStream::connect(&addr)).await {
        Ok(Ok(_)) => {
            debug!("✅ [HEALTH] Local gRPC port check passed: {}", addr);
            true
        }
        Ok(Err(e)) => {
            debug!("❌ [HEALTH] Local gRPC port check failed {}: {}", addr, e);
            false
        }
        Err(_) => {
            debug!("⏱️ [HEALTH] Local gRPC port check timeout: {}", addr);
            false
        }
    }
}
