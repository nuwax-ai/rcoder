//! VNC 后端解析器模块
//!
//! 提供动态解析 VNC 后端容器 IP 的接口，支持透明代理。
//! 使用 trait 抽象解耦 pingora-proxy 和 docker_manager。

use async_trait::async_trait;
use std::sync::Arc;

/// VNC 后端解析错误
#[derive(Debug, thiserror::Error)]
pub enum VncResolveError {
    /// 容器不存在
    #[error("容器不存在: user_id={0}")]
    ContainerNotFound(String),

    /// 容器未运行
    #[error("容器未运行: user_id={0}")]
    ContainerNotRunning(String),

    /// 无法获取容器 IP
    #[error("无法获取容器 IP: {0}")]
    IpNotAvailable(String),

    /// 查询失败
    #[error("查询失败: {0}")]
    QueryFailed(String),
}

/// VNC 后端信息
#[derive(Debug, Clone)]
pub struct VncBackendInfo {
    /// 容器 IP
    pub container_ip: String,
    /// VNC 端口 (通常 6080)
    pub vnc_port: u16,
    /// 容器是否运行中
    pub is_running: bool,
}

impl VncBackendInfo {
    /// 创建新的 VNC 后端信息
    pub fn new(container_ip: String, vnc_port: u16, is_running: bool) -> Self {
        Self {
            container_ip,
            vnc_port,
            is_running,
        }
    }

    /// 获取完整的后端地址 (IP:端口)
    pub fn backend_addr(&self) -> String {
        format!("{}:{}", self.container_ip, self.vnc_port)
    }
}

/// VNC 后端解析器 trait
///
/// 用于解耦 pingora-proxy 和 docker_manager，
/// 允许不同的实现策略（直接查询 Docker、缓存查询等）
#[async_trait]
pub trait VncBackendResolver: Send + Sync {
    /// 根据 user_id 解析 VNC 后端信息
    ///
    /// # Arguments
    /// * `user_id` - 用户 ID（ComputerAgentRunner 模式下即容器标识）
    ///
    /// # Returns
    /// * `Ok(VncBackendInfo)` - 成功解析到 VNC 后端
    /// * `Err(VncResolveError)` - 解析失败
    async fn resolve(&self, user_id: &str) -> Result<VncBackendInfo, VncResolveError>;

    /// 检查用户是否有对应的容器（快速检查，不获取详细信息）
    ///
    /// 用于快速判断是否需要返回 404
    async fn exists(&self, user_id: &str) -> bool;
}

/// 用于类型擦除的 Arc 包装
pub type DynVncBackendResolver = Arc<dyn VncBackendResolver>;

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的 Mock 解析器
    struct MockResolver {
        users: std::collections::HashMap<String, VncBackendInfo>,
    }

    #[async_trait]
    impl VncBackendResolver for MockResolver {
        async fn resolve(&self, user_id: &str) -> Result<VncBackendInfo, VncResolveError> {
            self.users
                .get(user_id)
                .cloned()
                .ok_or_else(|| VncResolveError::ContainerNotFound(user_id.to_string()))
        }

        async fn exists(&self, user_id: &str) -> bool {
            self.users.contains_key(user_id)
        }
    }

    #[tokio::test]
    async fn test_mock_resolver() {
        let mut users = std::collections::HashMap::new();
        users.insert(
            "user_123".to_string(),
            VncBackendInfo::new("172.17.0.5".to_string(), 6080, true),
        );

        let resolver = MockResolver { users };

        // 测试解析成功
        let info = resolver.resolve("user_123").await.unwrap();
        assert_eq!(info.container_ip, "172.17.0.5");
        assert_eq!(info.vnc_port, 6080);
        assert!(info.is_running);

        // 测试解析失败
        let err = resolver.resolve("nonexistent").await.unwrap_err();
        assert!(matches!(err, VncResolveError::ContainerNotFound(_)));

        // 测试存在性检查
        assert!(resolver.exists("user_123").await);
        assert!(!resolver.exists("nonexistent").await);
    }

    #[test]
    fn test_backend_addr() {
        let info = VncBackendInfo::new("192.168.1.100".to_string(), 6080, true);
        assert_eq!(info.backend_addr(), "192.168.1.100:6080");
    }
}
