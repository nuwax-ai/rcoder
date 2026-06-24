//! 健康检查处理器

use axum::Json;
use shared_types::HttpResult;

/// 健康检查端点
///
/// 检查服务的健康状态，同时检查 HTTP 和 gRPC 服务是否就绪
#[utoipa::path(
    get,
    path = "/health",
    responses(
        (status = 200, description = "服务健康状态", body = HttpResult<shared_types::HealthCheckResponse>)
    ),
    tag = "system"
)]
pub async fn health_check() -> Json<HttpResult<shared_types::HealthCheckResponse>> {
    // HTTP 服务：本端点正常响应即表示就绪
    let http_ready = true;

    // RCoder 主服务没有 gRPC 服务，所以 grpc_ready 为 true
    let grpc_ready = true;

    let health_response = shared_types::HealthCheckResponse::new("rcoder-ai-service", http_ready, grpc_ready);

    Json(HttpResult::success(health_response))
}
