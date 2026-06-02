//! gRPC Channel 连接池
//!
//! 管理到各个 agent_runner 容器的 gRPC 连接，支持 TTL 自动清理失效连接。

use anyhow::Result;
use dashmap::DashMap;
use shared_types::grpc::agent_mgmt_service_client::AgentMgmtServiceClient;
use shared_types::grpc::agent_service_client::AgentServiceClient;
use std::time::{Duration, Instant};
use tonic::transport::Channel;
use tracing::{debug, info, warn};

/// gRPC 连接池 TTL（5分钟）
///
/// 连接超过此时间未被使用则自动清理，防止内存泄漏。
/// 注意：基于最后使用时间而非创建时间，活跃连接不会被误清理。
const CHANNEL_TTL_SECS: u64 = 300;

/// gRPC 连接池最大容量
const MAX_CAPACITY: usize = 10000;

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

/// Channel 元数据（包含最后使用时间）
#[derive(Clone)]
struct ChannelEntry {
    channel: Channel,
    last_used: Instant,
}

impl ChannelEntry {
    fn is_expired(&self) -> bool {
        self.last_used.elapsed() > Duration::from_secs(CHANNEL_TTL_SECS)
    }

    fn touch(&mut self) {
        self.last_used = Instant::now();
    }
}

/// gRPC 连接池
///
/// 为每个容器维护独立的 gRPC 连接，支持：
/// - 连接复用：相同地址的请求复用同一连接
/// - TTL 自动清理：5分钟未使用的连接自动移除，防止内存泄漏（基于最后使用时间）
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
    /// 🚀 优化：使用 entry API 原子化检查和插入，消除 TOCTOU 竞态窗口
    pub async fn get_client(&self, addr: &str) -> Result<AgentServiceClient<Channel>> {
        let channel = self.get_or_create_channel(addr).await?;
        Ok(create_configured_client(channel))
    }

    /// 清理过期的连接
    ///
    /// 每次调用时检查并清理过期连接，保持缓存高效。
    fn cleanup_expired(&self) {
        let _len = self.channels.len();

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

    /// P0-4: 获取指定地址的 AgentMgmtServiceClient(用于 agent_mgmt 转发层)
    ///
    /// 复用 cache 中的 Channel(cheap clone),但 wrap 成 AgentMgmtServiceClient。
    /// 调用方负责传 project 维度的容器地址。
    pub async fn get_mgmt_client(
        &self,
        addr: &str,
    ) -> Result<AgentMgmtServiceClient<Channel>> {
        let channel = self.get_or_create_channel(addr).await?;
        Ok(create_mgmt_client(channel))
    }

    /// 获取或创建 Channel(公共逻辑,消除 get_client / get_mgmt_client 重复)
    ///
    /// 流程:缓存命中 → 创建新连接 → entry API 双重检查 → 写入缓存
    async fn get_or_create_channel(&self, addr: &str) -> Result<Channel> {
        // 快速路径:缓存命中且未过期
        if let Some(mut entry) = self.channels.get_mut(addr)
            && !entry.is_expired()
        {
            debug!("📡 [gRPC] reuse connection: {}", addr);
            entry.touch();
            return Ok(entry.channel.clone());
        }

        // 缓存未命中或已过期,创建新连接
        info!("🔌 [gRPC] creating connection: {}", addr);
        let endpoint = format!("http://{}", addr);
        let channel = Channel::from_shared(endpoint)
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
            .map_err(|e| anyhow::anyhow!("Connection failed: {}", e))?;

        self.cleanup_expired();

        // entry API 双重检查:避免并发创建重复连接
        match self.channels.entry(addr.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                let existing = entry.get_mut();
                if !existing.is_expired() {
                    debug!("📡 [gRPC] concurrent creation detected, reusing: {}", addr);
                    existing.touch();
                    return Ok(existing.channel.clone());
                }
                entry.insert(ChannelEntry {
                    channel: channel.clone(),
                    last_used: Instant::now(),
                });
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(ChannelEntry {
                    channel: channel.clone(),
                    last_used: Instant::now(),
                });
            }
        }

        Ok(channel)
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
