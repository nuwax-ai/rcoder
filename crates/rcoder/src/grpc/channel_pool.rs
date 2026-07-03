//! gRPC Channel 连接池
//!
//! 管理到各个 agent_runner 容器的 gRPC 连接，支持 TTL 自动清理失效连接。
//!
//! ## M2 修复
//!
//! 历史实现用 `DashMap` 手写 TTL + 粗暴容量管理（接近 80% 时随机驱逐一半），
//! DashMap iter 顺序无序导致可能驱逐活跃连接。
//!
//! 现改用 `moka::future::Cache`：
//! - **真 LRU** + **TTI**（time-to-idle）淘汰，活跃连接不会被误清理
//! - **并发安全**：内置并发哈希表，无需 entry API 手动双重检查
//! - **容量上限 8000**：到容量自动按 LRU 淘汰最久未用的连接

use anyhow::Result;
use moka::future::Cache;
use shared_types::grpc::agent_mgmt_service_client::AgentMgmtServiceClient;
use shared_types::grpc::agent_service_client::AgentServiceClient;
use std::time::Duration;
use tonic::transport::Channel;
use tracing::{debug, info, warn};

/// gRPC 连接池 TTL（5分钟）
///
/// 连接超过此时间未被使用则自动清理，防止内存泄漏。
/// moka 的 `time_to_idle` 实现真 LRU + TTI 语义。
const CHANNEL_TTL_SECS: u64 = 300;

/// gRPC 连接池最大容量
///
/// 大量并发容器（K8s 模式下每 project 一个 Pod）时上限保护。
const MAX_CAPACITY: usize = 8000;

/// 创建配置好的 gRPC 客户端（设置消息大小限制）
///
/// tonic 的消息大小限制是在 AgentServiceClient 级别配置的，
/// 无法在 Channel 或 Endpoint 级别统一配置，所以需要这个辅助函数。
fn create_configured_client(channel: Channel) -> AgentServiceClient<Channel> {
    AgentServiceClient::new(channel)
        .max_decoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE)
        .max_encoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE)
}

/// P0-4: 创建配置好的 AgentMgmtServiceClient(共享 Channel,但独立的消息大小限制)
fn create_mgmt_client(channel: Channel) -> AgentMgmtServiceClient<Channel> {
    AgentMgmtServiceClient::new(channel)
        .max_decoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE)
        .max_encoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE)
}

/// gRPC 连接池
///
/// 为每个容器维护独立的 gRPC 连接，支持：
/// - 连接复用：相同地址的请求复用同一连接
/// - LRU + TTI 淘汰：5分钟未使用或容量达上限时自动淘汰最旧连接
/// - 并发安全：moka 内置无锁并发哈希表
pub struct GrpcChannelPool {
    /// moka 异步缓存：addr → Channel
    ///
    /// - `max_capacity`：8000（超过后按 LRU 淘汰）
    /// - `time_to_idle`：5 分钟未访问自动淘汰
    channels: Cache<String, Channel>,
}

impl GrpcChannelPool {
    /// 创建新的连接池
    pub fn new() -> Self {
        let channels = Cache::builder()
            .max_capacity(MAX_CAPACITY as u64)
            .time_to_idle(Duration::from_secs(CHANNEL_TTL_SECS))
            .eviction_listener(|key, _value, cause| {
                debug!(
                    " [gRPC] moka evicted connection: addr={}, cause={:?}",
                    key, cause
                );
            })
            .build();
        Self { channels }
    }

    /// 获取指定地址的 gRPC 客户端
    ///
    /// 如果连接不存在则创建新连接。
    pub async fn get_client(&self, addr: &str) -> Result<AgentServiceClient<Channel>> {
        let channel = self.get_or_create_channel(addr).await?;
        Ok(create_configured_client(channel))
    }

    /// 获取指定容器端口的 gRPC 客户端
    ///
    /// 假设容器 IP 为 localhost，端口为 gRPC 端口（默认 50051）
    pub async fn get_client_for_container(
        &self,
        container_ip: &str,
        grpc_port: u16,
    ) -> Result<AgentServiceClient<Channel>> {
        let addr = format!("{}:{}", container_ip, grpc_port);
        self.get_client(&addr).await
    }

    /// P0-4: 获取指定地址的 AgentMgmtServiceClient(用于 agent_mgmt 转发层)
    pub async fn get_mgmt_client(&self, addr: &str) -> Result<AgentMgmtServiceClient<Channel>> {
        let channel = self.get_or_create_channel(addr).await?;
        Ok(create_mgmt_client(channel))
    }

    /// 获取或创建 Channel
    ///
    /// moka 的 `try_get_with` 提供原子性"查或建"语义：
    /// - 缓存命中：返回 cheap clone（lock-free）
    /// - 缓存未命中：执行 `init_future`，期间并发请求会等待第一个完成（**不会重复建连**）
    ///
    /// **关键设计（Issue 4 修复）**：`Channel::connect()` 必须放在 `try_get_with` 的
    /// init future **内部**，否则并发场景下多个线程会各 connect 一个 channel，
    /// 只有一个写入缓存，其他被丢弃，浪费 TCP+HTTP/2 握手。
    async fn get_or_create_channel(&self, addr: &str) -> Result<Channel> {
        // 快速路径：moka get 是 lock-free 的
        if let Some(channel) = self.channels.get(addr).await {
            debug!(" [gRPC] reuse connection: {}", addr);
            return Ok(channel);
        }

        // 慢路径：try_get_with 原子化"查或建"
        // 把 connect 放进 init future 内部，并发请求共享同一个 connect future
        let addr_key = addr.to_string();
        let channel = self
            .channels
            .try_get_with(addr_key, async move {
                info!(" [gRPC] creating connection: {}", addr);
                let endpoint = format!("http://{}", addr);
                Channel::from_shared(endpoint)
                    .map_err(|e| anyhow::anyhow!("Invalid URI: {}", e))?
                    .connect_timeout(Duration::from_secs(shared_types::GRPC_CONNECT_TIMEOUT_SECS))
                    .timeout(Duration::from_secs(shared_types::GRPC_REQUEST_TIMEOUT_SECS))
                    .http2_keep_alive_interval(Duration::from_secs(30))
                    .keep_alive_timeout(Duration::from_secs(10))
                    .keep_alive_while_idle(true)
                    .tcp_keepalive(Some(Duration::from_secs(60)))
                    .tcp_nodelay(true)
                    .connect()
                    .await
                    .map_err(|e| anyhow::anyhow!("Connection failed: {}", e))
            })
            .await
            .map_err(|e| {
                warn!(" [gRPC] try_get_with failed: {}", e);
                anyhow::anyhow!("Failed to get or create channel: {}", e)
            })?;

        Ok(channel)
    }

    /// 移除指定地址的连接（异步，等待 invalidate 完成才返回）
    ///
    /// 用于连接失败后的失效清理（让下次请求重建）。
    ///
    /// **重要**：必须 `await` 此方法，确保下一次 `get_client` 不会命中刚 remove 的坏连接。
    /// moka 的 `invalidate` 是异步的，调用方必须在重试前等待完成。
    ///
    /// Bug 7 修复：原 fire-and-forget 版本会让 chat_handler 重试时拿到刚 remove 的坏连接。
    pub async fn remove(&self, addr: &str) {
        // moka 的 invalidate 是 async，直接 await
        // entry_count 是 O(1) 近似值，用于日志判断
        let had = self.channels.contains_key(addr);
        self.channels.invalidate(addr).await;
        if had {
            info!(" [gRPC] removed connection: {}", addr);
        }
    }

    /// 清空所有连接
    pub fn clear(&self) {
        self.channels.invalidate_all();
        info!(" [gRPC] cleared all connections");
    }

    /// 获取当前连接数
    ///
    /// 注意：moka 的 `entry_count` 是近似值（基于内部采样），用于监控足够。
    pub fn connection_count(&self) -> usize {
        self.channels.entry_count() as usize
    }
}

impl Default for GrpcChannelPool {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for GrpcChannelPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrpcChannelPool")
            .field("connection_count", &self.connection_count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_pool() {
        let pool = GrpcChannelPool::new();
        assert_eq!(pool.connection_count(), 0);
    }

    #[tokio::test]
    async fn test_remove_non_existent() {
        let pool = GrpcChannelPool::new();
        pool.remove("non_existent").await;
        assert_eq!(pool.connection_count(), 0);
    }

    #[test]
    fn test_clear() {
        let pool = GrpcChannelPool::new();
        pool.clear();
        assert_eq!(pool.connection_count(), 0);
    }

    #[test]
    fn test_default() {
        let pool = GrpcChannelPool::default();
        assert_eq!(pool.connection_count(), 0);
    }

    #[test]
    fn test_debug_format() {
        let pool = GrpcChannelPool::new();
        let debug_str = format!("{:?}", pool);
        assert!(debug_str.contains("GrpcChannelPool"));
    }
}
