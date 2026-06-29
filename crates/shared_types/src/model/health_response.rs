//! 健康检查响应类型
//!
//! 提供统一的健康检查响应结构，供所有服务复用

use chrono::{DateTime, Utc};
use utoipa::ToSchema;

/// 健康检查响应结构
///
/// 用于所有服务的健康检查端点返回统一格式
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
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

/// 健康检查详细响应结构
///
/// 包含 HTTP 和 gRPC 服务的就绪状态
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
pub struct HealthCheckResponse {
    /// 服务状态：healthy（完全就绪）、starting（启动中）
    #[schema(example = "healthy")]
    pub status: String,

    /// 时间戳
    #[schema(example = "2024-01-15T10:30:00Z")]
    pub timestamp: DateTime<Utc>,

    /// 服务名称
    #[schema(example = "agent-runner")]
    pub service: String,

    /// HTTP 服务是否就绪
    #[schema(example = true)]
    pub http_ready: bool,

    /// gRPC 服务是否就绪
    #[schema(example = true)]
    pub grpc_ready: bool,
}

impl HealthCheckResponse {
    /// 创建健康检查详细响应
    pub fn new(service: impl Into<String>, http_ready: bool, grpc_ready: bool) -> Self {
        Self {
            status: if grpc_ready {
                "healthy".to_string()
            } else {
                "starting".to_string()
            },
            timestamp: Utc::now(),
            service: service.into(),
            http_ready,
            grpc_ready,
        }
    }
}
