//! 健康检查处理器

use axum::Json;
use chrono::Utc;
use utoipa::ToSchema;

/// 健康检查响应结构
#[derive(serde::Serialize, ToSchema)]
pub struct HealthResponse {
    /// 服务状态
    pub status: String,
    /// 时间戳
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// 服务名称
    pub service: String,
}

/// 健康检查端点
///
/// 检查服务的健康状态
#[utoipa::path(
    get,
    path = "/health",
    responses(
        (status = 200, description = "服务健康状态", body = HealthResponse)
    ),
    tag = "system"
)]
pub async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "healthy".to_string(),
        timestamp: Utc::now(),
        service: "rcoder-ai-service".to_string(),
    })
}