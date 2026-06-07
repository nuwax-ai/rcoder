//! RAII 资源回收器
//!
//! 后台异步任务，接收 CleanupRequest 并执行物理容器销毁 + 资源清理。
//! 使用 tokio mpsc channel 与 ProjectAdapter 解耦（同步发送、异步处理）。

use std::sync::Arc;

use shared_types::{GRPC_DEFAULT_PORT, ServiceType};
use tracing::{info, warn};

use container_runtime_api::ContainerRuntime;

/// RAII 清理请求（当容器引用计数归零时发送）
#[derive(Debug, Clone)]
pub struct CleanupRequest {
    /// 容器标识符（传给 runtime.stop_container_by_identifier）
    pub identifier: String,
    /// 容器名称（日志用）
    pub container_name: String,
    /// 服务类型
    pub service_type: ServiceType,
    /// 容器 IP（gRPC 连接池清理用）
    pub container_ip: String,
    /// 关联的 project_id 列表（日志用）
    pub project_ids: Vec<String>,
}

/// 后台资源回收器
pub struct ResourceReaper {
    rx: tokio::sync::mpsc::UnboundedReceiver<CleanupRequest>,
    runtime: Arc<dyn ContainerRuntime>,
    grpc_pool: Arc<crate::grpc::GrpcChannelPool>,
    pingora: Option<Arc<rcoder_proxy::PingoraProxyService>>,
    docker_manager: Option<Arc<docker_manager::DockerManager>>,
}

impl ResourceReaper {
    pub fn new(
        rx: tokio::sync::mpsc::UnboundedReceiver<CleanupRequest>,
        runtime: Arc<dyn ContainerRuntime>,
        grpc_pool: Arc<crate::grpc::GrpcChannelPool>,
        pingora: Option<Arc<rcoder_proxy::PingoraProxyService>>,
        docker_manager: Option<Arc<docker_manager::DockerManager>>,
    ) -> Self {
        Self {
            rx,
            runtime,
            grpc_pool,
            pingora,
            docker_manager,
        }
    }

    /// 主循环：持续接收并处理清理请求
    pub async fn run(mut self) {
        info!("[REAPER] started");
        while let Some(req) = self.rx.recv().await {
            self.process_cleanup(req).await;
        }
        info!("[REAPER] shutdown");
    }

    /// 处理单个清理请求
    async fn process_cleanup(&self, req: CleanupRequest) {
        info!(
            "[REAPER] processing: identifier={}, service_type={:?}, projects={:?}",
            req.identifier, req.service_type, req.project_ids
        );

        // 1. 物理销毁容器
        match self
            .runtime
            .stop_container_by_identifier(&req.identifier, &req.service_type)
            .await
        {
            Ok(()) => info!("[REAPER] destroyed container: {}", req.container_name),
            Err(e) => {
                warn!(
                    "[REAPER] failed to stop container {}: {}",
                    req.container_name, e
                );
                // 继续清理其他资源，不因 stop 失败而中断
            }
        }

        // 2. 清理 gRPC 连接
        if !req.container_ip.is_empty() {
            let addr = format!("{}:{}", req.container_ip, GRPC_DEFAULT_PORT);
            self.grpc_pool.remove(&addr);
        }

        // 3. 清理 DockerManager 缓存（Docker 模式）
        if let Some(ref dm) = self.docker_manager {
            let removed = dm.remove_container_cache(&req.identifier).await;
            if removed.is_some() {
                info!("[REAPER] cleaned DockerManager cache for {}", req.identifier);
            }
        }

        // 4. 清理 Pingora VNC backend（ComputerAgentRunner）
        if req.service_type == ServiceType::ComputerAgentRunner {
            if let Some(ref pingora) = self.pingora {
                pingora.remove_vnc_backend(&req.identifier);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cleanup_request_debug() {
        let req = CleanupRequest {
            identifier: "user-123".to_string(),
            container_name: "agent-user-123".to_string(),
            service_type: ServiceType::ComputerAgentRunner,
            container_ip: "10.0.0.1".to_string(),
            project_ids: vec!["proj-1".to_string()],
        };
        let debug_str = format!("{:?}", req);
        assert!(debug_str.contains("user-123"));
    }
}
