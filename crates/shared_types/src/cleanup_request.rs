//! RAII 清理请求
//!
//! 当容器引用计数归零时发送，用于触发后台资源回收。

use crate::ServiceType;

/// RAII 清理请求（当容器引用计数归零时发送）
#[derive(Debug, Clone)]
pub struct CleanupRequest {
    /// 容器标识符（传给 runtime.stop_container_by_identifier）
    pub identifier: String,
    /// 容器名称（日志用）
    pub container_name: String,
    /// 服务类型
    pub service_type: ServiceType,
    /// 容器 IP（gRPC 连接池清理用）
    pub container_ip: String,
    /// K8s namespace（用于构建 K8s Service FQDN）
    pub namespace: String,
    /// K8s 集群域名
    pub cluster_domain: String,
    /// 关联的 project_id 列表（日志用）
    pub project_ids: Vec<String>,
}
