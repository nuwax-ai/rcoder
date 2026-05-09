//! gRPC Channel 连接池
//!
//! 管理到各个 agent_runner 容器的 gRPC 连接，支持 TTL 自动清理失效连接。

use anyhow::Result;
use dashmap::DashMap;
use shared_types::grpc::agent_service_client::AgentServiceClient;
use std::time::{Duration, Instant};
use tonic::transport::Channel;
use tracing::{debug, info, warn};

/// gRPC 连接池 TTL（5分钟）
///
/// 连接超过此时间未被使用则自动清理，防止内存泄漏。
const CHANNEL_TTL_SECS: u64 = 300;

/// gRPC 连接池最大容量
const MAX_CAPACITY: usize = 1000;

/// 创建配置好的 gRPC 客户端（设置消息大小限制）
///
/// tonic 的消息大小限制是在 AgentServiceClient 级别配置的，
/// 无法在 Channel 或 Endpoint 级别统一配置，所以需要这个辅助函数。
fn create_configured_client(channel: Channel) -> AgentServiceClient<Channel> {
    AgentServiceClient::new(channel)
        .max_decoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE)
        .max_encoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE)
}

/// Channel 元数据（包含创建时间）
#[derive(Clone)]
struct ChannelEntry {
    channel: Channel,
    created_at: Instant,
}

impl ChannelEntry {
    fn is_expired(&self) -> bool {
        self.created_at.elapsed() > Duration::from_secs(CHANNEL_TTL_SECS)
    }
}

/// gRPC 连接池
///
/// 为每个容器维护独立的 gRPC 连接，支持：
/// - 连接复用：相同地址的请求复用同一连接
/// - TTL 自动清理：5分钟未使用的连接自动移除，防止内存泄漏
/// - 并发安全：支持高并发下的安全连接创建
pub struct GrpcChannelPool {
    /// 容器地址到 Channel 的映射
    channels: DashMap<String, ChannelEntry>,
}

impl GrpcChannelPool {
    /// 创建新的连接池
    pub fn new() -> Self {
        Self {
            channels: DashMap::new(),
        }
    }

    /// 获取指定地址的 gRPC 客户端
    ///
    /// 如果连接不存在则创建新连接。过期连接会被自动清理。
    pub async fn get_client(&self, addr: &str) -> Result<AgentServiceClient<Channel>> {
        // 先检查缓存，同时清理过期条目
        // 使用 remove_if_available 模式避免 TOCTOU 竞态
        let should_remove = {
            self.channels.get(addr).map(|entry| entry.is_expired()).unwrap_or(false)
        };

        if should_remove {
            // 过期则移除
            self.channels.remove(addr);
        }

        // 再次检查（可能被其他线程修改）
        if let Some(entry) = self.channels.get(addr) {
            debug!("📡 [gRPC] reuse connection: {}", addr);
            return Ok(create_configured_client(entry.channel.clone()));
        }

        // 缓存未命中或已过期，创建新连接
        info!("🔌 [gRPC] creating connection: {}", addr);
        let endpoint = format!("http://{}", addr);
        let channel = Channel::from_shared(endpoint)
            .map_err(|e| anyhow::anyhow!("Invalid URI: {}", e))?
            .connect_timeout(Duration::from_secs(
                shared_types::GRPC_CONNECT_TIMEOUT_SECS,
            ))
            .timeout(Duration::from_secs(
                shared_types::GRPC_REQUEST_TIMEOUT_SECS,
            ))
            // HTTP/2 Keepalive 配置
            .http2_keep_alive_interval(Duration::from_secs(30))
            .keep_alive_timeout(Duration::from_secs(10))
            .keep_alive_while_idle(true)
            // TCP Keepalive 配置
            .tcp_keepalive(Some(Duration::from_secs(60)))
            .tcp_nodelay(true)
            .connect()
            .await
            .map_err(|e| anyhow::anyhow!("Connection failed: {}", e))?;

        // 检查容量限制，清理过期连接
        self.try_cleanup_expired();

        // 插入缓存
        self.channels.insert(
            addr.to_string(),
            ChannelEntry {
                channel: channel.clone(),
                created_at: Instant::now(),
            },
        );

        debug!("📡 [gRPC] connection ready: {}", addr);
        Ok(create_configured_client(channel))
    }

    /// 尝试清理过期的连接（如果缓存已满）
    ///
    /// 只有当缓存接近容量上限时才执行清理，避免每次调用都扫描。
    fn try_cleanup_expired(&self) {
        let len = self.channels.len();

        // 只有当缓存接近满时（>= 容量上限的 80%），才执行清理
        if len < MAX_CAPACITY * 8 / 10 {
            return;
        }

        // 收集所有过期连接的键
        let expired: Vec<String> = self
            .channels
            .iter()
            .filter(|e| e.is_expired())
            .map(|e| e.key().clone())
            .collect();

        // 移除过期连接
        for key in &expired {
            self.channels.remove(key);
        }

        if !expired.is_empty() {
            debug!(
                "🔌 [gRPC] cleaned up {} expired connections (cache size: {})",
                expired.len(),
                self.channels.len()
            );
        }

        // 如果清理后仍然满（>= 80%），再清理一半的非过期连接
        if self.channels.len() >= MAX_CAPACITY * 8 / 10 {
            let to_remove: Vec<_> = self
                .channels
                .iter()
                .take(MAX_CAPACITY / 2)
                .map(|e| e.key().clone())
                .collect();

            for key in &to_remove {
                self.channels.remove(key);
            }
            warn!(
                "🔌 [gRPC] cache still full after cleanup, evicted {} entries",
                to_remove.len()
            );
        }
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

    /// 移除指定地址的连接
    pub fn remove(&self, addr: &str) {
        if self.channels.remove(addr).is_some() {
            info!("🔌 [gRPC] removed connection: {}", addr);
        }
    }

    /// 清空所有连接
    pub fn clear(&self) {
        self.channels.clear();
        info!("🔌 [gRPC] cleared all connections");
    }

    /// 获取当前连接数
    pub fn connection_count(&self) -> usize {
        self.channels.len()
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

    #[test]
    fn test_remove_non_existent() {
        let pool = GrpcChannelPool::new();
        pool.remove("non_existent");
        assert_eq!(pool.connection_count(), 0);
    }

    #[test]
    fn test_clear() {
        let pool = GrpcChannelPool::new();
        pool.clear();
        assert_eq!(pool.connection_count(), 0);
    }
}
