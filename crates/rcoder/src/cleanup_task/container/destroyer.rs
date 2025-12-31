//! 容器销毁器
//!
//! 销毁容器并清理相关资源（gRPC 连接池、Pingora VNC 后端、容器 IP 缓存）

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
    /// 容器 IP 缓存（用于销毁时失效）
    pub container_ip_cache: Option<Arc<crate::grpc::ContainerIpCache>>,
}

impl ContainerDestroyer {
    pub fn new(
        docker_manager: Arc<docker_manager::DockerManager>,
        grpc_pool: Arc<crate::grpc::GrpcChannelPool>,
        pingora_service: Option<Arc<rcoder_proxy::PingoraProxyService>>,
    ) -> Self {
        Self {
            docker_manager,
            grpc_pool,
            pingora_service,
            container_ip_cache: None,
        }
    }

    /// 设置容器 IP 缓存引用
    pub fn with_ip_cache(mut self, cache: Arc<crate::grpc::ContainerIpCache>) -> Self {
        self.container_ip_cache = Some(cache);
        self
    }

    /// 销毁容器并清理相关资源（带原因）
    ///
    /// # 参数
    /// * `container_id` - 容器 ID
    /// * `service_type` - 服务类型（用于决定是否清理 VNC 后端）
    /// * `container_identifier` - 容器标识符（project_id 或 user_id）
    /// * `reason` - 销毁原因
    pub async fn destroy_with_reason(
        &self,
        container_id: &str,
        service_type: &ServiceType,
        container_identifier: &str,
        reason: &DestroyReason,
    ) -> Result<()> {
        info!(
            "🔥 [destroyer] 开始销毁容器: container_id={}, service_type={:?}, identifier={}, 原因={}",
            container_id,
            service_type,
            container_identifier,
            reason.as_str()
        );

        // 输出详细原因
        debug!("📋 [destroyer] 销毁详情: {}", reason.description());

        // 1. 执行物理销毁（这会自动清理 gRPC 连接池中的连接）
        docker_manager::container_stop::runtime_cleanup_container(
            &self.docker_manager,
            container_id,
        )
        .await
        .map_err(|e| anyhow::anyhow!("停止容器失败: {}", e))?;

        // 2. 对于 ComputerAgentRunner，清理 Pingora VNC 后端
        if *service_type == ServiceType::ComputerAgentRunner {
            if let Some(ref pingora_service) = self.pingora_service {
                let _: Option<String> = pingora_service.remove_vnc_backend(container_identifier);
            }

            // 🆕 使容器 IP 缓存失效（容器名称格式: computer-agent-runner-{user_id}）
            if let Some(ref cache) = self.container_ip_cache {
                let container_name = format!(
                    "{}-{}",
                    ServiceType::ComputerAgentRunner.container_prefix(),
                    container_identifier
                );
                cache.invalidate(&container_name);
            }
        } else {
            // RCoder 模式：容器名称格式: rcoder-agent-{project_id}
            if let Some(ref cache) = self.container_ip_cache {
                let container_name = format!(
                    "{}-{}",
                    ServiceType::RCoder.container_prefix(),
                    container_identifier
                );
                cache.invalidate(&container_name);
            }
        }

        info!(
            "✅ [destroyer] 容器销毁完成: container_id={}, 原因={}",
            container_id,
            reason.as_str()
        );
        Ok(())
    }

    /// 销毁容器并清理相关资源（兼容旧接口）
    ///
    /// # 参数
    /// * `container_id` - 容器 ID
    /// * `service_type` - 服务类型（用于决定是否清理 VNC 后端）
    /// * `container_identifier` - 容器标识符（project_id 或 user_id）
    pub async fn destroy(
        &self,
        container_id: &str,
        service_type: &ServiceType,
        container_identifier: &str,
    ) -> Result<()> {
        // 使用默认原因
        let reason = DestroyReason::ManualStop {
            source: "unknown".to_string(),
        };
        self.destroy_with_reason(container_id, service_type, container_identifier, &reason)
            .await
    }
}
