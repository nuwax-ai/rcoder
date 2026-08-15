//! RAII 清理请求
//!
//! 当容器引用计数归零时发送，用于触发后台资源回收。
//! 生产端：rcoder-storage（ProjectAdapter / PgStore，try_send 非阻塞）；
//! 消费端：rcoder 的 ResourceReaper（物理销毁容器 + 清理 gRPC 池/SSE/Pingora）。

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
    /// re-enqueue 重试次数（0=首次，上限 MAX_STOP_RETRIES；reaper stop 失败时自增并重新入队）
    pub retry_count: u32,
}

/// cleanup 通道容量（bounded）。
///
/// 清理请求本身低频, 但突发批量回收 (cleanup_task 批量回收 / userapp 自动回收潮)
/// 会瞬时涌入; 通道满时生产端 try_send 丢弃且无孤儿容器对账兼保险机制 (丢弃 = 物理容器泄漏),
/// 故容量取大值提高突发吸收上限。单条消息仅几个 String, 4096 条内存代价只有几 MB。
/// 注: 真正的吞吐瓶颈在消费端 (单 task 串行 + 120s 超时), 容量只决定突发缓冲上限。
pub const CLEANUP_CHANNEL_CAPACITY: usize = 4096;
