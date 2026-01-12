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
    /// * `container_name` - 容器名称（稳定不变，优先使用）
    /// * `service_type` - 服务类型（用于决定是否清理 VNC 后端）
    /// * `container_identifier` - 容器标识符（project_id 或 user_id）
    /// * `reason` - 销毁原因
    pub async fn destroy_with_reason(
        &self,
        container_name: &str,
        service_type: &ServiceType,
        container_identifier: &str,
        reason: &DestroyReason,
    ) -> Result<()> {
        info!(
            "🔥 [destroyer] 开始销毁容器: container_name={}, service_type={:?}, identifier={}, 原因={}",
            container_name,
            service_type,
            container_identifier,
            reason.as_str()
        );

        // 输出详细原因
        debug!("📋 [destroyer] 销毁详情: {}", reason.description());

        // 1. 🔍 通过容器名称实时查询最新的容器信息
        // 这样可以获取最新的 container_id，避免使用缓存中过期的 ID
        let actual_container_id = match self
            .docker_manager
            .find_container_realtime(container_name)
            .await
        {
            Ok(Some((container_id, _, _, _))) => {
                debug!(
                    "✅ [destroyer] 找到容器: name={}, id={}",
                    container_name, container_id
                );
                container_id
            }
            Ok(None) => {
                // 容器不存在，可能已经被删除了，这不是错误
                info!(
                    "⚠️ [destroyer] 容器不存在，可能已被删除: name={}",
                    container_name
                );
                return Ok(());
            }
            Err(e) => {
                // 查询出错，返回错误
                return Err(anyhow::anyhow!(
                    "查询容器信息失败: name={}, error={}",
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
        .map_err(|e| anyhow::anyhow!("停止容器失败: {}", e))?;

        // 3. 清理 DockerManager 内存缓存（防止缓存残留导致孤立容器无法被清理）
        let _: Option<_> = self.docker_manager.remove_container_cache(container_identifier).await;
        debug!(
            "🧹 [destroyer] 已清理 DockerManager 内存缓存: identifier={}",
            container_identifier
        );

        // 4. 对于 ComputerAgentRunner，清理 Pingora VNC 后端
        if *service_type == ServiceType::ComputerAgentRunner {
            if let Some(ref pingora_service) = self.pingora_service {
                let _: Option<String> = pingora_service.remove_vnc_backend(container_identifier);
            }

            // 🆕 使容器 IP 缓存失效
            if let Some(ref cache) = self.container_ip_cache {
                cache.invalidate(container_name);
            }
        } else {
            // RCoder 模式：使容器 IP 缓存失效
            if let Some(ref cache) = self.container_ip_cache {
                cache.invalidate(container_name);
            }
        }

        info!(
            "✅ [destroyer] 容器销毁完成: container_name={}, actual_id={}, 原因={}",
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
    pub async fn destroy(
        &self,
        container_name: &str,
        service_type: &ServiceType,
        container_identifier: &str,
    ) -> Result<()> {
        // 使用默认原因
        let reason = DestroyReason::ManualStop {
            source: "unknown".to_string(),
        };
        self.destroy_with_reason(container_name, service_type, container_identifier, &reason)
            .await
    }
}
