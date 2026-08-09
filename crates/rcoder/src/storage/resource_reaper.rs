//! RAII 资源回收器
//!
//! 后台异步任务，接收 CleanupRequest 并执行物理容器销毁 + 资源清理。
//! 使用 tokio mpsc channel 与 ProjectAdapter 解耦（try_send 非阻塞、异步处理）。
//!
//! ## m2 文档：并发模型与堆积风险
//!
//! - **单消费者**：单个 tokio task 串行处理清理请求。
//! - **120s 超时**：单个清理操作超时则跳过（防止慢容器 stop 阻塞队列）。
//! - **bounded channel（容量 CLEANUP_CHANNEL_CAPACITY）**：生产者走 `try_send`，
//!   永不阻塞；通道满时丢弃并告警（见 ProjectAdapter 侧日志）。
//! - **重试延迟队列**：stop 失败的请求进入 reaper 内部的 `DelayQueue`（10s 后重试），
//!   不再 spawn 游离 task 回发，避免无界 task 泄漏与通道回环。
//!
//! 堆积风险评估：
//! - chat 路径**不会**因清理队列堆积而阻塞（try_send 立即返回）。
//! - 堆积仅影响**清理延迟**：容器物理销毁变慢，但不会导致内存泄漏（containers map
//!   条目在 ProjectAdapter::remove 内已被移除）。
//! - 若业务观察到清理严重延迟（如 idle 容器数持续增长），可考虑改成多消费者并行，
//!   但需要权衡 stop_container_by_identifier 在 runtime 层的并发安全性。

use std::sync::Arc;
use std::time::Duration;

use shared_types::ServiceType;
use tokio_stream::StreamExt;
use tokio_util::time::DelayQueue;
use tracing::{error, info, warn};

use container_runtime_api::ContainerRuntime;

use crate::grpc::ShutdownSseFn;

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

/// cleanup 通道容量（bounded）。
///
/// 清理请求本身低频, 但突发批量回收 (cleanup_task 批量回收 / userapp 自动回收潮)
/// 会瞬时涌入; 通道满时生产端 try_send 丢弃且无孤儿容器对账兼保险机制 (丢弃 = 物理容器泄漏),
/// 故容量取大值提高突发吸收上限。单条消息仅几个 String, 4096 条内存代价只有几 MB。
/// 注: 真正的吞吐瓶颈在消费端 (单 task 串行 + 120s 超时), 容量只决定突发缓冲上限。
pub const CLEANUP_CHANNEL_CAPACITY: usize = 4096;

/// stop 失败后 re-enqueue 的重试延迟
const RETRY_DELAY_SECS: u64 = 10;

/// 后台资源回收器
pub struct ResourceReaper {
    rx: tokio::sync::mpsc::Receiver<CleanupRequest>,
    runtime: Arc<dyn ContainerRuntime>,
    grpc_pool: Arc<crate::grpc::GrpcChannelPool>,
    pingora: Option<Arc<rcoder_proxy::PingoraProxyService>>,
    docker_manager: Option<Arc<docker_manager::DockerManager>>,
    /// 是否是 K8s 运行时
    is_kubernetes: bool,
    /// SSE 共享流关闭回调（参数为 grpc_addr）：容器销毁后按地址关闭前端进度流
    shutdown_sse: ShutdownSseFn,
    /// stop 失败的重试延迟队列（reaper 内部持有，10s 后重新参与调度）
    retry_queue: DelayQueue<CleanupRequest>,
}

impl ResourceReaper {
    pub fn new(
        rx: tokio::sync::mpsc::Receiver<CleanupRequest>,
        runtime: Arc<dyn ContainerRuntime>,
        grpc_pool: Arc<crate::grpc::GrpcChannelPool>,
        pingora: Option<Arc<rcoder_proxy::PingoraProxyService>>,
        docker_manager: Option<Arc<docker_manager::DockerManager>>,
        shutdown_sse: ShutdownSseFn,
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
            shutdown_sse,
            retry_queue: DelayQueue::new(),
        }
    }

    /// 主循环：持续接收并处理清理请求（含重试延迟队列到期项）
    ///
    /// 单个清理操作超时 120s，超时后跳过并告警，防止慢清理阻塞队列。
    pub async fn run(mut self) {
        info!("[REAPER] started");
        loop {
            let req = tokio::select! {
                biased;
                // 优先消费重试队列，避免到期项饥饿
                Some(expired) = self.retry_queue.next() => expired.into_inner(),
                req = self.rx.recv() => match req {
                    Some(req) => req,
                    // 通道关闭（所有 sender 已 drop）：排空重试队列后退出
                    None => {
                        while let Some(expired) = self.retry_queue.next().await {
                            self.process_with_timeout(expired.into_inner()).await;
                        }
                        break;
                    }
                },
            };
            self.process_with_timeout(req).await;
        }
        info!("[REAPER] shutdown");
    }

    /// 带 120s 超时保护地处理单个清理请求
    async fn process_with_timeout(&mut self, req: CleanupRequest) {
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

    /// 处理单个清理请求
    async fn process_cleanup(&mut self, req: CleanupRequest) {
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
                        "[REAPER] stop failed (attempt {}/{}), re-enqueue after {}s: id={}, err={}",
                        retry_count, MAX_STOP_RETRIES, RETRY_DELAY_SECS, req.identifier, e
                    );
                    let mut next = req.clone();
                    next.retry_count = retry_count;
                    // 进入 reaper 内部延迟队列，到期后由主循环重新调度（不 spawn 游离 task）
                    self.retry_queue.insert_at(
                        next,
                        tokio::time::Instant::now() + Duration::from_secs(RETRY_DELAY_SECS),
                    );
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
        // Docker 模式下 container_ip 为空时无法构造 grpc_addr，跳过 gRPC/SSE 清理
        // （K8s 用 FQDN，不受影响；无法反查容器→addr 映射，只能告警提示可能残留）
        if !self.is_kubernetes && req.container_ip.is_empty() {
            warn!(
                "[REAPER] Container IP is empty, skipping gRPC/SSE cleanup (no grpc_addr available, SSE streams for {} may linger)",
                req.container_name
            );
            return;
        }
        let grpc_addr = shared_types::build_grpc_addr(
            &req.container_name,
            &req.container_ip,
            &req.namespace,
            &req.cluster_domain,
        );
        self.grpc_pool.remove(&grpc_addr).await;

        // 2.1 关闭指向该地址的 SSE 共享流（与 grpc_pool.remove 同源同处；幂等）。
        (self.shutdown_sse)(&grpc_addr);

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
