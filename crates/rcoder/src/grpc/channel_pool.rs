//! gRPC Channel 连接池
//!
//! 管理到各个 agent_runner 容器的 gRPC 连接

use anyhow::Result;
use dashmap::DashMap;
use shared_types::grpc::agent_service_client::AgentServiceClient;
use tonic::transport::Channel;
use tracing::{debug, info};

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
    /// 使用 DashMap entry API 消除 TOCTOU (Time-Of-Check-Time-Of-Use) 竞态条件
    /// 确保在高并发场景下不会重复创建连接
    pub async fn get_client(&self, addr: &str) -> Result<AgentServiceClient<Channel>> {
        // 🛡️ 使用 entry API 进行原子性检查和插入，消除 TOCTOU 竞态条件
        match self.channels.entry(addr.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(entry) => {
                debug!("📡 [gRPC] 复用现有连接: {}", addr);
                Ok(AgentServiceClient::new(entry.get().clone()))
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                // 创建新连接
                info!("🔌 [gRPC] 创建新连接: {}", addr);
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

                // 🛡️ 原子性插入连接，确保其他并发线程能看到这个连接
                let channel_ref = entry.insert(channel);
                Ok(AgentServiceClient::new(channel_ref.clone()))
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
            info!("🔌 [gRPC] 移除连接: {}", addr);
        }
    }

    /// 清空所有连接
    pub fn clear(&self) {
        self.channels.clear();
        info!("🔌 [gRPC] 清空所有连接");
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
