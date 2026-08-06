//! 容器销毁器
//!
//! 销毁容器并清理相关资源（gRPC 连接池、Pingora VNC 后端）
//!
//! 运行时无关: 物理销毁走 `ContainerRuntime` trait 的 `stop_container_by_identifier`,
//! Docker / K8s 各自实现正确语义 (Docker 整删; K8s 删 Pod+Service 并保留 PVC)。

#![allow(dead_code)]

use anyhow::Result;
use container_runtime_api::ContainerRuntime;
use shared_types::ServiceType;
use std::sync::Arc;
use tracing::{debug, info};

use crate::cleanup_task::strategies::DestroyReason;
use crate::grpc::ShutdownSseFn;

/// 容器销毁器
pub struct ContainerDestroyer {
    /// 容器运行时抽象 (Docker / K8s)。物理销毁走 trait, 不再依赖具体 DockerManager。
    pub runtime: Arc<dyn ContainerRuntime>,
    pub grpc_pool: Arc<crate::grpc::GrpcChannelPool>,
    pub pingora_service: Option<Arc<rcoder_proxy::PingoraProxyService>>,
    /// K8s namespace（用于构建 K8s Service FQDN）
    pub namespace: String,
    /// K8s 集群域名
    pub cluster_domain: String,
    /// 是否是 K8s 运行时
    pub is_kubernetes: bool,
    /// 可选的 SSE 共享流关闭回调（参数为 grpc_addr）。
    /// 容器销毁后按地址关闭前端 SSE 进度流；未注入时保持旧行为（向后兼容）。
    shutdown_sse: Option<ShutdownSseFn>,
}

impl ContainerDestroyer {
    pub fn new(
        runtime: Arc<dyn ContainerRuntime>,
        grpc_pool: Arc<crate::grpc::GrpcChannelPool>,
        pingora_service: Option<Arc<rcoder_proxy::PingoraProxyService>>,
        namespace: String,
        cluster_domain: String,
        is_kubernetes: bool,
    ) -> Self {
        Self {
            runtime,
            grpc_pool,
            pingora_service,
            namespace,
            cluster_domain,
            is_kubernetes,
            shutdown_sse: None,
        }
    }

    /// 注入 SSE 共享流关闭回调（builder 风格，保持 `new` 签名向后兼容）。
    ///
    /// 回调参数为 grpc_addr；销毁流程在 `grpc_pool.remove` 同处调用，
    /// 确保容器死亡后前端 SSE 进度流被主动关闭（幂等，重复调用安全）。
    pub fn with_shutdown_sse(mut self, shutdown_sse: ShutdownSseFn) -> Self {
        self.shutdown_sse = Some(shutdown_sse);
        self
    }

    /// 销毁容器并清理相关资源（带原因）
    ///
    /// 物理销毁走 `ContainerRuntime::stop_container_by_identifier` —— 按 identifier/name
    /// 销毁 (容器 name 稳定, 不受重建影响)。不再 `find_container_realtime` 实时查最新
    /// container_id: trait 内部已封装查找 (Docker: `find_user_container`→`stop_by_id` /
    /// `stop_container(name)`; K8s: `pod_name(identifier)`→删 Pod)。
    ///
    /// # 参数
    /// * `container_name` - 容器名称（稳定不变；用于 gRPC addr 构建与日志）
    /// * `service_type` - 服务类型（用于决定是否清理 VNC 后端）
    /// * `container_identifier` - 容器标识符（project_id 或 user_id；传给 trait stop）
    /// * `reason` - 销毁原因
    /// * `project_id` - 项目 ID（用于清理 project_backends，可选）
    /// * `container_ip` - 容器 IP（来自 state 的 `container_info`，用于 gRPC addr 清理；
    ///   重建后可能陈旧，但 `grpc_pool.remove` 一个陈旧 addr 是无害 no-op, 且 ResourceReaper
    ///   对 gRPC/pingora 清理有冗余兜底 —— best-effort）
    pub async fn destroy_with_reason(
        &self,
        container_name: &str,
        service_type: &ServiceType,
        container_identifier: &str,
        reason: &DestroyReason,
        project_id: Option<&str>,
        container_ip: &str,
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

        // 1. 物理销毁: 走 ContainerRuntime trait
        //    按 identifier 销毁 (name 稳定); 两后端对"容器已不存在"都幂等返回 Ok。
        //    (Docker: ComputerAgentRunner 走 find_user_container→stop_by_id, WebAgentRunner 走
        //     stop_container(name); K8s: pod_name(identifier)→删 Pod+Service, 保留 PVC)
        self.runtime
            .stop_container_by_identifier(container_identifier, service_type)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to stop container: identifier={}, error={}",
                    container_identifier,
                    e
                )
            })?;

        // 2. 清理关联资源 (gRPC 连接池旧连接 + Pingora 后端)
        // 清理 gRPC 连接池中的旧连接（避免复用已失效的 TCP 连接）。
        // Docker 环境 container_ip 为空时跳过(K8s 用 FQDN 不依赖 ip)。
        if !self.is_kubernetes && container_ip.is_empty() {
            debug!(" [destroyer] Container IP is empty, skipping gRPC cleanup");
            return Ok(());
        }
        let grpc_addr = shared_types::build_grpc_addr(
            container_name,
            container_ip,
            &self.namespace,
            &self.cluster_domain,
        );
        self.grpc_pool.remove(&grpc_addr).await;

        // 关闭指向该地址的 SSE 共享流（与 grpc_pool.remove 同源同处；幂等）。
        if let Some(ref shutdown_sse) = self.shutdown_sse {
            shutdown_sse(&grpc_addr);
        }

        if let Some(ref pingora_service) = self.pingora_service {
            if *service_type == ServiceType::ComputerAgentRunner {
                // 清理 Pingora VNC 后端
                let _unused: Option<String> =
                    pingora_service.remove_vnc_backend(container_identifier);
            }

            // 清理 Pingora Project 后端（WebAgentRunner 容器）
            // 使用 project_id 而不是 container_identifier，因为 container_identifier 可能是 pod_id
            if *service_type == ServiceType::WebAgentRunner
                && let Some(pid) = project_id
            {
                let _unused: Option<String> = pingora_service.remove_project_backend(pid);
                debug!(
                    " [destroyer] Cleaned up project_backends: project_id={}",
                    pid
                );
            }
        }

        info!(
            " [destroyer] Container destruction completed: container_name={}, identifier={}, reason={}",
            container_name,
            container_identifier,
            reason.as_str()
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use container_runtime_api::{
        AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeError,
        ContainerRuntimeResult, RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
    };
    use rcoder_proxy::PingoraProxyService;
    use rcoder_proxy::config::ProxyConfig;
    use shared_types::ContainerBasicInfo;
    use std::sync::Mutex;

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

    // === 运行时路由测试 (验证 destroyer 物理销毁走 ContainerRuntime trait) ===

    /// 记录型 ContainerRuntime 桩: 只记录 stop_container_by_identifier 调用,
    /// 其余方法返回空/Ok (满足 trait 形状, 参考 agent_mgmt_forward_test::StubRuntime)。
    struct RecordingRuntime {
        stops: Mutex<Vec<(String, ServiceType)>>,
    }

    #[async_trait]
    impl AgentContainerRuntime for RecordingRuntime {
        async fn create_container(
            &self,
            _params: ContainerCreateParams,
        ) -> ContainerRuntimeResult<ContainerBasicInfo> {
            Err(ContainerRuntimeError::ContainerNotFound("stub".into()))
        }
        async fn get_container_info(
            &self,
            _project_id: &str,
        ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
            Ok(None)
        }
        async fn find_container(
            &self,
            _project_id: &str,
            _service_type: &ServiceType,
        ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
            Ok(None)
        }
        async fn stop_container(&self, _project_id: &str) -> ContainerRuntimeResult<()> {
            Ok(())
        }
        // 覆盖默认实现, 记录按 identifier 的停止调用 (destroyer 现在走这条, 不再 Docker-only)
        async fn stop_container_by_identifier(
            &self,
            identifier: &str,
            service_type: &ServiceType,
        ) -> ContainerRuntimeResult<()> {
            self.stops
                .lock()
                .unwrap()
                .push((identifier.to_string(), service_type.clone()));
            Ok(())
        }
        async fn is_container_running(&self, _project_id: &str) -> ContainerRuntimeResult<bool> {
            Ok(false)
        }
        async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
            Ok(vec![])
        }
        async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
            Ok(())
        }
        async fn health_check(&self) -> ContainerRuntimeResult<()> {
            Ok(())
        }
    }

    // 空 impl 块继承默认实现 → RecordingRuntime impl B+C → 自动 impl ContainerRuntime (super-trait bounds)
    #[async_trait]
    impl WorkspaceRuntime for RecordingRuntime {}
    #[async_trait]
    impl UserAppDeploymentRuntime for RecordingRuntime {}

    #[tokio::test]
    async fn destroyer_routes_stop_through_runtime_trait_and_cleans_vnc() {
        use crate::grpc::GrpcChannelPool;

        let runtime = Arc::new(RecordingRuntime {
            stops: Mutex::new(Vec::new()),
        });
        let grpc_pool = Arc::new(GrpcChannelPool::new());
        let pingora = Arc::new(PingoraProxyService::new(ProxyConfig::default()));
        // 预置一个 VNC 后端, 验证销毁时被清理
        pingora.add_vnc_backend("6", "10.42.1.201");
        assert_eq!(pingora.vnc_backend_count(), 1);

        let destroyer = ContainerDestroyer::new(
            runtime.clone(),
            grpc_pool,
            Some(pingora.clone()),
            "nuwax-k8s-test".to_string(),
            "cluster.local".to_string(),
            false, // is_kubernetes=false + 非空 container_ip → 走完整 gRPC/pingora 清理
        );

        let reason = DestroyReason::IdleTimeout {
            idle_duration_secs: 900,
            timeout_secs: 600,
        };
        destroyer
            .destroy_with_reason(
                "rcoder-computer-agent-runner-6",
                &ServiceType::ComputerAgentRunner,
                "6", // identifier = user_id (ComputerAgentRunner 维度)
                &reason,
                None,
                "10.42.1.201", // container_ip 非空 → 不跳过清理
            )
            .await
            .unwrap();

        // 1. 物理销毁走 trait: stop_container_by_identifier 被调一次, 参数=(user_id, ComputerAgentRunner)
        let stops = runtime.stops.lock().unwrap();
        assert_eq!(stops.len(), 1, "stop_container_by_identifier 应被调一次");
        assert_eq!(stops[0].0, "6", "identifier 应为 user_id");
        assert_eq!(
            stops[0].1,
            ServiceType::ComputerAgentRunner,
            "service_type 应为 ComputerAgentRunner"
        );
        drop(stops);

        // 2. ComputerAgentRunner 销毁后 VNC 后端被清理
        assert_eq!(
            pingora.vnc_backend_count(),
            0,
            "ComputerAgentRunner 销毁后 VNC 后端应被清理"
        );
    }
}
