//! Pingora 反向代理 API 数据结构
//!
//! 为 OpenAPI 文档提供 Pingora 代理接口的数据结构定义。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Pingora 代理路径参数
#[derive(Debug, Deserialize, ToSchema)]
#[allow(dead_code)] // 字段由 axum 框架自动提取使用
pub struct ProxyPathParams {
    /// 目标端口号
    #[schema(example = 3000)]
    pub port: u16,
}

/// Pingora 代理路径和尾部参数
#[derive(Debug, Deserialize, ToSchema)]
#[allow(dead_code)] // 字段由 axum 框架自动提取使用
pub struct ProxyPathWithTailParams {
    /// 目标端口号
    #[schema(example = 3000)]
    pub port: u16,
    /// 路径尾部 (剩余的路径部分)
    #[schema(example = "/api/users")]
    pub path: String,
}

/// Pingora 代理响应
#[derive(Debug, Serialize, ToSchema)]
pub struct ProxyResponse {
    /// 代理状态
    #[schema(example = true)]
    pub success: bool,
    /// 目标端口
    #[schema(example = 3000)]
    pub target_port: u16,
    /// 目标主机
    #[schema(example = "127.0.0.1")]
    pub target_host: String,
    /// 目标URL
    #[schema(example = "http://127.0.0.1:3000/api/users")]
    pub target_url: String,
    /// 响应时间 (毫秒)
    #[schema(example = 45)]
    pub response_time_ms: Option<u64>,
    /// 负载均衡器信息
    pub load_balancer: LoadBalancerInfo,
}

/// 负载均衡器信息
#[derive(Debug, Serialize, ToSchema)]
pub struct LoadBalancerInfo {
    /// 负载均衡算法
    #[schema(example = "round-robin")]
    pub algorithm: String,
    /// 是否启用健康检查
    #[schema(example = true)]
    pub health_check_enabled: bool,
    /// 后端服务数量
    #[schema(example = 3)]
    pub backend_count: usize,
}

/// Pingora 代理状态信息
#[derive(Debug, Serialize, ToSchema)]
pub struct ProxyStatus {
    /// 代理服务状态
    #[schema(example = "running")]
    pub status: String,
    /// 监听端口
    #[schema(example = 8080)]
    pub listen_port: u16,
    /// 默认后端端口
    #[schema(example = 3000)]
    pub default_backend_port: u16,
    /// 默认后端主机
    #[schema(example = "127.0.0.1")]
    pub default_backend_host: String,
    /// 已配置的后端服务
    pub backends: Vec<BackendInfo>,
    /// 负载均衡配置
    pub load_balancer: LoadBalancerInfo,
}

/// 后端服务信息
#[derive(Debug, Serialize, ToSchema)]
pub struct BackendInfo {
    /// 端口号
    #[schema(example = 3000)]
    pub port: u16,
    /// 主机地址
    #[schema(example = "127.0.0.1")]
    pub host: String,
    /// 健康状态
    #[schema(example = "healthy")]
    pub health_status: String,
    /// 最后检查时间
    #[schema(example = "2025-01-12T10:30:00Z")]
    pub last_check: String,
}

/// Pingora 代理错误响应
#[derive(Debug, Serialize, ToSchema)]
pub struct ProxyErrorResponse {
    /// 错误代码
    #[schema(example = "BACKEND_NOT_FOUND")]
    pub error: String,
    /// 错误消息
    #[schema(example = "未找到端口 9999 对应的后端服务")]
    pub message: String,
    /// 目标端口
    #[schema(example = 9999)]
    pub target_port: u16,
    /// 请求时间戳
    #[schema(example = "2025-01-12T10:30:00Z")]
    pub timestamp: String,
}

/// 代理统计信息
#[derive(Debug, Serialize, ToSchema)]
pub struct ProxyStats {
    /// 总请求数
    #[schema(example = 15420)]
    pub total_requests: u64,
    /// 成功请求数
    #[schema(example = 15200)]
    pub successful_requests: u64,
    /// 失败请求数
    #[schema(example = 220)]
    pub failed_requests: u64,
    /// 平均响应时间 (毫秒)
    #[schema(example = 35.5)]
    pub avg_response_time_ms: f64,
    /// 当前活跃连接数
    #[schema(example = 12)]
    pub active_connections: u32,
    /// 按端口统计
    pub port_stats: Vec<PortStats>,
}

/// 端口统计信息
#[derive(Debug, Serialize, ToSchema)]
pub struct PortStats {
    /// 端口号
    #[schema(example = 3000)]
    pub port: u16,
    /// 请求数
    #[schema(example = 8560)]
    pub requests: u64,
    /// 成功率
    #[schema(example = 0.987)]
    pub success_rate: f64,
    /// 平均响应时间
    #[schema(example = 28.3)]
    pub avg_response_time_ms: f64,
}

/// 代理配置信息
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ProxyConfig {
    /// 监听端口
    #[schema(example = 8080)]
    pub listen_port: u16,
    /// 默认后端端口
    #[schema(example = 3000)]
    pub default_backend_port: u16,
    /// 默认后端主机
    #[schema(example = "127.0.0.1")]
    pub default_backend_host: String,
    /// 负载均衡算法
    #[schema(example = "round-robin")]
    pub load_balancing_algorithm: String,
    /// 健康检查配置
    pub health_check: HealthCheckConfig,
}

/// 健康检查配置
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct HealthCheckConfig {
    /// 是否启用
    #[schema(example = true)]
    pub enabled: bool,
    /// 检查间隔 (秒)
    #[schema(example = 5)]
    pub interval_seconds: u32,
    /// 超时时间 (秒)
    #[schema(example = 3)]
    pub timeout_seconds: u32,
    /// 健康阈值
    #[schema(example = 2)]
    pub healthy_threshold: u32,
    /// 不健康阈值
    #[schema(example = 3)]
    pub unhealthy_threshold: u32,
}
