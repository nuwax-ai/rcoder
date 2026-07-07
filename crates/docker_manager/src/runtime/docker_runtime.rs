//! Docker runtime implementation
//!
//! This module provides `DockerRuntime` that wraps the existing `DockerManager`
//! and implements the `ContainerRuntime` trait.

use async_trait::async_trait;
use container_runtime_api::{
    ContainerCreateParams, ContainerRuntime, ContainerRuntimeError, ContainerRuntimeResult,
    ContainerRuntimeStatus, DeploymentStatus, ExposeType, RemovedContainerInfo,
    RuntimeContainerInfo,
};
use moka::future::Cache;
use shared_types::{ContainerBasicInfo, ServiceType};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::DockerManager;

/// Docker runtime implementation wrapping DockerManager
pub struct DockerRuntime {
    inner: Arc<DockerManager>,
    /// TTL cache for list_containers result (15 seconds)
    list_cache: Cache<(), Vec<RuntimeContainerInfo>>,
}

impl DockerRuntime {
    /// Create a new DockerRuntime wrapping the given DockerManager
    pub fn new(inner: Arc<DockerManager>) -> Self {
        Self {
            inner,
            list_cache: Cache::builder()
                .max_capacity(1)
                .time_to_live(Duration::from_secs(15))
                .build(),
        }
    }
}

#[async_trait]
impl ContainerRuntime for DockerRuntime {
    async fn create_container(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        // start_agent_container 被标记为 deprecated 是因为返回的 container_id 可能过期，
        // 但 ContainerRuntime trait 的调用方应通过 find_container 获取最新信息，
        // 因此在 runtime 适配层使用是安全的。
        #[allow(deprecated)]
        self.inner
            .start_agent_container(params)
            .await
            .map_err(|e| ContainerRuntimeError::ContainerCreationError(e.to_string()))
    }

    async fn get_container_info(
        &self,
        project_id: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.inner
            .get_agent_info(project_id)
            .await
            .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string()))
    }

    async fn get_container_info_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        match service_type {
            ServiceType::WebAgentRunner => self
                .inner
                .get_agent_info(identifier)
                .await
                .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string())),
            // 使用 find_container 实时查询 Docker API 获取 IP，
            // 避免 get_user_container_info → get_agent_info → get_container_info 只查缓存
            // 导致服务重启后缓存丢失返回 None
            ServiceType::ComputerAgentRunner => {
                let result = self.find_container(identifier, service_type).await?;
                Ok(result.map(|pod| ContainerBasicInfo {
                    container_id: pod.container_id,
                    container_name: pod.container_name,
                    container_ip: pod.container_ip.clone(),
                    internal_port: shared_types::GRPC_DEFAULT_PORT,
                    external_port: 0,
                    project_id: identifier.to_string(),
                    status: String::from(pod.status),
                    created_at: pod.created_at,
                    service_url: format!(
                        "http://{}:{}",
                        pod.container_ip,
                        shared_types::GRPC_DEFAULT_PORT
                    ),
                }))
            }
            // UserApp 兜底：UserApp 通常走 create_deployment/get_deployment_status，
            // 此处仅为 trait 穷尽性，端口不固定故 internal_port=0
            ServiceType::UserApp => {
                let result = self.find_container(identifier, service_type).await?;
                Ok(result.map(|pod| ContainerBasicInfo {
                    container_id: pod.container_id,
                    container_name: pod.container_name,
                    container_ip: pod.container_ip.clone(),
                    internal_port: 0,
                    external_port: 0,
                    project_id: identifier.to_string(),
                    status: String::from(pod.status),
                    created_at: pod.created_at,
                    service_url: format!("http://{}", pod.container_ip),
                }))
            }
        }
    }

    async fn find_container(
        &self,
        project_id: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        let result = self
            .inner
            .find_project_container(project_id, service_type)
            .await
            .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string()))?;

        Ok(result.map(|r| RuntimeContainerInfo {
            container_id: r.container_id,
            container_name: r.container_name,
            container_ip: r.container_ip,
            status: match r.status {
                crate::types::ContainerStatus::Running => ContainerRuntimeStatus::Running,
                crate::types::ContainerStatus::Stopped => ContainerRuntimeStatus::Failed,
                crate::types::ContainerStatus::Creating => ContainerRuntimeStatus::Pending,
                crate::types::ContainerStatus::Restarting => ContainerRuntimeStatus::Pending,
                crate::types::ContainerStatus::Paused => {
                    ContainerRuntimeStatus::Unknown("paused".to_string())
                }
                crate::types::ContainerStatus::Dead => ContainerRuntimeStatus::Failed,
                crate::types::ContainerStatus::Removing => ContainerRuntimeStatus::Failed,
                crate::types::ContainerStatus::Exited => ContainerRuntimeStatus::Failed,
                crate::types::ContainerStatus::Unknown(s) => ContainerRuntimeStatus::Unknown(s),
            },
            created_at: r.created_at,
            env_vars: None, // 不填充环境变量（用于快速查找）
        }))
    }

    async fn stop_container(&self, project_id: &str) -> ContainerRuntimeResult<()> {
        self.inner
            .stop_container(project_id)
            .await
            .map_err(|e| ContainerRuntimeError::ContainerStopError(e.to_string()))
    }

    async fn stop_container_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        match service_type {
            // UserApp 的 identifier=app_id，复用 WebAgentRunner 的 stop_container 路径
            ServiceType::WebAgentRunner | ServiceType::UserApp => self
                .inner
                .stop_container(identifier)
                .await
                .map_err(|e| ContainerRuntimeError::ContainerStopError(e.to_string())),
            ServiceType::ComputerAgentRunner => {
                if let Some(container) = self
                    .inner
                    .find_user_container(identifier, service_type)
                    .await
                    .map_err(|e| ContainerRuntimeError::ContainerStopError(e.to_string()))?
                {
                    self.inner
                        .stop_container_by_id(&container.container_id)
                        .await
                        .map_err(|e| ContainerRuntimeError::ContainerStopError(e.to_string()))?;
                }
                Ok(())
            }
        }
    }

    async fn is_container_running(&self, project_id: &str) -> ContainerRuntimeResult<bool> {
        if let Some(info) = self.get_container_info(project_id).await? {
            Ok(info.status == "running")
        } else {
            Ok(false)
        }
    }

    async fn is_container_running_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<bool> {
        Ok(self
            .find_container(identifier, service_type)
            .await?
            .map(|c| c.status == ContainerRuntimeStatus::Running)
            .unwrap_or(false))
    }

    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        // 尝试从缓存获取
        if let Some(cached) = self.list_cache.get(&()).await {
            return Ok(cached);
        }

        // 缓存未命中或过期，fetch 并写入缓存
        let result = self.fetch_containers().await?;
        self.list_cache.insert((), result.clone()).await;
        Ok(result)
    }

    async fn sync_states(&self) -> ContainerRuntimeResult<(u32, Vec<RemovedContainerInfo>)> {
        self.inner
            .sync_all_container_states()
            .await
            .map_err(|e| ContainerRuntimeError::DockerError(e.to_string()))
    }

    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        self.inner
            .cleanup_all_containers()
            .await
            .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string()))
    }

    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        self.inner.get_docker_client().ping().await.map_err(|e| {
            ContainerRuntimeError::ConnectionError(format!("Docker ping failed: {}", e))
        })?;
        Ok(())
    }

    // ===== Deployment 生命周期（UserApp 专用，Docker 语义映射）=====
    // Docker 无 Deployment 概念，用容器 create/stop/start 做等价映射。
    // app 容器加入主网络（与 rcoder 同网络），HTTP 端口由 app_manager 通过
    // Pingora backend 注册（container_ip:port），TCP 端口做 port_bindings（自动分配 host port）。
    async fn create_deployment(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountType, PortBinding};
        use bollard::query_parameters::{CreateContainerOptions, StartContainerOptions};

        let app_id = params.project_id.clone().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "create_deployment requires project_id (app_id)".to_string(),
            )
        })?;
        let image = params.image_override.clone().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "create_deployment requires image_override".to_string(),
            )
        })?;
        let container_name = format!("rcoder-app-{app_id}");

        // env（env + secrets 合并；Docker 模式无 Secret 概念）
        let mut env_map: HashMap<String, String> = HashMap::new();
        if let Some(e) = &params.env {
            env_map.extend(e.clone());
        }
        if let Some(s) = &params.secrets {
            env_map.extend(s.clone());
        }
        let env_vec: Vec<String> = env_map.iter().map(|(k, v)| format!("{k}={v}")).collect();

        // labels（供对账/list 过滤）
        let mut labels: HashMap<String, String> = HashMap::new();
        labels.insert("managed-by".to_string(), "rcoder-app-manager".to_string());
        labels.insert("app-id".to_string(), app_id.clone());
        labels.insert("service-type".to_string(), ServiceType::UserApp.to_string());

        // TCP port_bindings（host_port=None 让 Docker 自动分配）
        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        if let Some(ports) = &params.ports {
            for p in ports.iter().filter(|p| p.expose_type == ExposeType::Tcp) {
                port_bindings.insert(
                    format!("{}/tcp", p.port),
                    Some(vec![PortBinding {
                        host_ip: Some("0.0.0.0".to_string()),
                        host_port: None,
                    }]),
                );
            }
        }

        // workspace bind mount（host_workspace_path → /app）
        let mounts = if !params.host_workspace_path.is_empty() {
            Some(vec![Mount {
                target: Some("/app".to_string()),
                source: Some(params.host_workspace_path.clone()),
                typ: Some(MountType::BIND),
                ..Default::default()
            }])
        } else {
            None
        };

        // 加入主网络（与 rcoder 同网络，Pingora 才能通过 container_ip 访问）
        let network_mode = self.inner.detect_main_network_name().await.ok();

        let host_config = HostConfig {
            mounts,
            port_bindings: if port_bindings.is_empty() {
                None
            } else {
                Some(port_bindings)
            },
            network_mode,
            ..Default::default()
        };

        let config = ContainerCreateBody {
            image: Some(image),
            cmd: params.command.clone(),
            env: if env_vec.is_empty() {
                None
            } else {
                Some(env_vec)
            },
            labels: Some(labels),
            host_config: Some(host_config),
            ..Default::default()
        };

        let client = self.inner.get_docker_client();
        let created = client
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(container_name.clone()),
                    platform: String::new(),
                }),
                config,
            )
            .await
            .map_err(|e| ContainerRuntimeError::ContainerCreationError(e.to_string()))?;
        client
            .start_container(&created.id, None::<StartContainerOptions>)
            .await
            .map_err(|e| ContainerRuntimeError::ContainerStartError(e.to_string()))?;

        let inspect = client
            .inspect_container(&created.id, None)
            .await
            .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string()))?;
        let ip = inspect
            .network_settings
            .as_ref()
            .and_then(|n| n.networks.as_ref())
            .and_then(|nets| nets.values().next())
            .and_then(|e| e.ip_address.clone())
            .unwrap_or_default();

        Ok(ContainerBasicInfo {
            container_id: created.id.clone(),
            container_name,
            container_ip: ip,
            internal_port: 0,
            external_port: 0,
            project_id: app_id,
            status: "Running".to_string(),
            created_at: chrono::Utc::now(),
            service_url: String::new(),
        })
    }

    async fn scale_deployment(&self, app_id: &str, replicas: i32) -> ContainerRuntimeResult<()> {
        use bollard::query_parameters::{StartContainerOptions, StopContainerOptions};
        let name = format!("rcoder-app-{app_id}");
        let client = self.inner.get_docker_client();
        if replicas == 0 {
            client
                .stop_container(
                    &name,
                    Some(StopContainerOptions {
                        t: Some(10),
                        signal: Some(String::new()),
                    }),
                )
                .await
                .map_err(|e| ContainerRuntimeError::ContainerStopError(e.to_string()))?;
        } else {
            client
                .start_container(&name, None::<StartContainerOptions>)
                .await
                .map_err(|e| ContainerRuntimeError::ContainerStartError(e.to_string()))?;
        }
        Ok(())
    }

    async fn restart_deployment(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        use bollard::query_parameters::{StartContainerOptions, StopContainerOptions};
        let name = format!("rcoder-app-{app_id}");
        let client = self.inner.get_docker_client();
        let _ = client
            .stop_container(
                &name,
                Some(StopContainerOptions {
                    t: Some(10),
                    signal: Some(String::new()),
                }),
            )
            .await;
        client
            .start_container(&name, None::<StartContainerOptions>)
            .await
            .map_err(|e| ContainerRuntimeError::ContainerStartError(e.to_string()))?;
        Ok(())
    }

    async fn delete_deployment(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        use bollard::query_parameters::RemoveContainerOptions;
        let name = format!("rcoder-app-{app_id}");
        let client = self.inner.get_docker_client();
        let _ = client
            .remove_container(
                &name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await;
        Ok(())
    }

    async fn get_deployment_status(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Option<DeploymentStatus>> {
        let name = format!("rcoder-app-{app_id}");
        let client = self.inner.get_docker_client();
        let inspect = match client.inspect_container(&name, None).await {
            Ok(i) => i,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                return Ok(None);
            }
            Err(e) => {
                return Err(ContainerRuntimeError::ConnectionError(format!(
                    "inspect: {e}"
                )));
            }
        };
        let running = inspect
            .state
            .as_ref()
            .and_then(|s| s.running)
            .unwrap_or(false);
        let ip = inspect
            .network_settings
            .as_ref()
            .and_then(|n| n.networks.as_ref())
            .and_then(|nets| nets.values().next())
            .and_then(|e| e.ip_address.clone())
            .unwrap_or_default();
        Ok(Some(DeploymentStatus {
            app_id: app_id.to_string(),
            replicas: if running { 1 } else { 0 },
            ready_replicas: if running { 1 } else { 0 },
            phase: if running { "Running" } else { "Stopped" }.to_string(),
            pod_ip: if ip.is_empty() { None } else { Some(ip) },
            node: None,
            restart_count: inspect.restart_count.unwrap_or(0) as u32,
            started_at: inspect.state.and_then(|s| s.started_at),
            ports: vec![],
        }))
    }

    async fn list_deployments(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
        // TODO: Docker 模式对账接口——按 label managed-by=rcoder-app-manager list containers。
        // Docker 模式主要用于开发，对账需求低；MVP 返回空，后续按需补 bollard list_containers 过滤。
        Ok(vec![])
    }
}

impl DockerRuntime {
    /// Fetch containers from Docker API (used as cache loader)
    async fn fetch_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        let containers = self.inner.list_containers().await;
        let mut result = Vec::with_capacity(containers.len());
        for c in containers {
            let container_ip = self
                .inner
                .get_container_connection_info(&c)
                .await
                .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string()))?
                .unwrap_or_default();

            // 构建环境变量映射（包含 project_id 和 service_type）
            let mut env_vars = std::collections::HashMap::new();
            env_vars.insert("PROJECT_ID".to_string(), c.project_id.clone());
            if let Some(ref user_id) = c.user_id {
                env_vars.insert("USER_ID".to_string(), user_id.clone());
            }
            if let Some(ref service_type) = c.service_type {
                env_vars.insert("SERVICE_TYPE".to_string(), service_type.to_string());
            }

            result.push(RuntimeContainerInfo {
                container_id: c.container_id,
                container_name: c.container_name,
                container_ip,
                status: match c.status {
                    crate::types::ContainerStatus::Running => ContainerRuntimeStatus::Running,
                    crate::types::ContainerStatus::Stopped => ContainerRuntimeStatus::Failed,
                    crate::types::ContainerStatus::Creating => ContainerRuntimeStatus::Pending,
                    crate::types::ContainerStatus::Restarting => ContainerRuntimeStatus::Pending,
                    crate::types::ContainerStatus::Paused => {
                        ContainerRuntimeStatus::Unknown("paused".to_string())
                    }
                    crate::types::ContainerStatus::Dead => ContainerRuntimeStatus::Failed,
                    crate::types::ContainerStatus::Removing => ContainerRuntimeStatus::Failed,
                    crate::types::ContainerStatus::Exited => ContainerRuntimeStatus::Failed,
                    crate::types::ContainerStatus::Unknown(s) => ContainerRuntimeStatus::Unknown(s),
                },
                created_at: c.created_at,
                env_vars: Some(env_vars),
            });
        }
        Ok(result)
    }
}
