//! 健康检查处理器

use axum::Json;
use serde_json::Value;
use chrono::Utc;

/// 健康检查端点
pub async fn health_check() -> Json<Value> {
    Json(serde_json::json!({
        "status": "healthy",
        "timestamp": Utc::now(),
        "service": "rcoder-ai-service"
    }))
}