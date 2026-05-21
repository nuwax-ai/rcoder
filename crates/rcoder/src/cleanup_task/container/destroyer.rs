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
        }
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
            "🔥 [destroyer] Starting container destruction: container_name={}, service_type={:?}, identifier={}, reason={}",
            container_name,
            service_type,
            container_identifier,
            reason.as_str()
        );

        // 输出详细原因
        debug!("📋 [destroyer] destroy reason: {}", reason.description());

        // 1. 🔍 通过容器名称实时查询最新的容器信息
        // 这样可以获取最新的 container_id，避免使用缓存中过期的 ID
        let (actual_container_id, container_ip) = match self
            .docker_manager
            .find_container_realtime(container_name)
            .await
        {
            Ok(Some(result)) => {
                debug!(
                    "✅ [destroyer] Found container: name={}, id={}, ip={}",
                    container_name, result.container_id, result.container_ip
                );
                (result.container_id, result.container_ip)
            }
            Ok(None) => {
                // 容器不存在，可能已经被删除了，这不是错误
                info!(
                    "⚠️ [destroyer] Container does not exist, may have been deleted: name={}",
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
            "🧹 [destroyer] DockerManager memory cache cleaned: identifier={}",
            container_identifier
        );

        // 4. 清理关联资源
        // 清理 gRPC 连接池中的旧连接（避免复用已失效的 TCP 连接）
        if !container_ip.is_empty() {
            let old_grpc_addr = format!("{}:{}", container_ip, shared_types::GRPC_DEFAULT_PORT);
            self.grpc_pool.remove(&old_grpc_addr);
        }

        if *service_type == ServiceType::ComputerAgentRunner {
            // 清理 Pingora VNC 后端
            if let Some(ref pingora_service) = self.pingora_service {
                let _: Option<String> = pingora_service.remove_vnc_backend(container_identifier);
            }
        }

        info!(
            "✅ [destroyer] Container destruction completed: container_name={}, actual_id={}, reason={}",
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
