//! gRPC Channel 连接池
//!
//! 管理到各个 agent_runner 容器的 gRPC 连接

use anyhow::Result;
use dashmap::DashMap;
use shared_types::grpc::agent_service_client::AgentServiceClient;
use tonic::transport::Channel;
use tracing::{debug, info};

/// 创建配置好的 gRPC 客户端（设置消息大小限制）
///
/// tonic 的消息大小限制是在 AgentServiceClient 级别配置的，
/// 无法在 Channel 或 Endpoint 级别统一配置，所以需要这个辅助函数。
fn create_configured_client(channel: Channel) -> AgentServiceClient<Channel> {
    AgentServiceClient::new(channel)
        .max_decoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE)
        .max_encoding_message_size(shared_types::GRPC_MAX_MESSAGE_SIZE)
}

/// gRPC 连接池
///
/// 为每个容器维护独立的 gRPC 连接，支持连接复用
pub struct GrpcChannelPool {
    /// 容器地址到 gRPC Channel 的映射
    channels: DashMap<String, Channel>,
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
    /// 如果连接不存在则创建新连接
    ///
    /// # 并发安全性
    ///
    /// 使用三阶段模式避免在持有 entry 期间调用 `.await`：
    ///
    /// 1. **快速检查**：先检查连接是否已存在
    /// 2. **创建连接**：如果不存在，在**不持有锁**的情况下创建连接（.await）
    /// 3. **原子性插入**：使用 entry API 原子性插入，如果其他线程已创建则使用已存在的
    ///
    /// 这样确保：
    /// - 不会在持有 DashMap entry 期间跨越 await 点
    /// - 高并发下同一地址最多只有一个连接被实际使用
    /// - 避免了 TOCTOU 竞态条件
    pub async fn get_client(&self, addr: &str) -> Result<AgentServiceClient<Channel>> {
        use dashmap::mapref::entry::Entry;

        // 第一阶段：快速检查（无锁读）
        if let Some(entry) = self.channels.get(addr) {
 debug!("📡 [gRPC] reuse message connection: {}", addr);
            return Ok(create_configured_client(entry.value().clone()));
        }

        // 第二阶段：创建连接（不持有任何锁）
 info!("🔌 [gRPC] created message connection: {}", addr);
        let endpoint = format!("http://{}", addr);
        let channel = Channel::from_shared(endpoint)
            .map_err(|e| anyhow::anyhow!("Invalid URI: {}", e))?
            .connect_timeout(std::time::Duration::from_secs(
                shared_types::GRPC_CONNECT_TIMEOUT_SECS,
            ))
            .timeout(std::time::Duration::from_secs(
                shared_types::GRPC_REQUEST_TIMEOUT_SECS,
            ))
            // HTTP/2 Keepalive 配置
            .http2_keep_alive_interval(std::time::Duration::from_secs(30))
            .keep_alive_timeout(std::time::Duration::from_secs(10))
            .keep_alive_while_idle(true)
            // TCP Keepalive 配置
            .tcp_keepalive(Some(std::time::Duration::from_secs(60)))
            .tcp_nodelay(true)
            .connect()
            .await
            .map_err(|e| anyhow::anyhow!("Connection failed: {}", e))?;

        // 第三阶段：原子性插入
        match self.channels.entry(addr.to_string()) {
            Entry::Vacant(entry) => {
                // 其他线程还没有创建，使用我们创建的连接
 debug!("📡 [gRPC] message connectionalready message : {}", addr);
                entry.insert(channel.clone());
                Ok(create_configured_client(channel))
            }
            Entry::Occupied(entry) => {
                // 其他线程已经创建了连接，使用已存在的（丢弃我们创建的）
 debug!("📡 [gRPC] message created message connection: {}", addr);
                Ok(create_configured_client(entry.get().clone()))
            }
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
 info!("🔌 [gRPC] removedconnection: {}", addr);
        }
    }

    /// 清空所有连接
    pub fn clear(&self) {
        self.channels.clear();
 info!("🔌 [gRPC] message empty message connection");
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
