//! 端口管理器 - 管理容器端口分配和回收

use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// 端口管理器
#[derive(Debug)]
pub struct PortManager {
    /// 已分配的端口
    allocated_ports: Arc<RwLock<HashSet<u16>>>,
    /// 端口分配范围
    port_range: (u16, u16),
}

impl PortManager {
    /// 创建新的端口管理器
    pub fn new() -> Self {
        Self::with_range(8000, 9999)
    }

    /// 指定端口范围创建端口管理器
    pub fn with_range(min_port: u16, max_port: u16) -> Self {
        Self {
            allocated_ports: Arc::new(RwLock::new(HashSet::new())),
            port_range: (min_port, max_port),
        }
    }

    /// 分配一个可用端口
    pub async fn allocate_port(&self) -> Result<u16, String> {
        let mut ports = self.allocated_ports.write().await;

        // 尝试在范围内找一个可用端口
        for port in self.port_range.0..=self.port_range.1 {
            if !ports.contains(&port) {
                // 检查端口是否真的可用
                if self.is_port_available(port).await {
                    ports.insert(port);
                    debug!("分配端口: {}", port);
                    return Ok(port);
                }
            }
        }

        Err(format!("端口范围内无可用端口: {}-{}",
                  self.port_range.0, self.port_range.1))
    }

    /// 释放端口
    pub async fn release_port(&self, port: u16) {
        let mut ports = self.allocated_ports.write().await;
        if ports.remove(&port) {
            debug!("释放端口: {}", port);
        } else {
            warn!("尝试释放未分配的端口: {}", port);
        }
    }

    /// 检查端口是否可用
    async fn is_port_available(&self, port: u16) -> bool {
        use tokio::net::TcpListener;

        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(_) => {
                // 立即关闭，端口就可用
                true
            }
            Err(_) => false,
        }
    }

    /// 获取已分配端口数量
    pub async fn allocated_count(&self) -> usize {
        self.allocated_ports.read().await.len()
    }

    /// 强制释放指定范围内的所有端口（用于清理）
    pub async fn clear_all(&self) {
        let mut ports = self.allocated_ports.write().await;
        let count = ports.len();
        ports.clear();
        info!("清理所有已分配端口: {} 个", count);
    }
}

impl Default for PortManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 全局端口管理器实例
pub static GLOBAL_PORT_MANAGER: std::sync::LazyLock<PortManager> =
    std::sync::LazyLock::new(PortManager::new);