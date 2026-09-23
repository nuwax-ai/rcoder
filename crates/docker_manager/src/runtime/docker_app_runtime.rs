//! Docker 侧 Userapp Deployment 运行时（从 docker_runtime.rs 拆出）。
//!
//! `UserAppDeploymentRuntime` 的 trait 壳：**变更组**（create/patch/scale/
//! recycle/restart/delete）一行委托 docker_app_create.rs 的自有 impl；
//! **观测组**（status/spec/list/logs/exec/stream）在本文件。与 K8s 侧
//! k8s_app_*.rs 文件群对称；工具函数在 docker_runtime.rs（pub(crate) 共享）。

use async_trait::async_trait;
use container_runtime_api::{
    AppPortStatus, ContainerCreateParams, ContainerLogEntry, ContainerRuntimeError,
    ContainerRuntimeResult, ContainerSpecSnapshot, DeploymentStatus, ExposeType,
    UserAppDeploymentRuntime,
};
use shared_types::ContainerBasicInfo;
use std::collections::HashMap;

use super::docker_runtime::DockerRuntime;
use super::docker_runtime::{
    APP_COMMAND_LABEL, APP_PORTS_LABEL, app_deployment_name, docker_cpus_to_quantity,
    docker_memory_to_quantity, extract_container_ip, extract_container_ports, parse_ports_label,
};

/// Docker reports published ports as TCP regardless of their application
/// protocol. The persisted app port label determines which ports use Pingora.
fn merge_http_port_labels(ports: &mut Vec<AppPortStatus>, raw: &str) {
    for port in parse_ports_label(raw) {
        if port.expose_type != ExposeType::Http {
            continue;
        }
        if let Some(existing) = ports.iter_mut().find(|existing| existing.port == port.port) {
            existing.expose_type = ExposeType::Http;
        } else {
            ports.push(AppPortStatus {
                name: format!("http-{}", port.port),
                port: port.port,
                expose_type: ExposeType::Http,
                external_port: None,
            });
        }
    }
}

#[async_trait]
impl UserAppDeploymentRuntime for DockerRuntime {
    async fn cleanup_builder_restart_archive(
        &self,
        template: &shared_types::BuilderRestartTemplate,
    ) -> ContainerRuntimeResult<()> {
        self.remove_builder_restart_archive(template).await
    }

    async fn list_builder_creation_receipt_contexts(
        &self,
    ) -> ContainerRuntimeResult<Vec<shared_types::UserAppExecutionContext>> {
        super::docker_compute_receipt::list_builder_creation_receipt_contexts().await
    }

    async fn cleanup_builder_creation_receipts(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<()> {
        super::docker_compute_receipt::cleanup_builder_creation_receipt_files(context).await
    }

    async fn cleanup_compute_receipt_files(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<()> {
        super::docker_compute_receipt::cleanup_compute_receipt_files(context).await
    }

    async fn capture_app_adoption(
        &self,
        context: &shared_types::UserAppExecutionContext,
        expected_uid: &str,
    ) -> ContainerRuntimeResult<Option<shared_types::AppAdoptionTarget>> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if expected_uid.is_empty() || expected_uid.len() > 128 {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Expected resource UID is required".into(),
            ));
        }
        let name = app_deployment_name(&context.app_id);
        let inspect = match self
            .inner
            .get_docker_client()
            .inspect_container(&name, None)
            .await
        {
            Ok(inspect) => inspect,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => return Ok(None),
            Err(error) => {
                return Err(ContainerRuntimeError::DockerError(format!(
                    "Inspect adoption candidate: {error}"
                )));
            }
        };
        let uid = inspect.id.clone().unwrap_or_default();
        if uid.is_empty() || uid != expected_uid {
            return Ok(None);
        }
        let labels = inspect
            .config
            .as_ref()
            .and_then(|config| config.labels.clone())
            .unwrap_or_default();
        // Family identity: the platform app label or a previous rcoder.io
        // identity label. A foreign container is never adopted; an OLDER
        // lifecycle's values are acceptable — rebinding is the point.
        let family = labels.get("managed-by").map(String::as_str) == Some("rcoder-app-manager")
            || labels.contains_key("rcoder.io/application-id");
        if !family {
            return Ok(None);
        }
        if let Some(application) = labels.get("rcoder.io/application-id")
            && application != &context.app_id
        {
            return Ok(None);
        }
        let volumes = super::docker_builder_restart::bind_witness(&inspect)?;
        Ok(Some(shared_types::AppAdoptionTarget {
            context: context.clone(),
            resource: shared_types::AppResourceIdentity {
                kind: shared_types::AppResourceKind::Container,
                name,
                uid,
                resource_version: None,
            },
            volumes,
        }))
    }

    async fn bind_app_adoption(
        &self,
        _target: &shared_types::AppAdoptionTarget,
    ) -> ContainerRuntimeResult<()> {
        // Docker container labels are immutable after creation. Registration
        // for an adopted application container is the caller's durable store
        // binding; no container is recreated or restarted here.
        Ok(())
    }

    async fn adopted_app_physical_uid(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<Option<String>> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let name = app_deployment_name(&context.app_id);
        match self
            .inner
            .get_docker_client()
            .inspect_container(&name, None)
            .await
        {
            Ok(inspect) => Ok(inspect.id.filter(|id| !id.is_empty())),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(None),
            Err(error) => Err(ContainerRuntimeError::DockerError(format!(
                "Inspect application physical UID: {error}"
            ))),
        }
    }

    async fn capture_bound_app_control(
        &self,
        context: &shared_types::UserAppExecutionContext,
        binding: &shared_types::UserAppResourceBinding,
    ) -> ContainerRuntimeResult<shared_types::UserAppMutationTarget> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        binding
            .validate(context, &binding.physical_uid)
            .map_err(ContainerRuntimeError::Conflict)?;
        let name = app_deployment_name(&context.app_id);
        let inspect = self
            .inner
            .get_docker_client()
            .inspect_container(&name, None)
            .await
            .map_err(|error| {
                ContainerRuntimeError::DockerError(format!(
                    "Inspect bound application target: {error}"
                ))
            })?;
        let uid = inspect.id.clone().unwrap_or_default();
        if uid.is_empty() || uid != binding.physical_uid {
            return Err(ContainerRuntimeError::Conflict(
                "Bound application container was replaced".into(),
            ));
        }
        Ok(shared_types::UserAppMutationTarget {
            context: context.clone(),
            resource: shared_types::AppResourceIdentity {
                kind: shared_types::AppResourceKind::Container,
                name,
                uid,
                resource_version: None,
            },
        })
    }

    async fn verify_recovered_volumes(
        &self,
        _context: &shared_types::UserAppExecutionContext,
        _scope: shared_types::UserAppOperationScope,
        volumes: &[shared_types::AppResourceIdentity],
    ) -> ContainerRuntimeResult<()> {
        Self::verify_recovered_host_volumes(volumes)
    }

    async fn verify_committed_creation(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let name = app_deployment_name(&context.app_id);
        let inspect = match self
            .inner
            .get_docker_client()
            .inspect_container(&name, None)
            .await
        {
            Ok(inspect) => inspect,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => return Ok(None),
            Err(error) => {
                return Err(ContainerRuntimeError::DockerError(format!(
                    "Inspect committed creation: {error}"
                )));
            }
        };
        if !inspect
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref())
            .is_some_and(|labels| {
                let metadata = labels.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                context.validate_operation_metadata(&metadata).is_ok()
            })
        {
            // A foreign or unstamped identity is never adopted here; callers
            // keep their AlreadyExists conflict.
            return Ok(None);
        }
        let preferred = inspect
            .host_config
            .as_ref()
            .and_then(|config| config.network_mode.as_deref());
        let address = extract_container_ip(&inspect, preferred);
        Ok(Some(ContainerBasicInfo {
            container_id: inspect.id.clone().unwrap_or_default(),
            container_name: name.clone(),
            container_ip: address,
            internal_port: 0,
            external_port: 0,
            project_id: context.app_id.clone(),
            status: "Starting".to_string(),
            created_at: chrono::Utc::now(),
            service_url: String::new(),
            workload_uid: None,
        }))
    }

    async fn discover_application_identity(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Option<shared_types::UserAppDiscoveredIdentity>> {
        use shared_types::{ServiceType, UserAppOperationScope};
        shared_types::validate_identifier(app_id, "app_id")
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let builder_name = crate::utils::DockerUtils::generate_container_name(
            ServiceType::UserappBuilder.container_prefix(),
            app_id,
        )
        .map_err(ContainerRuntimeError::ConfigurationError)?;
        let mut found = None;
        for (scope, name) in [
            (UserAppOperationScope::Dev, builder_name),
            (UserAppOperationScope::Prod, app_deployment_name(app_id)),
        ] {
            let info = match self
                .inner
                .get_docker_client()
                .inspect_container(&name, None)
                .await
            {
                Ok(info) => info,
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => continue,
                Err(e) => {
                    return Err(ContainerRuntimeError::DockerError(format!(
                        "Discover application container: {e}"
                    )));
                }
            };
            let uid = info.id.clone().ok_or_else(|| {
                ContainerRuntimeError::Conflict("Discovered container ID missing".into())
            })?;
            let metadata = info
                .config
                .as_ref()
                .and_then(|c| c.labels.clone())
                .unwrap_or_default()
                .into_iter()
                .collect();
            let stopped = info
                .state
                .as_ref()
                .is_some_and(|s| s.running == Some(false) && s.restarting != Some(true));
            let context = super::lifecycle_discovery::include(
                &mut found,
                app_id,
                scope,
                &metadata,
                uid.clone(),
                stopped,
            )?;
            let actual = if scope == UserAppOperationScope::Dev {
                container_runtime_api::AgentContainerRuntime::capture_builder_control(
                    self, &context,
                )
                .await?
                .workload
                .ok_or_else(|| {
                    ContainerRuntimeError::Conflict("Builder disappeared during discovery".into())
                })?
            } else {
                self.capture_stop_target(&context).await?.resource
            };
            if actual.uid != uid {
                return Err(ContainerRuntimeError::Conflict(
                    "Container changed during discovery".into(),
                ));
            }
        }
        Ok(found)
    }

    async fn acquire_app_operation(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Option<Box<dyn shared_types::AppOperationLease>>> {
        self.acquire_application_file_lease(app_id, &shared_types::ServiceType::Userapp)
            .await
            .map(Some)
    }

    async fn acquire_builder_family_operation(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Box<dyn shared_types::AppOperationLease>> {
        self.acquire_builder_lease(app_id).await
    }

    async fn validate_app_operation_receipt(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<bool> {
        self.validate_captured_file_lease(context, receipt).await
    }

    async fn app_operation_receipt_holder_dead(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<bool> {
        // Docker flock 极性与 K8s Lease 相反：validate 的 Ok(true) 是孤儿
        // marker 的 authority 残留而非活跃持有，默认推导会把活锁判成已死、
        // 把孤儿判成存活，必须以 flock 活性覆写。
        self.captured_file_lease_holder_dead(context, receipt).await
    }

    async fn release_app_operation_receipt(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<()> {
        self.release_captured_file_lease(context, receipt).await
    }

    async fn capture_app_deletion(
        &self,
        app_id: &str,
        _expected: Option<&str>,
    ) -> ContainerRuntimeResult<shared_types::AppDeletionSnapshot> {
        let name = app_deployment_name(app_id);
        let mut snapshot = shared_types::AppDeletionSnapshot {
            app_id: app_id.into(),
            operation_id: uuid::Uuid::new_v4().to_string(),
            resources: vec![],
        };
        match self
            .inner
            .get_docker_client()
            .inspect_container(&name, None)
            .await
        {
            Ok(info) => {
                let labels = info
                    .config
                    .as_ref()
                    .and_then(|c| c.labels.as_ref())
                    .ok_or_else(|| {
                        ContainerRuntimeError::Conflict(
                            "application container has no ownership labels".into(),
                        )
                    })?;
                if labels
                    .get(shared_types::USERAPP_DOCKER_APP_ID_LABEL)
                    .map(String::as_str)
                    != Some(app_id)
                    || labels.get("service-type").map(String::as_str)
                        != Some(shared_types::ServiceType::Userapp.to_string().as_str())
                {
                    return Err(ContainerRuntimeError::Conflict(
                        "application container ownership mismatch".into(),
                    ));
                }
                let uid = info.id.filter(|id| !id.is_empty()).ok_or_else(|| {
                    ContainerRuntimeError::DockerError("application container has no ID".into())
                })?;
                snapshot.resources.push(shared_types::AppResourceIdentity {
                    kind: shared_types::AppResourceKind::Container,
                    name,
                    uid,
                    resource_version: None,
                });
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {}
            Err(error) => {
                return Err(ContainerRuntimeError::DockerError(format!(
                    "capture application container: {error}"
                )));
            }
        }
        Ok(snapshot)
    }

    async fn delete_app_snapshot(
        &self,
        snapshot: &shared_types::AppDeletionSnapshot,
    ) -> ContainerRuntimeResult<()> {
        for identity in &snapshot.resources {
            if identity.kind != shared_types::AppResourceKind::Container || identity.uid.is_empty()
            {
                return Err(ContainerRuntimeError::ConfigurationError(
                    "invalid Docker deletion identity".into(),
                ));
            }
            match self
                .inner
                .get_docker_client()
                .remove_container(
                    &identity.uid,
                    Some(bollard::query_parameters::RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await
            {
                Ok(()) => {}
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => {}
                Err(error) => {
                    return Err(ContainerRuntimeError::DockerError(format!(
                        "delete captured container: {error}"
                    )));
                }
            }
            #[cfg(feature = "deploy-host")]
            if shared_types::is_deploy_host() {
                shared_types::published::unregister_if_physical(&identity.name, &identity.uid);
                shared_types::published::unregister_if_physical(&snapshot.app_id, &identity.uid);
            }
            self.inner
                .retire_container_cache(&identity.uid)
                .await
                .map_err(|error| {
                    ContainerRuntimeError::DockerError(format!(
                        "retire deleted container cache: {error}"
                    ))
                })?;
        }
        // A replacement invalidates the old operation's routing/metadata cleanup.
        if !self
            .capture_app_deletion(&snapshot.app_id, None)
            .await?
            .resources
            .is_empty()
        {
            return Err(ContainerRuntimeError::Conflict(
                "application container was replaced during deletion".into(),
            ));
        }
        Ok(())
    }
    async fn create_deployment(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        self.create_deployment_impl(params).await
    }

    async fn patch_deployment(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        self.patch_deployment_impl(params).await
    }

    async fn capture_app_mutation_target(
        &self,
        context: &shared_types::UserAppExecutionContext,
        _expected_resource_version: Option<&str>,
    ) -> ContainerRuntimeResult<shared_types::UserAppMutationTarget> {
        self.capture_stop_target(context).await
    }

    async fn restart_app_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
        image: Option<&str>,
    ) -> ContainerRuntimeResult<()> {
        self.restart_captured_target(target, image).await
    }

    async fn start_app_management_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        self.start_captured_target(target).await
    }

    async fn start_app_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        self.start_captured_target(target).await
    }

    async fn start_app_compute(
        &self,
        target: &shared_types::UserAppComputeStartTarget,
    ) -> ContainerRuntimeResult<()> {
        if let Some(image) = target.restart_image.as_deref() {
            // Scoped divergence: the compute-restart verification and recovery
            // fences bind to the captured physical UID, and a Docker image
            // roll recreates the container. Image rolls on Docker go through
            // restart_app_target / update / redeploy, which carry no UID
            // fence; the compute path restarts the captured container.
            tracing::warn!(
                app_id = %target.target.context.app_id,
                image,
                "Docker compute restart keeps the captured container; image not rolled"
            );
        }
        self.start_captured_target(&target.target).await?;
        if !self.captured_start_is_running(&target.target).await? {
            return Err(ContainerRuntimeError::Conflict(
                "Application start is not yet confirmed".into(),
            ));
        }
        super::docker_compute_receipt::save_app_start(&target.target).await
    }

    async fn reconcile_app_compute_start(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<bool> {
        if !super::docker_compute_receipt::matches_app_start(target).await? {
            return Ok(false);
        }
        self.captured_start_is_running(target).await
    }

    async fn app_compute_absent(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<bool> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        match self
            .inner
            .get_docker_client()
            .inspect_container(&app_deployment_name(&context.app_id), None)
            .await
        {
            Ok(_) => Ok(false),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(true),
            Err(error) => Err(ContainerRuntimeError::DockerError(format!(
                "Observe absent application compute: {error}"
            ))),
        }
    }

    async fn confirm_app_compute_stopped(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if target.resource.kind != shared_types::AppResourceKind::Container
            || target.resource.uid.is_empty()
            || target.resource.name != app_deployment_name(&target.context.app_id)
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Invalid captured stop confirmation target".into(),
            ));
        }

        match self
            .inner
            .get_docker_client()
            .inspect_container(&target.resource.uid, None)
            .await
        {
            Ok(info)
                if info.id.as_deref() == Some(target.resource.uid.as_str())
                    && info.state.as_ref().is_some_and(|s| {
                        s.running == Some(false) && s.restarting != Some(true)
                    }) =>
            {
                Ok(())
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(()),
            Ok(_) => Err(ContainerRuntimeError::Conflict(
                "Captured container has not stopped".into(),
            )),
            Err(e) => Err(ContainerRuntimeError::DockerError(format!(
                "Confirm captured container stopped: {e}"
            ))),
        }
    }

    async fn stop_app_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
        _wake_on_traffic: bool,
    ) -> ContainerRuntimeResult<()> {
        // Docker wake policy is represented by the durable Stop intent and the
        // service activity state; there is no mutable container annotation API.
        self.stop_captured_target(target).await?;
        self.confirm_app_compute_stopped(target).await?;
        super::docker_compute_receipt::save_app_stop(target).await
    }

    async fn reconcile_app_compute_stop(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<bool> {
        if !super::docker_compute_receipt::matches_app_stop(target).await? {
            return Ok(false);
        }
        self.confirm_app_compute_stopped(target).await?;
        Ok(true)
    }

    async fn app_compute_write_acknowledged(
        &self,
        target: &shared_types::UserAppMutationTarget,
        starting: bool,
    ) -> ContainerRuntimeResult<bool> {
        if starting {
            super::docker_compute_receipt::matches_app_start(target).await
        } else {
            super::docker_compute_receipt::matches_app_stop(target).await
        }
    }

    async fn scale_deployment(&self, app_id: &str, replicas: i32) -> ContainerRuntimeResult<()> {
        self.scale_deployment_impl(app_id, replicas).await
    }

    async fn patch_app_policy_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
        _policy: &shared_types::UserAppRuntimePolicy,
    ) -> ContainerRuntimeResult<()> {
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if target.resource.kind != shared_types::AppResourceKind::Container
            || target.resource.uid.is_empty()
            || target.resource.name != app_deployment_name(&target.context.app_id)
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Invalid Docker policy target".into(),
            ));
        }
        // No label rewrite or in-memory policy cache. The lifecycle transaction
        // persists policy for this application generation after this validation.
        Ok(())
    }

    async fn restart_deployment(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        self.restart_deployment_impl(app_id).await
    }

    async fn delete_deployment(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        let snapshot = self.capture_app_deletion(app_id, None).await?;
        self.delete_app_snapshot(&snapshot).await
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
        let mut ports = extract_container_ports(&inspect);
        if let Some(raw) = inspect
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref())
            .and_then(|labels| labels.get(APP_PORTS_LABEL))
        {
            merge_http_port_labels(&mut ports, raw);
        }
        Ok(Some(DeploymentStatus {
            app_id: app_id.to_string(),
            lifecycle_id: inspect
                .config
                .as_ref()
                .and_then(|config| config.labels.as_ref())
                .and_then(|labels| labels.get("rcoder.io/lifecycle-id"))
                .cloned(),
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
            ..Default::default()
        }))
    }

    /// 读 app 当前容器的 desired 快照（update 部分更新回退用，见 trait 注释）。
    /// Docker：command = `Config.cmd`，env = `Config.env`（`K=V` 数组）；
    /// resources 从 inspect HostConfig 换算（NanoCpus→核数、字节→Quantity）。
    /// secrets/health_check 恒 None：Docker create 时 env+secrets **合并**进容器 env
    /// （不可分），而 Docker 无探针概念——env 回退已含 secrets 值，容器行为不丢。
    /// 容器不存在 → 空快照。
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
        let labels = cfg.and_then(|c| c.labels.as_ref());
        // command/ports：从元数据 label 读回（create 时写入，见 create_deployment 内
        // 注释）。label 缺失 = 本版本之前创建的存量容器 → None（部分更新缺省会清空
        // 对应字段，过渡态；重建容器后 label 补齐）。command 不从 Config.cmd 读回：
        // 它无法区分"用户显式设置"与"镜像 CMD 固化"（create 未指定时 Docker 把镜像
        // CMD 写进容器 Config），读回会把旧镜像 CMD 钉死到换镜像后的新容器。
        let command = labels
            .and_then(|l| l.get(APP_COMMAND_LABEL))
            .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
            .filter(|c| !c.is_empty());
        let env = cfg
            .and_then(|c| c.env.clone())
            .map(|envs| {
                envs.into_iter()
                    .filter_map(|kv| {
                        let (k, v) = kv.split_once('=')?;
                        Some((k.to_string(), v.to_string()))
                    })
                    .collect::<HashMap<String, String>>()
            })
            .filter(|m| !m.is_empty());
        // ports：label 编码还原（name 空串、strip_prefix None——Docker 单机模式这两项
        // 无运行时语义；expose_type 精确保留 Http/Tcp 区分）。
        let ports = labels
            .and_then(|l| l.get(APP_PORTS_LABEL).map(String::as_str))
            .map(parse_ports_label)
            .filter(|ps| !ps.is_empty());
        let resources = inspect
            .host_config
            .as_ref()
            .map(|hc| container_runtime_api::AppResourceRequirements {
                cpu: hc.nano_cpus.map(docker_cpus_to_quantity),
                memory: hc.memory.map(docker_memory_to_quantity),
                storage: None,
                ephemeral_storage: None,
            })
            .filter(|r| r.cpu.is_some() || r.memory.is_some());
        Ok(ContainerSpecSnapshot {
            command,
            env,
            secrets: None,
            resources,
            health_check: None,
            ports,
        })
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
            let Some(app_id) = labels
                .get(shared_types::USERAPP_DOCKER_APP_ID_LABEL)
                .cloned()
            else {
                continue;
            };
            let running = s.state == Some(ContainerSummaryStateEnum::RUNNING);
            if s.names.as_ref().is_none_or(|names| {
                !names
                    .iter()
                    .any(|name| name.trim_start_matches('/') == app_deployment_name(&app_id))
            }) {
                tracing::warn!(
                    app_id,
                    "Ignore managed Docker app with unexpected container name"
                );
                continue;
            }
            let mut ports: Vec<AppPortStatus> = s
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
            if let Some(raw) = labels.get(APP_PORTS_LABEL) {
                merge_http_port_labels(&mut ports, raw);
            }
            let pod_ip = if running {
                match client
                    .inspect_container(s.id.as_deref().unwrap_or_default(), None)
                    .await
                {
                    Ok(inspect) => {
                        if inspect.id != s.id {
                            tracing::warn!(
                                app_id,
                                "Docker app physical ID changed during route recovery"
                            );
                            continue;
                        }
                        let ip = extract_container_ip(&inspect, None);
                        (!ip.is_empty()).then_some(ip)
                    }
                    Err(error) => {
                        tracing::warn!(app_id, %error, "Cannot inspect Docker app for route recovery");
                        continue;
                    }
                }
            } else {
                None
            };
            out.push(DeploymentStatus {
                app_id,
                lifecycle_id: labels.get("rcoder.io/lifecycle-id").cloned(),
                replicas: if running { 1 } else { 0 },
                ready_replicas: if running { 1 } else { 0 },
                phase: if running { "Running" } else { "Stopped" }.to_string(),
                message: None,
                pod_ip,
                node: None,
                restart_count: 0,
                started_at: None,
                ports,
                resource_version: None,
                ..Default::default()
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
        execute_container_command(
            self.inner.get_docker_client(),
            &app_deployment_name(app_id),
            command,
        )
        .await
    }

    async fn exec_app_configuration_target(
        &self,
        context: &shared_types::UserAppExecutionContext,
        target: &shared_types::RuntimeConfigurationTarget,
        command: Vec<String>,
    ) -> ContainerRuntimeResult<container_runtime_api::ExecResult> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if target.physical_uid.is_empty() || target.deployment_generation.is_empty() {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Configuration exec requires a complete physical identity".into(),
            ));
        }
        let client = self.inner.get_docker_client();
        let inspected = client
            .inspect_container(&target.physical_uid, None)
            .await
            .map_err(|error| {
                ContainerRuntimeError::ContainerExecError(format!(
                    "Inspect configuration exec target: {error}"
                ))
            })?;
        validate_configuration_exec_target(context, target, &inspected)?;
        // Immutable ID also fences replacement between inspect and create_exec.
        execute_container_command(client, &target.physical_uid, command).await
    }

    async fn capture_app_configuration_target(
        &self,
        context: &shared_types::UserAppExecutionContext,
        generation: &str,
    ) -> ContainerRuntimeResult<shared_types::RuntimeConfigurationTarget> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if generation.is_empty() {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Configuration generation is required".into(),
            ));
        }
        let inspected = self
            .inner
            .get_docker_client()
            .inspect_container(&app_deployment_name(&context.app_id), None)
            .await
            .map_err(|error| {
                ContainerRuntimeError::ContainerExecError(format!(
                    "Capture configuration container: {error}"
                ))
            })?;
        let target = shared_types::RuntimeConfigurationTarget {
            physical_uid: inspected
                .id
                .clone()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    ContainerRuntimeError::Conflict(
                        "Configuration container identity is missing".into(),
                    )
                })?,
            deployment_generation: generation.into(),
        };
        validate_configuration_exec_target(context, &target, &inspected)?;
        if inspected.state.as_ref().and_then(|state| state.running) != Some(true) {
            return Err(ContainerRuntimeError::ManagementNotRunning);
        }
        Ok(target)
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

pub(super) async fn execute_container_command(
    client: &bollard::Docker,
    name: &str,
    command: Vec<String>,
) -> ContainerRuntimeResult<container_runtime_api::ExecResult> {
    use bollard::container::LogOutput;
    use bollard::exec::{CreateExecOptions, StartExecResults};
    use futures_util::StreamExt;

    // 1. create exec(容器不存在 → ContainerNotFound,与 get_deployment_status 404 处理一致)
    let exec = client
        .create_exec(
            name,
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
            } => ContainerRuntimeError::ContainerNotFound(name.to_owned()),
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
    if inspect.running != Some(false) {
        return Err(ContainerRuntimeError::ContainerExecError(
            "Exec has no confirmed stopped state; outcome is unknown".into(),
        ));
    }
    let exit_code = inspect.exit_code.filter(|code| *code >= 0).ok_or_else(|| {
        ContainerRuntimeError::ContainerExecError(
            "Exec has no exit code; outcome is unknown".into(),
        )
    })?;

    Ok(container_runtime_api::ExecResult {
        stdout,
        stderr,
        exit_code,
    })
}

fn validate_configuration_exec_target(
    context: &shared_types::UserAppExecutionContext,
    target: &shared_types::RuntimeConfigurationTarget,
    container: &bollard::models::ContainerInspectResponse,
) -> ContainerRuntimeResult<()> {
    let mismatch =
        || ContainerRuntimeError::Conflict("Configuration exec target identity changed".into());
    if container.id.as_deref() != Some(target.physical_uid.as_str()) {
        return Err(mismatch());
    }
    let config = container.config.as_ref().ok_or_else(mismatch)?;
    let labels = config.labels.as_ref().ok_or_else(mismatch)?;
    if labels.get("managed-by").map(String::as_str) != Some("rcoder-app-manager")
        || labels.get("service-type").map(String::as_str)
            != Some(shared_types::ServiceType::Userapp.to_string().as_str())
        || labels.get(shared_types::USERAPP_DOCKER_APP_ID_LABEL) != Some(&context.app_id)
    {
        return Err(mismatch());
    }
    context
        .validate_application_metadata(
            &labels.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        )
        .map_err(ContainerRuntimeError::Conflict)?;
    let expected = format!(
        "{}={}",
        shared_types::APP_DEPLOY_GENERATION_ID,
        target.deployment_generation
    );
    let generations: Vec<_> = config
        .env
        .iter()
        .flatten()
        .filter(|entry| {
            entry
                .split_once('=')
                .is_some_and(|(key, _)| key == shared_types::APP_DEPLOY_GENERATION_ID)
        })
        .collect();
    if generations.len() != 1 || generations[0] != &expected {
        return Err(mismatch());
    }
    Ok(())
}

#[cfg(test)]
mod configuration_exec_tests {
    use super::*;

    #[test]
    fn http_port_label_overrides_docker_tcp_port_observation() {
        let mut ports = vec![AppPortStatus {
            name: String::new(),
            port: 9080,
            expose_type: ExposeType::Tcp,
            external_port: Some(32080),
        }];
        merge_http_port_labels(&mut ports, "9080:http,60000:http,5432:tcp,9999:unknown");
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].expose_type, ExposeType::Http);
        assert_eq!(ports[0].external_port, Some(32080));
        assert_eq!(ports[1].port, 60000);
        assert_eq!(ports[1].expose_type, ExposeType::Http);
    }

    #[test]
    fn configuration_exec_requires_physical_lifecycle_and_generation_match() {
        let context = shared_types::UserAppExecutionContext {
            app_id: "app1".into(),
            lifecycle_id: "life1".into(),
            operation_id: "operation1".into(),
            executor_id: "executor1".into(),
            request_fingerprint: "a".repeat(64),
        };
        let target = shared_types::RuntimeConfigurationTarget {
            physical_uid: "container1".into(),
            deployment_generation: "generation1".into(),
        };
        let mut labels = context.resource_metadata();
        labels.insert("managed-by".into(), "rcoder-app-manager".into());
        labels.insert(
            "service-type".into(),
            shared_types::ServiceType::Userapp.to_string(),
        );
        labels.insert(
            shared_types::USERAPP_DOCKER_APP_ID_LABEL.into(),
            "app1".into(),
        );
        let fixture = serde_json::json!({
            "Id":"container1", "Config":{"Labels": labels,
            "Env":[format!("{}=generation1",shared_types::APP_DEPLOY_GENERATION_ID)]}
        });
        let inspected = serde_json::from_value(fixture.clone()).unwrap();
        validate_configuration_exec_target(&context, &target, &inspected).unwrap();
        for (pointer, replacement) in [
            ("/Id", serde_json::json!("container2")),
            (
                "/Config/Labels/rcoder.io~1lifecycle-id",
                serde_json::json!("life2"),
            ),
            (
                "/Config/Labels/service-type",
                serde_json::json!(shared_types::ServiceType::UserappBuilder.to_string()),
            ),
            ("/Config/Env", serde_json::json!([])),
            (
                "/Config/Env",
                serde_json::json!([format!(
                    "{}=generation2",
                    shared_types::APP_DEPLOY_GENERATION_ID
                )]),
            ),
            (
                "/Config/Env",
                serde_json::json!([
                    format!("{}=generation1", shared_types::APP_DEPLOY_GENERATION_ID),
                    format!("{}=generation2", shared_types::APP_DEPLOY_GENERATION_ID)
                ]),
            ),
        ] {
            let mut changed = fixture.clone();
            *changed.pointer_mut(pointer).unwrap() = replacement;
            let inspected = serde_json::from_value(changed).unwrap();
            assert!(
                validate_configuration_exec_target(&context, &target, &inspected).is_err(),
                "{pointer}"
            );
        }
    }
}
