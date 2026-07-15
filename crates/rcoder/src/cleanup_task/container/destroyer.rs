//! 容器销毁器
//!
//! 销毁容器并清理相关资源（gRPC 连接池、Pingora VNC 后端）

#![allow(dead_code)]

use anyhow::Result;
use shared_types::ServiceType;
use std::sync::Arc;
use tracing::{debug, info};

use crate::cleanup_task::strategies::DestroyReason;

/// 容器销毁器
pub struct ContainerDestroyer {
    pub docker_manager: Arc<docker_manager::DockerManager>,
    pub grpc_pool: Arc<crate::grpc::GrpcChannelPool>,
    pub pingora_service: Option<Arc<rcoder_proxy::PingoraProxyService>>,
    /// K8s namespace（用于构建 K8s Service FQDN）
    pub namespace: String,
    /// K8s 集群域名
    pub cluster_domain: String,
    /// 是否是 K8s 运行时
    pub is_kubernetes: bool,
}

impl ContainerDestroyer {
    pub fn new(
        docker_manager: Arc<docker_manager::DockerManager>,
        grpc_pool: Arc<crate::grpc::GrpcChannelPool>,
        pingora_service: Option<Arc<rcoder_proxy::PingoraProxyService>>,
        namespace: String,
        cluster_domain: String,
        is_kubernetes: bool,
    ) -> Self {
        Self {
            docker_manager,
            grpc_pool,
            pingora_service,
            namespace,
            cluster_domain,
            is_kubernetes,
        }
    }

    /// 销毁容器并清理相关资源（带原因）
    ///
    /// # 参数
    /// * `container_name` - 容器名称（稳定不变，优先使用）
    /// * `service_type` - 服务类型（用于决定是否清理 VNC 后端）
    /// * `container_identifier` - 容器标识符（project_id 或 user_id）
    /// * `reason` - 销毁原因
    /// * `project_id` - 项目 ID（用于清理 project_backends，可选）
    pub async fn destroy_with_reason(
        &self,
        container_name: &str,
        service_type: &ServiceType,
        container_identifier: &str,
        reason: &DestroyReason,
        project_id: Option<&str>,
    ) -> Result<()> {
        info!(
            " [destroyer] Starting container destruction: container_name={}, service_type={:?}, identifier={}, reason={}",
            container_name,
            service_type,
            container_identifier,
            reason.as_str()
        );

        // 输出详细原因
        debug!(" [destroyer] destroy reason: {}", reason.description());

        // 1. 🔍 通过容器名称实时查询最新的容器信息
        // 这样可以获取最新的 container_id，避免使用缓存中过期的 ID
        let (actual_container_id, container_ip) = match self
            .docker_manager
            .find_container_realtime(container_name)
            .await
        {
            Ok(Some(result)) => {
                debug!(
                    " [destroyer] Found container: name={}, id={}, ip={}",
                    container_name, result.container_id, result.container_ip
                );
                (result.container_id, result.container_ip)
            }
            Ok(None) => {
                // 容器不存在，可能已经被删除了，这不是错误
                info!(
                    " [destroyer] Container does not exist, may have been deleted: name={}",
                    container_name
                );
                return Ok(());
            }
            Err(e) => {
                // 查询出错，返回错误
                return Err(anyhow::anyhow!(
                    "Failed to query container info: name={}, error={}",
                    container_name,
                    e
                ));
            }
        };

        // 2. 执行物理销毁（使用最新的 container_id）
        docker_manager::container_stop::runtime_cleanup_container(
            &self.docker_manager,
            &actual_container_id,
        )
        .await
        .map_err(|e| anyhow::anyhow!("Failed to stop container: {}", e))?;

        // 3. 清理 DockerManager 内存缓存（防止缓存残留导致孤立容器无法被清理）
        let _: Option<_> = self
            .docker_manager
            .remove_container_cache(container_identifier)
            .await;
        debug!(
            " [destroyer] DockerManager memory cache cleaned: identifier={}",
            container_identifier
        );

        // 4. 清理关联资源
        // 清理 gRPC 连接池中的旧连接（避免复用已失效的 TCP 连接）。
        // Docker 环境 container_ip 为空时跳过(K8s 用 FQDN 不依赖 ip)。
        if !self.is_kubernetes && container_ip.is_empty() {
            debug!(" [destroyer] Container IP is empty, skipping gRPC cleanup");
            return Ok(());
        }
        let grpc_addr = shared_types::build_grpc_addr(
            container_name,
            &container_ip,
            &self.namespace,
            &self.cluster_domain,
        );
        self.grpc_pool.remove(&grpc_addr).await;

        if let Some(ref pingora_service) = self.pingora_service {
            if *service_type == ServiceType::ComputerAgentRunner {
                // 清理 Pingora VNC 后端
                let _: Option<String> = pingora_service.remove_vnc_backend(container_identifier);
            }

            // 清理 Pingora Project 后端（WebAgentRunner 容器）
            // 使用 project_id 而不是 container_identifier，因为 container_identifier 可能是 pod_id
            if *service_type == ServiceType::WebAgentRunner
                && let Some(pid) = project_id
            {
                let _: Option<String> = pingora_service.remove_project_backend(pid);
                debug!(
                    " [destroyer] Cleaned up project_backends: project_id={}",
                    pid
                );
            }
        }

        info!(
            " [destroyer] Container destruction completed: container_name={}, actual_id={}, reason={}",
            container_name,
            actual_container_id,
            reason.as_str()
        );
        Ok(())
    }

    /// 销毁容器并清理相关资源（兼容旧接口）
    ///
    /// # 参数
    /// * `container_name` - 容器名称
    /// * `service_type` - 服务类型（用于决定是否清理 VNC 后端）
    /// * `container_identifier` - 容器标识符（project_id 或 user_id）
    /// * `project_id` - 项目 ID（用于清理 project_backends，可选）
    pub async fn destroy(
        &self,
        container_name: &str,
        service_type: &ServiceType,
        container_identifier: &str,
        project_id: Option<&str>,
    ) -> Result<()> {
        // 使用默认原因
        let reason = DestroyReason::ManualStop {
            source: "unknown".to_string(),
        };
        self.destroy_with_reason(
            container_name,
            service_type,
            container_identifier,
            &reason,
            project_id,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcoder_proxy::PingoraProxyService;
    use rcoder_proxy::config::ProxyConfig;

    #[test]
    fn test_project_backends_cleanup() {
        // 创建 PingoraProxyService
        let config = ProxyConfig::default();
        let pingora_service = Arc::new(PingoraProxyService::new(config));

        // 添加 project_backends 映射
        pingora_service.add_project_backend("proj_123", "192.168.1.100");
        pingora_service.add_project_backend("proj_456", "192.168.1.200");

        // 验证映射已添加
        assert_eq!(pingora_service.project_backend_count(), 2);

        // 移除一个映射
        let removed = pingora_service.remove_project_backend("proj_123");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap(), "192.168.1.100");

        // 验证映射已移除
        assert_eq!(pingora_service.project_backend_count(), 1);

        // 验证另一个映射仍然存在
        let backends = pingora_service.list_project_backends();
        assert!(backends.contains_key("proj_456"));
        assert_eq!(backends.get("proj_456").unwrap(), "192.168.1.200");
    }

    #[test]
    fn test_vnc_backends_cleanup() {
        // 创建 PingoraProxyService
        let config = ProxyConfig::default();
        let pingora_service = Arc::new(PingoraProxyService::new(config));

        // 添加 vnc_backends 映射
        pingora_service.add_vnc_backend("user_123", "192.168.1.100");
        pingora_service.add_vnc_backend("user_456", "192.168.1.200");

        // 验证映射已添加
        assert_eq!(pingora_service.vnc_backend_count(), 2);

        // 移除一个映射
        let removed = pingora_service.remove_vnc_backend("user_123");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap(), "192.168.1.100");

        // 验证映射已移除
        assert_eq!(pingora_service.vnc_backend_count(), 1);

        // 验证另一个映射仍然存在
        let backends = pingora_service.list_vnc_backends();
        assert!(backends.contains_key("user_456"));
        assert_eq!(backends.get("user_456").unwrap(), "192.168.1.200");
    }

    #[test]
    fn test_project_backends_and_vnc_backends_independent() {
        // 创建 PingoraProxyService
        let config = ProxyConfig::default();
        let pingora_service = Arc::new(PingoraProxyService::new(config));

        // 添加映射
        pingora_service.add_project_backend("proj_123", "192.168.1.100");
        pingora_service.add_vnc_backend("user_123", "192.168.1.200");

        // 验证映射已添加
        assert_eq!(pingora_service.project_backend_count(), 1);
        assert_eq!(pingora_service.vnc_backend_count(), 1);

        // 移除 project_backends 映射
        pingora_service.remove_project_backend("proj_123");

        // 验证 vnc_backends 映射仍然存在
        assert_eq!(pingora_service.project_backend_count(), 0);
        assert_eq!(pingora_service.vnc_backend_count(), 1);
        assert!(pingora_service.list_vnc_backends().contains_key("user_123"));
    }
}
