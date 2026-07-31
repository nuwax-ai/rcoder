//! Docker runtime implementation
//!
//! This module provides `DockerRuntime` that wraps the existing `DockerManager`
//! and implements the `ContainerRuntime` trait.

use async_trait::async_trait;
use container_runtime_api::{
    AgentContainerRuntime, AppPortStatus, ContainerCreateParams, ContainerLogEntry,
    ContainerRuntimeError, ContainerRuntimeResult, ContainerRuntimeStatus, ContainerSpecSnapshot,
    DeploymentStatus, ExposeType, RemovedContainerInfo, RuntimeContainerInfo,
    UserAppDeploymentRuntime, WorkspaceRuntime,
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
impl AgentContainerRuntime for DockerRuntime {
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
            status: map_container_status(&r.status),
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
}

/// Docker 不实现 WorkspaceRuntime 的 `resolve_*` / `list_workspace_identifiers` / `ensure_workspace`
/// (file-server 经 trait upcast 拿到 DockerRuntime 时这些方法命中 trait 默认 Ok(None)/Ok(vec![])/Ok(()),
/// 走 LocalWorkspaceResolver 降级 —— 符合 Docker 模式设计)。
/// **仅 `destroy_app_pvc` 重写** (Docker 模式 destroy = 删 app workspace 目录, 对应 K8s 删 PVC+subvolume).
#[async_trait]
impl WorkspaceRuntime for DockerRuntime {
    async fn destroy_app_pvc(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        // Docker 无 PVC 概念；destroy = 删除 app workspace 目录（对应 K8s 删 PVC + subvolume）。
        // 路径同 service 层 get_container_app_dir 的 Docker 分支：RCODER_WORKSPACE_ROOT/{app_id}
        // （默认 /app/project_workspace/apps，与 AppManagerConfig::get_workspace_root 同源）。
        // 幂等：目录不存在返回 Ok（对应 K8s PVC 404→Ok）。app_id 经 service 层 validate_app_id
        // 校验（DNS-1123，无 .. / 路径穿越），join 安全。
        let ws_root = std::env::var("RCODER_WORKSPACE_ROOT")
            .unwrap_or_else(|_| "/app/project_workspace/apps".to_string());
        let app_dir = std::path::Path::new(&ws_root).join(app_id);
        if app_dir.exists() {
            tokio::fs::remove_dir_all(&app_dir).await.map_err(|e| {
                ContainerRuntimeError::DockerError(format!(
                    "destroy_app_pvc: remove {}: {}",
                    app_dir.display(),
                    e
                ))
            })?;
            tracing::info!("[Docker] app workspace destroyed: {}", app_dir.display());
        } else {
            tracing::info!(
                "[Docker] app workspace not found, destroy no-op (idempotent): {}",
                app_dir.display()
            );
        }
        Ok(())
    }
}

#[async_trait]
impl UserAppDeploymentRuntime for DockerRuntime {
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
        let container_name = app_deployment_name(&app_id);

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
        if let Some(t) = &params.tenant_id {
            labels.insert("tenant".to_string(), t.clone());
        }
        if let Some(s) = &params.space_id {
            labels.insert("space".to_string(), s.clone());
        }

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
        // 同时保留网络名，供 start 后按网卡定位 container_ip（多网卡时避免 values().next() 取错）
        let main_network = self.inner.detect_main_network_name().await.ok();
        let network_mode = main_network.clone();

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
            image: Some(image.clone()),
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
            .map_err(|e| {
                // Fail Fast：打印 bollard 原始错误（含 daemon status_code/message），
                // 避免 service 层 context 吞掉根因（见 service.rs create_app 错误链）
                tracing::error!(
                    "[APP-DOCKER] create_container 失败 name={}, image={}: {e:?}",
                    container_name,
                    image
                );
                ContainerRuntimeError::ContainerCreationError(e.to_string())
            })?;
        client
            .start_container(&created.id, None::<StartContainerOptions>)
            .await
            .map_err(|e| {
                tracing::error!(
                    "[APP-DOCKER] start_container 失败 name={}, id={}: {e:?}",
                    container_name,
                    created.id
                );
                ContainerRuntimeError::ContainerStartError(e.to_string())
            })?;

        // 短轮询等待 container_ip 就绪（容器刚 start，IP 可能尚未分配）。
        // 优先取主网络网卡的 IP，回退任意网卡；最多重试 6 次 × 200ms。
        let preferred = main_network.as_deref();
        let ip = {
            let mut ip = String::new();
            for attempt in 0..6u32 {
                match client.inspect_container(&created.id, None).await {
                    Ok(inspect) => {
                        ip = extract_container_ip(&inspect, preferred);
                        if !ip.is_empty() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[APP-DOCKER] inspect container {} for ip failed (attempt {attempt}): {}",
                            created.id,
                            e
                        );
                    }
                }
                if attempt < 5 {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
            ip
        };

        if ip.is_empty() {
            tracing::warn!(
                "[APP-DOCKER] container {} started but IP not ready after polling; \
                 Pingora/gRPC 注册前应确认可达，否则会掩盖启动故障",
                created.id
            );
        }
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

    /// 更新 UserApp 容器：Docker 不支持 in-place 改 image/env/command，必须重建。
    /// force-remove 旧容器（best-effort，不存在则忽略）后用新 params 走 create_deployment。
    /// 工作空间目录不在 runtime 层（由 service 层管理），重建不丢数据。
    async fn patch_deployment(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        use bollard::query_parameters::RemoveContainerOptions;
        let app_id = params.project_id.clone().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "patch_deployment requires project_id (app_id)".to_string(),
            )
        })?;
        let name = app_deployment_name(&app_id);
        let client = self.inner.get_docker_client();
        // 旧容器 best-effort 强删（image/env/command 变了必须重建；不存在则忽略错误）
        if let Err(e) = client
            .remove_container(
                &name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
        {
            tracing::debug!(
                "[DOCKER] Best-effort remove old container {} failed (may not exist): {}",
                name,
                e
            );
        }
        // 用新 params 重建（复用 create_deployment 全套逻辑：mount/env/labels/ports/start）
        self.create_deployment(params).await
    }

    async fn scale_deployment(&self, app_id: &str, replicas: i32) -> ContainerRuntimeResult<()> {
        use bollard::query_parameters::{StartContainerOptions, StopContainerOptions};
        let name = app_deployment_name(app_id);
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
        let name = app_deployment_name(app_id);
        let client = self.inner.get_docker_client();
        // best-effort: 容器可能已停止，忽略 stop 失败
        if let Err(e) = client
            .stop_container(
                &name,
                Some(StopContainerOptions {
                    t: Some(10),
                    signal: Some(String::new()),
                }),
            )
            .await
        {
            tracing::debug!(
                "[DOCKER] Best-effort stop container {} before restart failed: {}",
                name,
                e
            );
        }
        client
            .start_container(&name, None::<StartContainerOptions>)
            .await
            .map_err(|e| ContainerRuntimeError::ContainerStartError(e.to_string()))?;
        Ok(())
    }

    async fn delete_deployment(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        use bollard::query_parameters::RemoveContainerOptions;
        let name = app_deployment_name(app_id);
        let client = self.inner.get_docker_client();
        // best-effort: 容器可能不存在，忽略删除失败
        if let Err(e) = client
            .remove_container(
                &name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
        {
            tracing::debug!(
                "[DOCKER] Best-effort delete container {} failed (may not exist): {}",
                name,
                e
            );
        }
        Ok(())
    }

    async fn get_deployment_status(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Option<DeploymentStatus>> {
        let name = app_deployment_name(app_id);
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
        let ip = extract_container_ip(&inspect, None);
        // 提前借用 inspect 提取 ports（避免下方 inspect.state 消费后借用冲突）
        let ports = extract_container_ports(&inspect);
        Ok(Some(DeploymentStatus {
            app_id: app_id.to_string(),
            replicas: if running { 1 } else { 0 },
            ready_replicas: if running { 1 } else { 0 },
            phase: if running { "Running" } else { "Stopped" }.to_string(),
            message: None,
            pod_ip: if ip.is_empty() { None } else { Some(ip) },
            node: None,
            restart_count: inspect.restart_count.unwrap_or(0) as u32,
            started_at: inspect.state.as_ref().and_then(|s| s.started_at.clone()),
            ports,
            resource_version: None,
        }))
    }

    /// 读 app 当前容器的 `command`/`env` 快照（update 部分更新回退用，见 trait 注释）。
    /// Docker：command = `Config.cmd`，env = `Config.env`（`K=V` 数组）。容器不存在 → 空快照。
    async fn get_app_container_spec(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<ContainerSpecSnapshot> {
        let name = app_deployment_name(app_id);
        let client = self.inner.get_docker_client();
        let inspect = match client.inspect_container(&name, None).await {
            Ok(i) => i,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => return Ok(ContainerSpecSnapshot::default()),
            Err(e) => {
                return Err(ContainerRuntimeError::ConnectionError(format!(
                    "inspect for container spec: {e}"
                )));
            }
        };
        let cfg = inspect.config.as_ref();
        let command = cfg.and_then(|c| c.cmd.clone());
        let env = cfg
            .and_then(|c| c.env.clone())
            .map(|envs| {
                envs.into_iter()
                    .filter_map(|kv| {
                        let (k, v) = kv.split_once('=')?;
                        Some((k.to_string(), v.to_string()))
                    })
                    .collect::<std::collections::HashMap<String, String>>()
            })
            .filter(|m| !m.is_empty());
        Ok(ContainerSpecSnapshot { command, env })
    }

    async fn list_deployments(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
        // Docker 模式对账：按 label managed-by=rcoder-app-manager list 容器（含 stopped），
        // 从 ContainerSummary 组装 DeploymentStatus。供 /apps/runtime 与 query_storage 的
        // is_orphan 判定（无此实现则 Docker 模式所有 app 被误判 orphan）。
        use bollard::models::ContainerSummaryStateEnum;
        use bollard::query_parameters::ListContainersOptionsBuilder;
        let client = self.inner.get_docker_client();
        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        filters.insert(
            "label".to_string(),
            vec!["managed-by=rcoder-app-manager".to_string()],
        );
        let opts = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&filters)
            .build();
        let summaries = client
            .list_containers(Some(opts))
            .await
            .map_err(|e| ContainerRuntimeError::ConnectionError(format!("list containers: {e}")))?;
        let mut out = Vec::with_capacity(summaries.len());
        for s in summaries {
            let Some(labels) = &s.labels else { continue };
            let Some(app_id) = labels.get("app-id").cloned() else {
                continue;
            };
            let running = s.state == Some(ContainerSummaryStateEnum::RUNNING);
            let ports: Vec<AppPortStatus> = s
                .ports
                .as_ref()
                .map(|ps| {
                    ps.iter()
                        .filter_map(|p| {
                            let ext = p.public_port?;
                            Some(AppPortStatus {
                                name: String::new(),
                                port: p.private_port,
                                expose_type: ExposeType::Tcp,
                                external_port: Some(ext),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.push(DeploymentStatus {
                app_id,
                replicas: if running { 1 } else { 0 },
                ready_replicas: if running { 1 } else { 0 },
                phase: if running { "Running" } else { "Stopped" }.to_string(),
                message: None,
                pod_ip: None,
                node: None,
                restart_count: 0,
                started_at: None,
                ports,
                resource_version: None,
            });
        }
        Ok(out)
    }

    async fn get_app_logs(
        &self,
        app_id: &str,
        tail: u32,
        timestamps: bool,
    ) -> ContainerRuntimeResult<Vec<ContainerLogEntry>> {
        use bollard::container::LogOutput;
        use bollard::query_parameters::LogsOptions;
        use futures_util::StreamExt;

        let name = app_deployment_name(app_id);
        let client = self.inner.get_docker_client();
        let opts = LogsOptions {
            stdout: true,
            stderr: true,
            tail: tail.to_string(),
            timestamps,
            ..Default::default()
        };
        let mut stream = client.logs(&name, Some(opts));
        let mut out: Vec<ContainerLogEntry> = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(log) => {
                    // 按 bollard LogOutput 变体区分 stdout/stderr（StdIn/Console 归 stdout）
                    let stream_name = match &log {
                        LogOutput::StdErr { .. } => "stderr",
                        _ => "stdout",
                    };
                    let bytes = log.into_bytes();
                    let text = String::from_utf8_lossy(&bytes);
                    for line in text.lines() {
                        let (ts, msg) =
                            container_runtime_api::split_log_timestamp(line, timestamps);
                        out.push(ContainerLogEntry {
                            timestamp: ts,
                            stream: stream_name.to_string(),
                            message: msg,
                        });
                    }
                }
                // 容器不存在（已删）→ 空日志，与 get_deployment_status 的 Ok(None) 语义对齐
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => return Ok(vec![]),
                Err(e) => {
                    return Err(ContainerRuntimeError::DockerError(format!("logs: {e}")));
                }
            }
        }
        Ok(out)
    }

    /// 在 app 容器内执行命令(docker exec):create_exec → start_exec(读 LogOutput)→ inspect_exec(exit code)。
    /// 用于数据库管理(reset-password / create-database 跑 psql)等场景。
    async fn exec(
        &self,
        app_id: &str,
        command: Vec<String>,
    ) -> ContainerRuntimeResult<container_runtime_api::ExecResult> {
        use bollard::container::LogOutput;
        use bollard::exec::{CreateExecOptions, StartExecResults};
        use futures_util::StreamExt;

        let name = app_deployment_name(app_id);
        let client = self.inner.get_docker_client();

        // 1. create exec(容器不存在 → ContainerNotFound,与 get_deployment_status 404 处理一致)
        let exec = client
            .create_exec(
                &name,
                CreateExecOptions {
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    cmd: Some(command),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| match e {
                bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                } => ContainerRuntimeError::ContainerNotFound(name.clone()),
                _ => ContainerRuntimeError::ContainerExecError(format!("create_exec: {e}")),
            })?;

        // 2. start exec + 读输出流(LogOutput 分桶 stdout/stderr,同 get_app_logs)
        let mut stdout = String::new();
        let mut stderr = String::new();
        match client
            .start_exec(&exec.id, None)
            .await
            .map_err(|e| ContainerRuntimeError::ContainerExecError(format!("start_exec: {e}")))?
        {
            StartExecResults::Attached { mut output, .. } => {
                while let Some(item) = output.next().await {
                    match item {
                        Ok(LogOutput::StdOut { message }) | Ok(LogOutput::Console { message }) => {
                            stdout.push_str(&String::from_utf8_lossy(&message));
                        }
                        Ok(LogOutput::StdErr { message }) => {
                            stderr.push_str(&String::from_utf8_lossy(&message));
                        }
                        Ok(_) => {}
                        Err(e) => {
                            return Err(ContainerRuntimeError::ContainerExecError(format!(
                                "stream: {e}"
                            )));
                        }
                    }
                }
            }
            StartExecResults::Detached => {
                return Err(ContainerRuntimeError::ContainerExecError(
                    "unexpected Detached".into(),
                ));
            }
        }

        // 3. exit code(stream 结束后 inspect 单独取)
        let inspect = client
            .inspect_exec(&exec.id)
            .await
            .map_err(|e| ContainerRuntimeError::ContainerExecError(format!("inspect_exec: {e}")))?;
        let exit_code = inspect.exit_code.unwrap_or(-1);

        Ok(container_runtime_api::ExecResult {
            stdout,
            stderr,
            exit_code,
        })
    }

    async fn stream_app_logs(
        &self,
        app_id: &str,
        tail: u32,
    ) -> ContainerRuntimeResult<container_runtime_api::mpsc::Receiver<ContainerLogEntry>> {
        use bollard::container::LogOutput;
        use bollard::query_parameters::LogsOptions;
        use futures_util::StreamExt;

        let name = app_deployment_name(app_id);
        let client = self.inner.get_docker_client();
        let app_id = app_id.to_string();
        let timestamps = true;
        let opts = LogsOptions {
            stdout: true,
            stderr: true,
            tail: if tail > 0 {
                tail.to_string()
            } else {
                "all".to_string()
            },
            follow: true,
            timestamps,
            ..Default::default()
        };
        let mut stream = client.logs(&name, Some(opts));
        let (tx, rx) = container_runtime_api::mpsc::channel::<ContainerLogEntry>(64);
        tokio::spawn(async move {
            while let Some(item) = stream.next().await {
                match item {
                    Ok(log) => {
                        let stream_name = match &log {
                            LogOutput::StdErr { .. } => "stderr",
                            _ => "stdout",
                        };
                        let bytes = log.into_bytes();
                        let text = String::from_utf8_lossy(&bytes);
                        for line in text.lines() {
                            let (ts, msg) =
                                container_runtime_api::split_log_timestamp(line, timestamps);
                            let entry = ContainerLogEntry {
                                timestamp: ts,
                                stream: stream_name.to_string(),
                                message: msg,
                            };
                            if tx.send(entry).await.is_err() {
                                return; // 客户端断开，receiver 已 drop
                            }
                        }
                    }
                    Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }) => {
                        tracing::warn!("[DOCKER-APP] log stream 容器不存在: {app_id}");
                        return;
                    }
                    Err(e) => {
                        tracing::warn!("[DOCKER-APP] log stream 读失败 (终止): {e}");
                        return;
                    }
                }
            }
        });
        Ok(rx)
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
                status: map_container_status(&c.status),
                created_at: c.created_at,
                env_vars: Some(env_vars),
            });
        }
        Ok(result)
    }
}

/// UserApp 容器/Deployment 命名（单一来源，与 K8s 侧 `KubernetesRuntime::app_deployment_name` 对称）。
///
/// 前缀取自 `ServiceType::UserApp::container_prefix()`，避免散落硬编码；改前缀只需改一处。
fn app_deployment_name(app_id: &str) -> String {
    format!("{}-{app_id}", ServiceType::UserApp.container_prefix())
}

/// 将内部 `ContainerStatus` 映射为运行时 `ContainerRuntimeStatus`
fn map_container_status(status: &crate::types::ContainerStatus) -> ContainerRuntimeStatus {
    match status {
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
        crate::types::ContainerStatus::Unknown(s) => ContainerRuntimeStatus::Unknown(s.clone()),
    }
}

/// 从容器 inspect 结果提取 IP：优先取 `preferred_network` 网卡，回退任意网卡。
///
/// Docker 容器可能同时连接多个网络（主网络 + 自定义），`networks.values().next()`
/// 会非确定性地取一个。优先按主网络名定位，确保拿到 Pingora backend 应指向的 IP。
fn extract_container_ip(
    inspect: &bollard::models::ContainerInspectResponse,
    preferred_network: Option<&str>,
) -> String {
    let Some(nets) = inspect
        .network_settings
        .as_ref()
        .and_then(|n| n.networks.as_ref())
    else {
        return String::new();
    };
    if let Some(net) = preferred_network
        && let Some(entry) = nets.get(net)
        && let Some(ip) = entry.ip_address.as_ref()
        && !ip.is_empty()
    {
        return ip.clone();
    }
    nets.values()
        .next()
        .and_then(|e| e.ip_address.clone())
        .filter(|ip| !ip.is_empty())
        .unwrap_or_default()
}

/// 从容器 inspect 提取 TCP 端口状态（Docker port_bindings → host_port）。
///
/// Docker 仅对 TCP 端口做 port_bindings（create_deployment 时），HTTP 端口走 Pingora
/// 不做 binding，故此处只还原 TCP；name 用 `tcp-{port}`（Docker 无端口名概念，调用方
/// 按 port 而非 name 匹配 external_port）。
fn extract_container_ports(
    inspect: &bollard::models::ContainerInspectResponse,
) -> Vec<AppPortStatus> {
    let Some(ports_map) = inspect
        .network_settings
        .as_ref()
        .and_then(|n| n.ports.as_ref())
    else {
        return vec![];
    };
    ports_map
        .iter()
        .filter_map(|(key, bindings)| {
            // key 形如 "80/tcp"
            let port: u16 = key.trim_end_matches("/tcp").parse().ok()?;
            let host_port = bindings
                .as_ref()
                .and_then(|b| b.first())
                .and_then(|pb| pb.host_port.as_deref())
                .and_then(|s| s.parse::<u16>().ok())?;
            Some(AppPortStatus {
                name: format!("tcp-{port}"),
                port,
                expose_type: ExposeType::Tcp,
                external_port: Some(host_port),
            })
        })
        .collect()
}
