//! 容器销毁器
//!
//! 仅物理销毁。稳定地址、SSE 和登记由持有 canonical frame 的调用方条件退休。
//!
//! 运行时无关: 物理销毁走 `ContainerRuntime` 的 captured stop 契约,
//! Docker / K8s 各自实现正确语义 (Docker 整删; K8s 删 Pod+Service 并保留 PVC)。

use anyhow::{Context, Result};
use container_runtime_api::ContainerRuntime;
use shared_types::ServiceType;
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::cleanup_task::strategies::DestroyReason;

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
}

impl ContainerDestroyer {
    /// Retire only the captured builder compute. A same-name replacement is
    /// never a deletion target; backend implementations preserve its PVC/data.
    pub async fn destroy_idle_builder_compute(
        &self,
        identifier: &str,
        captured: &shared_types::ContainerBasicInfo,
        reason: &DestroyReason,
    ) -> Result<()> {
        info!(identifier, container_name = %captured.container_name, container_id = %captured.container_id,
            project_id = %captured.project_id, container_ip = %captured.container_ip,
            reason = reason.as_str(), reason_detail = %reason.description(), "Retiring captured idle builder compute");
        self.runtime
            .stop_idle_builder_compute(identifier, captured)
            .await
            .with_context(|| format!("Retire captured builder: identifier={identifier}, container_name={}, container_id={}, project_id={}, container_ip={}, reason={}",
                captured.container_name, captured.container_id, captured.project_id, captured.container_ip, reason.description()))
            .inspect_err(|error| error!(identifier, container_id = %captured.container_id,
                reason = reason.as_str(), error = %format!("{error:#}"), "Captured builder retirement failed"))?;
        // Route/session cleanup belongs to the caller after its canonical
        // registration CAS; this physical receipt cannot authorize clearing a
        // later execution sharing the same Service address.
        info!(identifier, container_id = %captured.container_id, reason = reason.as_str(), "Captured builder compute retired; registration retirement remains caller-owned");
        Ok(())
    }

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
        }
    }

    /// Physical-only compatibility entry. Catalog-aware callers use AppState's
    /// captured stop/CAS path; this receipt never clears stable routes or SSE.
    pub async fn destroy_with_reason(
        &self,
        container_name: &str,
        service_type: &ServiceType,
        container_identifier: &str,
        reason: &DestroyReason,
        project_id: Option<&str>,
        container_ip: &str,
    ) -> Result<()> {
        info!(container_name, %service_type, container_identifier, ?project_id, container_ip,
            reason = reason.as_str(), reason_detail = %reason.description(), "Starting captured container retirement");
        let context = || {
            format!(
                "container_name={container_name}, service_type={service_type}, identifier={container_identifier}, project_id={project_id:?}, container_ip={container_ip}, reason={}",
                reason.description()
            )
        };
        let snapshot = self
            .runtime
            .capture_agent_stop(container_identifier, service_type)
            .await
            .with_context(|| format!("Capture container retirement target: {}", context()))
            .inspect_err(|error| error!(container_name, container_identifier, ?project_id, container_ip,
                reason = reason.as_str(), error = %format!("{error:#}"), "Container retirement capture failed"))?;
        match self.runtime.stop_captured_agent(&snapshot).await
            .with_context(|| format!("Stop captured container: {}", context()))
            .inspect_err(|error| error!(container_name, container_identifier, ?project_id, container_ip,
                reason = reason.as_str(), error = %format!("{error:#}"), "Captured container stop failed"))? {
            container_runtime_api::AgentStopOutcome::Complete => {
                info!(container_name, container_identifier, ?project_id, container_ip,
                    reason = reason.as_str(), "Captured compute and services retired; registration retirement remains caller-owned");
                Ok(())
            },
            container_runtime_api::AgentStopOutcome::ComputeGoneServicesPending { pending } => {
                warn!(container_name, container_identifier, ?project_id, container_ip,
                    reason = reason.as_str(), ?pending, "Compute stopped but captured Service cleanup remains pending");
                anyhow::bail!("Compute stopped but service cleanup remains pending: {}; failures={pending:?}", context())
            }
        }
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
        fail_stop: bool,
    }

    #[async_trait]
    impl AgentContainerRuntime for RecordingRuntime {
        async fn capture_agent_stop(
            &self,
            identifier: &str,
            service_type: &ServiceType,
        ) -> ContainerRuntimeResult<container_runtime_api::AgentStopSnapshot> {
            Ok(container_runtime_api::AgentStopSnapshot {
                app_id_hint: None,
                namespace: Some("fixture-scope".into()),
                identifier: identifier.into(),
                service_type: *service_type,
                controller: None,
                pods: vec![],
                container: None,
                services: vec![],
                replicas: None,
                endpoint: None,
            })
        }
        async fn stop_captured_agent(
            &self,
            snapshot: &container_runtime_api::AgentStopSnapshot,
        ) -> ContainerRuntimeResult<container_runtime_api::AgentStopOutcome> {
            self.stops
                .lock()
                .unwrap()
                .push((snapshot.identifier.clone(), snapshot.service_type));
            if self.fail_stop {
                return Err(ContainerRuntimeError::ContainerStopError(
                    "controlled backend stop failure".into(),
                ));
            }
            Ok(container_runtime_api::AgentStopOutcome::Complete)
        }
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
            panic!(
                "logical name stop must not replace the captured identity: {identifier} {service_type}"
            )
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
    async fn destroyer_uses_captured_stop_and_preserves_routes_until_registration_cas() {
        use crate::grpc::GrpcChannelPool;

        let runtime = Arc::new(RecordingRuntime {
            stops: Mutex::new(Vec::new()),
            fail_stop: false,
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
            1,
            "a physical stop without canonical retirement cannot clear a stable VNC route"
        );
    }

    #[tokio::test]
    async fn failed_stop_preserves_reason_scope_and_original_runtime_cause() {
        let runtime = Arc::new(RecordingRuntime {
            stops: Mutex::new(Vec::new()),
            fail_stop: true,
        });
        let destroyer = ContainerDestroyer::new(
            runtime,
            Arc::new(crate::grpc::GrpcChannelPool::new()),
            None,
            "fixture".into(),
            "cluster.local".into(),
            false,
        );
        let reason = DestroyReason::IdleTimeout {
            idle_duration_secs: 900,
            timeout_secs: 600,
        };
        let error = destroyer
            .destroy_with_reason(
                "captured-name",
                &ServiceType::ComputerAgentRunner,
                "captured-owner",
                &reason,
                Some("captured-project"),
                "192.0.2.1",
            )
            .await
            .unwrap_err();
        let detail = format!("{error:#}");
        for expected in [
            "captured-name",
            "captured-owner",
            "captured-project",
            "192.0.2.1",
            "Idle timeout (idle 900s / timeout 600s)",
            "controlled backend stop failure",
        ] {
            assert!(
                detail.contains(expected),
                "missing context: {expected}: {detail}"
            );
        }
        assert!(
            error.downcast_ref::<ContainerRuntimeError>().is_some(),
            "the original typed cause must survive context"
        );
    }
}
