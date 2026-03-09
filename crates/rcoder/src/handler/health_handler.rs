//! 健康检查处理器

use axum::Json;
use utoipa::ToSchema;

/// 健康检查响应结构
///
/// 统一的服务健康检查响应格式
#[derive(serde::Serialize, ToSchema)]
pub struct HealthResponse {
    /// 服务状态
    #[schema(example = "healthy")]
    pub status: String,
    /// 时间戳
    #[schema(example = "2024-01-15T10:30:00Z")]
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// 服务名称
    #[schema(example = "rcoder-ai-service")]
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
        timestamp: chrono::Utc::now(),
        service: "rcoder-ai-service".to_string(),
    })
}
