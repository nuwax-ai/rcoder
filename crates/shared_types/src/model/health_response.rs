//! 健康检查响应类型
//!
//! 提供统一的健康检查响应结构，供所有服务复用

use chrono::{DateTime, Utc};
use utoipa::ToSchema;

/// 健康检查响应结构
///
/// 用于所有服务的健康检查端点返回统一格式
#[derive(serde::Serialize, ToSchema)]
pub struct HealthResponse {
    /// 服务状态
    #[schema(example = "healthy")]
    pub status: String,

    /// 时间戳
    #[schema(example = "2024-01-15T10:30:00Z")]
    pub timestamp: DateTime<Utc>,

    /// 服务名称
    #[schema(example = "agent-runner")]
    pub service: String,
}

impl HealthResponse {
    /// 创建健康响应
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            status: "healthy".to_string(),
            timestamp: Utc::now(),
            service: service.into(),
        }
    }
}
