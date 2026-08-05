//! RAII 资源回收器
//!
//! 后台异步任务，接收 CleanupRequest 并执行物理容器销毁 + 资源清理。
//! 使用 tokio mpsc channel 与 ProjectAdapter 解耦（同步发送、异步处理）。
//!
//! ## m2 文档：并发模型与堆积风险
//!
//! - **单消费者**：单个 tokio task 串行处理清理请求。
//! - **120s 超时**：单个清理操作超时则跳过（防止慢容器 stop 阻塞队列）。
//! - **unbounded channel**：生产者（chat 路径上的 RAII 触发）永远不会阻塞。
//!
//! 堆积风险评估：
//! - chat 路径**不会**因清理队列堆积而阻塞（unbounded channel 同步 send）。
//! - 堆积仅影响**清理延迟**：容器物理销毁变慢，但不会导致内存泄漏（containers map
//!   条目在 ProjectAdapter::remove 内已被移除）。
//! - 若业务观察到清理严重延迟（如 idle 容器数持续增长），可考虑改成多消费者并行，
//!   但需要权衡 stop_container_by_identifier 在 runtime 层的并发安全性。

use std::sync::Arc;
use std::time::Duration;

use shared_types::ServiceType;
use tracing::{debug, error, info, warn};

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
    /// K8s namespace（用于构建 K8s Service FQDN）
    pub namespace: String,
    /// K8s 集群域名
    pub cluster_domain: String,
    /// 关联的 project_id 列表（日志用）
    pub project_ids: Vec<String>,
    /// re-enqueue 重试次数（0=首次，上限 MAX_STOP_RETRIES；reaper stop 失败时自增并重新入队）
    pub retry_count: u32,
}

/// 单个清理操作超时时间（防止慢清理阻塞队列）
const CLEANUP_TIMEOUT_SECS: u64 = 120;

/// 后台资源回收器
pub struct ResourceReaper {
    rx: tokio::sync::mpsc::UnboundedReceiver<CleanupRequest>,
    runtime: Arc<dyn ContainerRuntime>,
    grpc_pool: Arc<crate::grpc::GrpcChannelPool>,
    pingora: Option<Arc<rcoder_proxy::PingoraProxyService>>,
    docker_manager: Option<Arc<docker_manager::DockerManager>>,
    /// 是否是 K8s 运行时
    is_kubernetes: bool,
    /// cleanup channel sender：stop 失败时 re-enqueue（有限次重试）
    cleanup_tx: tokio::sync::mpsc::UnboundedSender<CleanupRequest>,
}

impl ResourceReaper {
    pub fn new(
        rx: tokio::sync::mpsc::UnboundedReceiver<CleanupRequest>,
        runtime: Arc<dyn ContainerRuntime>,
        grpc_pool: Arc<crate::grpc::GrpcChannelPool>,
        pingora: Option<Arc<rcoder_proxy::PingoraProxyService>>,
        docker_manager: Option<Arc<docker_manager::DockerManager>>,
        cleanup_tx: tokio::sync::mpsc::UnboundedSender<CleanupRequest>,
    ) -> Self {
        // 判断是否是 K8s 运行时（通过 features flag）
        let is_kubernetes = shared_types::is_kubernetes_runtime();

        Self {
            rx,
            runtime,
            grpc_pool,
            pingora,
            docker_manager,
            is_kubernetes,
            cleanup_tx,
        }
    }

    /// 主循环：持续接收并处理清理请求
    ///
    /// 单个清理操作超时 120s，超时后跳过并告警，防止慢清理阻塞队列。
    pub async fn run(mut self) {
        info!("[REAPER] started");
        while let Some(req) = self.rx.recv().await {
            let identifier = req.identifier.clone();
            match tokio::time::timeout(
                Duration::from_secs(CLEANUP_TIMEOUT_SECS),
                self.process_cleanup(req),
            )
            .await
            {
                Ok(()) => {}
                Err(_) => {
                    warn!(
                        "[REAPER] cleanup timed out after {}s, skipping: {}",
                        CLEANUP_TIMEOUT_SECS, identifier
                    );
                }
            }
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
                const MAX_STOP_RETRIES: u32 = 3;
                if req.retry_count < MAX_STOP_RETRIES {
                    let retry_count = req.retry_count + 1;
                    warn!(
                        "[REAPER] stop failed (attempt {}/{}), re-enqueue after 10s: id={}, err={}",
                        retry_count, MAX_STOP_RETRIES, req.identifier, e
                    );
                    let tx = self.cleanup_tx.clone();
                    let mut next = req.clone();
                    next.retry_count = retry_count;
                    // spawn 延迟发送，不阻塞串行 reaper 主循环
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        if let Err(send_e) = tx.send(next) {
                            warn!("[REAPER] re-enqueue send failed: {}", send_e);
                        }
                    });
                    // re-enqueue：跳过后续 steps 2-5，等 stop 成功那次再完整清
                    return;
                }
                // 重试耗尽：容器孤儿（rcoder 已无跟踪记录），但仍 best-effort 清理非容器资源（steps 2-5）
                error!(
                    "[REAPER] stop failed after {} retries, ORPHANED container: id={}, name={}, err={}",
                    MAX_STOP_RETRIES, req.identifier, req.container_name, e
                );
            }
        }

        // 2. 清理 gRPC 连接
        // K8s 用 Service FQDN，Docker 用容器 IP（统一走 shared_types 分发）；
        // Docker 模式下 container_ip 为空时无法定位连接，跳过清理（K8s 用 FQDN，不受影响）
        if !self.is_kubernetes && req.container_ip.is_empty() {
            debug!("[REAPER] Container IP is empty, skipping gRPC cleanup");
            return;
        }
        let grpc_addr = shared_types::build_grpc_addr(
            &req.container_name,
            &req.container_ip,
            &req.namespace,
            &req.cluster_domain,
        );
        self.grpc_pool.remove(&grpc_addr).await;

        // 3. 清理 DockerManager 缓存（Docker 模式）
        if let Some(ref dm) = self.docker_manager {
            let removed = dm.remove_container_cache(&req.identifier).await;
            if removed.is_some() {
                info!(
                    "[REAPER] cleaned DockerManager cache for {}",
                    req.identifier
                );
            }
        }

        // 4. 清理 Pingora VNC backend（ComputerAgentRunner）
        if req.service_type == ServiceType::ComputerAgentRunner
            && let Some(ref pingora) = self.pingora
        {
            pingora.remove_vnc_backend(&req.identifier);
        }

        // 5. 清理 Pingora Project backend（WebAgentRunner）
        if req.service_type == ServiceType::WebAgentRunner
            && let Some(ref pingora) = self.pingora
        {
            pingora.remove_project_backend(&req.identifier);
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
            namespace: "test-namespace".to_string(),
            cluster_domain: "test.cluster.local".to_string(),
            project_ids: vec!["proj-1".to_string()],
            retry_count: 0,
        };
        let debug_str = format!("{:?}", req);
        assert!(debug_str.contains("user-123"));
    }
}
