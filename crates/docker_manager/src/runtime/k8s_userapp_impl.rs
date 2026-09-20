//! K8s 侧 `UserAppDeploymentRuntime` 委托壳（自 kubernetes_runtime.rs 拆出；
//! 18 个方法一行转调 k8s_app_*.rs 子模块的自有实现）。

#[cfg(feature = "kubernetes")]
use async_trait::async_trait;
#[cfg(feature = "kubernetes")]
use chrono::Utc;
#[cfg(feature = "kubernetes")]
use container_runtime_api::{
    ContainerCreateParams, ContainerLogEntry, ContainerRuntimeError, ContainerRuntimeResult,
    ContainerSpecSnapshot, DeploymentStatus, UserAppDeploymentRuntime,
};
#[cfg(feature = "kubernetes")]
use kube::api::Patch;
#[cfg(feature = "kubernetes")]
use shared_types::{ContainerBasicInfo, ServiceType};
#[cfg(feature = "kubernetes")]
use tracing::info;

#[cfg(feature = "kubernetes")]
use super::kubernetes_runtime::{KubernetesRuntime, read_app_expose_env};

#[cfg(feature = "kubernetes")]
#[async_trait]
impl UserAppDeploymentRuntime for KubernetesRuntime {
    async fn cleanup_builder_restart_archive(
        &self,
        template: &shared_types::BuilderRestartTemplate,
    ) -> ContainerRuntimeResult<()> {
        self.remove_builder_restart_archive(template).await
    }

    async fn verify_recovered_volumes(
        &self,
        context: &shared_types::UserAppExecutionContext,
        scope: shared_types::UserAppOperationScope,
        volumes: &[shared_types::AppResourceIdentity],
    ) -> ContainerRuntimeResult<()> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let api: kube::Api<k8s_openapi::api::core::v1::PersistentVolumeClaim> =
            kube::Api::namespaced(self.client.clone(), &self.namespace);
        for expected in volumes {
            if expected.kind != shared_types::AppResourceKind::PersistentVolumeClaim
                || expected.uid.is_empty()
                || expected.name.is_empty()
            {
                return Err(ContainerRuntimeError::ConfigurationError(
                    "Invalid recovered volume witness".into(),
                ));
            }
            let current = api.get(&expected.name).await.map_err(|error| {
                ContainerRuntimeError::K8sError(format!(
                    "Verify recovered volume {}: {error}",
                    expected.name
                ))
            })?;
            if current.metadata.uid.as_deref() != Some(expected.uid.as_str())
                || current.metadata.deletion_timestamp.is_some()
            {
                return Err(ContainerRuntimeError::Conflict(
                    "Recovered workspace volume was replaced or is deleting".into(),
                ));
            }
            let metadata = current.metadata.annotations.as_ref().ok_or_else(|| {
                ContainerRuntimeError::Conflict("Recovered volume lifecycle missing".into())
            })?;
            context
                .validate_application_metadata(metadata)
                .map_err(ContainerRuntimeError::Conflict)?;
        }
        self.verify_recovered_mounts(context, scope, volumes)
            .await?;
        Ok(())
    }
    async fn discover_application_identity(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Option<shared_types::UserAppDiscoveredIdentity>> {
        use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
        use kube::{Api, api::ListParams};
        shared_types::validate_identifier(app_id, "app_id")
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let mut found = None;
        let builders: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let list = builders
            .list(&ListParams::default().labels(&format!(
                "rcoder.io/identifier={app_id},rcoder.io/service-type={}",
                ServiceType::UserappBuilder
            )))
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("Discover builders: {e}")))?;
        for resource in list.items {
            if resource.metadata.deletion_timestamp.is_some() {
                return Err(ContainerRuntimeError::Conflict(
                    "Existing builder is deleting".into(),
                ));
            }
            let uid = resource
                .metadata
                .uid
                .clone()
                .ok_or_else(|| ContainerRuntimeError::Conflict("Builder UID missing".into()))?;
            let context = super::lifecycle_discovery::include(
                &mut found,
                app_id,
                shared_types::UserAppOperationScope::Dev,
                &resource.metadata.annotations.unwrap_or_default(),
                uid.clone(),
                resource
                    .spec
                    .as_ref()
                    .is_some_and(|spec| spec.replicas == Some(0)),
            )?;
            let spec = resource.spec.as_ref().ok_or_else(|| {
                ContainerRuntimeError::Conflict("Discovered builder spec missing".into())
            })?;
            let pod = spec.template.spec.as_ref().ok_or_else(|| {
                ContainerRuntimeError::Conflict("Discovered builder pod spec missing".into())
            })?;
            let workload_name = resource.metadata.name.as_deref().ok_or_else(|| {
                ContainerRuntimeError::Conflict("Discovered builder name missing".into())
            })?;
            // RCoder builders are single-workspace StatefulSets. Never guess
            // which ordinal owns data for an unexpected multi-replica controller.
            if spec.replicas.is_some_and(|replicas| replicas > 1) {
                return Err(ContainerRuntimeError::Conflict(
                    "Multiple builder ordinals require explicit recovery".into(),
                ));
            }
            let ordinal = spec
                .ordinals
                .as_ref()
                .and_then(|ordinals| ordinals.start)
                .unwrap_or(0);
            let claims = spec
                .volume_claim_templates
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|claim| {
                    claim
                        .metadata
                        .name
                        .as_deref()
                        .filter(|name| !name.is_empty())
                        .map(|name| format!("{name}-{workload_name}-{ordinal}"))
                        .ok_or_else(|| {
                            ContainerRuntimeError::Conflict(
                                "Builder volume template name missing".into(),
                            )
                        })
                })
                .collect::<ContainerRuntimeResult<Vec<_>>>()?;
            let volumes = self
                .discover_volume_identities(&context, pod, claims)
                .await?;
            if let Some(identity) = &mut found {
                identity.dev_volumes = volumes;
            }
            let captured = self.capture_builder_compute(&context).await?;
            if captured.workload.as_ref().map(|r| &r.uid) != Some(&uid) {
                return Err(ContainerRuntimeError::Conflict(
                    "Builder changed during discovery".into(),
                ));
            }
        }
        let deployments: Api<Deployment> = self.deployments_api();
        let selector = self
            .build_app_labels(app_id, None, None)
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        let list = deployments
            .list(&ListParams::default().labels(&selector))
            .await
            .map_err(|e| {
                ContainerRuntimeError::K8sError(format!("Discover production compute: {e}"))
            })?;
        for resource in list.items {
            if resource.metadata.deletion_timestamp.is_some() {
                return Err(ContainerRuntimeError::Conflict(
                    "Production compute is deleting".into(),
                ));
            }
            let uid =
                resource.metadata.uid.clone().ok_or_else(|| {
                    ContainerRuntimeError::Conflict("Production UID missing".into())
                })?;
            let context = super::lifecycle_discovery::include(
                &mut found,
                app_id,
                shared_types::UserAppOperationScope::Prod,
                &resource.metadata.annotations.unwrap_or_default(),
                uid.clone(),
                resource
                    .spec
                    .as_ref()
                    .is_some_and(|spec| spec.replicas == Some(0)),
            )?;
            let pod = resource
                .spec
                .as_ref()
                .and_then(|spec| spec.template.spec.as_ref())
                .ok_or_else(|| {
                    ContainerRuntimeError::Conflict("Discovered production pod spec missing".into())
                })?;
            let volumes = self
                .discover_volume_identities(&context, pod, Vec::new())
                .await?;
            if let Some(identity) = &mut found {
                identity.prod_volumes = volumes;
            }
            if self.capture_owned_app_identity(&context, None).await?.uid != uid {
                return Err(ContainerRuntimeError::Conflict(
                    "Production changed during discovery".into(),
                ));
            }
        }
        Ok(found)
    }

    async fn validate_app_operation_receipt(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<bool> {
        self.validate_captured_application_operation(context, receipt)
            .await
    }

    async fn release_app_operation_receipt(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<()> {
        self.release_captured_application_operation(context, receipt)
            .await
    }

    async fn acquire_app_operation(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Option<Box<dyn shared_types::AppOperationLease>>> {
        self.acquire_application_operation(app_id, &ServiceType::Userapp)
            .await
            .map(Some)
    }

    async fn acquire_builder_family_operation(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Box<dyn shared_types::AppOperationLease>> {
        self.acquire_application_operation(app_id, &ServiceType::UserappBuilder)
            .await
    }

    async fn observe_app_pods(
        &self,
        app_id: &str,
        target: &container_runtime_api::AppDeployTarget,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::PodObservation>> {
        self.observe_app_pods_structured(app_id, target).await
    }
    async fn capture_app_deletion(
        &self,
        app_id: &str,
        expected: Option<&str>,
    ) -> ContainerRuntimeResult<shared_types::AppDeletionSnapshot> {
        self.capture_deletion(app_id, expected).await
    }

    async fn delete_app_snapshot(
        &self,
        snapshot: &shared_types::AppDeletionSnapshot,
    ) -> ContainerRuntimeResult<()> {
        self.delete_captured(snapshot, false).await
    }
    // ===== Deployment 生命周期（Userapp 专用，转调 k8s_deployment.rs 的 inherent 方法）=====

    async fn app_env_snapshot(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<shared_types::AppEnvSnapshot> {
        let deployment = self
            .deployments_api()
            .get(&self.app_deployment_name(app_id))
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("read env deployment: {e}")))?;
        let name = deployment
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .and_then(|s| s.containers.first())
            .and_then(|c| c.env_from.as_ref())
            .and_then(|sources| {
                sources
                    .iter()
                    .find_map(|s| s.config_map_ref.as_ref().map(|r| r.name.clone()))
            })
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError(
                    "app has no env ConfigMap reference".into(),
                )
            })?;
        let cm = self
            .configmaps_api()
            .get(&name)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("read env configmap: {e}")))?;
        Ok(shared_types::AppEnvSnapshot {
            env: cm.data.unwrap_or_default().into_iter().collect(),
            deployment_uid: deployment.metadata.uid,
            deployment_version: deployment.metadata.resource_version,
            resource_name: Some(name),
            resource_version: cm.metadata.resource_version,
        })
    }

    async fn update_env_configmap(
        &self,
        app_id: &str,
        env: &std::collections::HashMap<String, String>,
    ) -> ContainerRuntimeResult<()> {
        let snapshot = self.app_env_snapshot(app_id).await?;
        self.update_env_configmap_if_version(app_id, env, &snapshot)
            .await
    }

    async fn update_env_configmap_if_version(
        &self,
        app_id: &str,
        env: &std::collections::HashMap<String, String>,
        snapshot: &shared_types::AppEnvSnapshot,
    ) -> ContainerRuntimeResult<()> {
        let current = self.app_env_snapshot(app_id).await?;
        if current.deployment_uid != snapshot.deployment_uid
            || current.deployment_version != snapshot.deployment_version
            || current.resource_name != snapshot.resource_name
            || current.resource_version != snapshot.resource_version
        {
            return Err(ContainerRuntimeError::Conflict(
                "hot deployment or env changed concurrently".into(),
            ));
        }
        let name = snapshot.resource_name.as_deref().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError("env resource name missing".into())
        })?;
        let version = snapshot.resource_version.as_deref().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError("env resource version missing".into())
        })?;
        let mut data = serde_json::to_value(env).map_err(|error| {
            ContainerRuntimeError::ConfigurationError(format!("serialize deployment env: {error}"))
        })?;
        // JSON Merge Patch needs explicit nulls to remove keys; otherwise stale
        // platform identity fields survive and the readback cannot converge.
        if let Some(values) = data.as_object_mut() {
            for key in snapshot.env.keys().filter(|key| !env.contains_key(*key)) {
                values.insert(key.clone(), serde_json::Value::Null);
            }
        }
        let patch = serde_json::json!({"metadata": {"resourceVersion": version}, "data": data});
        // The API server enforces this resourceVersion against the actual write.
        self.configmaps_api()
            .patch(
                name,
                &kube::api::PatchParams::default(),
                &Patch::Merge(patch),
            )
            .await
            .map_err(|e| match e {
                kube::Error::Api(response) if response.code == 409 => {
                    ContainerRuntimeError::Conflict(format!(
                        "conditional hot env commit: {}",
                        response.message
                    ))
                }
                other => {
                    ContainerRuntimeError::K8sError(format!("conditional hot env commit: {other}"))
                }
            })?;
        let committed = self.app_env_snapshot(app_id).await?;
        if committed.deployment_uid != snapshot.deployment_uid
            || committed.deployment_version != snapshot.deployment_version
            || committed.resource_name != snapshot.resource_name
            || committed.env != *env
        {
            return Err(ContainerRuntimeError::Conflict(
                "deployment changed during hot env convergence; inspect active operation".into(),
            ));
        }
        Ok(())
    }

    async fn create_deployment(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        // app_id 占据 project_id 字段位
        let app_id = params.project_id.clone().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "create_deployment requires project_id (app_id)".to_string(),
            )
        })?;
        // Gateway 配置：env 注入优先，未注入则用默认（nuwax-gateway / default，匹配部署现状），
        // 避免部署侧未配 env 时静默跳过 HTTPRoute 创建。
        let (gateway_name, gateway_namespace, http_expose) = read_app_expose_env();
        self.create_app_resources(
            &app_id,
            &params,
            gateway_name.as_deref(),
            gateway_namespace.as_deref(),
            http_expose,
        )
        .await?;
        Ok(ContainerBasicInfo {
            container_id: self.app_deployment_name(&app_id),
            container_name: self.app_deployment_name(&app_id),
            container_ip: String::new(),
            internal_port: 0,
            external_port: 0,
            project_id: app_id.clone(),
            status: "Starting".to_string(),
            created_at: Utc::now(),
            // Userapp service_url: 传 app_deployment_name(不含 -svc),由 build_k8s_service_fqdn
            // 追加单层 -svc。【不要】传 app_service_name(已含 -svc) → -svc-svc 双后缀。
            service_url: format!(
                "http://{}",
                shared_types::build_k8s_service_fqdn(
                    &self.app_deployment_name(&app_id),
                    &self.namespace,
                    &self.config.cluster_domain,
                ),
            ),
        })
    }

    async fn patch_deployment(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        let app_id = params
            .project_id
            .as_deref()
            .ok_or_else(|| ContainerRuntimeError::ConfigurationError("missing app_id".into()))?;
        let current = self
            .deployments_api()
            .get(&self.app_deployment_name(app_id))
            .await
            .map_err(|e| {
                ContainerRuntimeError::K8sError(format!("read deployment version: {e}"))
            })?;
        self.patch_deployment_if_version(
            params,
            shared_types::AppMutationPrecondition {
                resource_version: current.metadata.resource_version,
            },
        )
        .await
    }

    async fn patch_deployment_if_version(
        &self,
        params: ContainerCreateParams,
        expected: shared_types::AppMutationPrecondition,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        let version = expected
            .resource_version
            .as_deref()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError(
                    "K8s update requires resource_version".into(),
                )
            })?;
        let app_id = params.project_id.clone().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "patch_deployment requires project_id (app_id)".to_string(),
            )
        })?;
        let (gateway_name, gateway_namespace, http_expose) = read_app_expose_env();
        // SSA re-apply 全部资源（幂等 create-or-update，收敛到新 desired state）
        self.write_app_resources(
            &app_id,
            &params,
            gateway_name.as_deref(),
            gateway_namespace.as_deref(),
            http_expose,
            Some(version),
        )
        .await?;
        // 清理 update 后不再需要的端口/配置资源（HTTPRoute/NodePort/ConfigMap/Secret orphan）
        self.cleanup_orphan_port_resources(&app_id, &params).await?;
        info!("[K8S-APP] Deployment patched for app: {app_id}");
        Ok(ContainerBasicInfo {
            container_id: self.app_deployment_name(&app_id),
            container_name: self.app_deployment_name(&app_id),
            container_ip: String::new(),
            internal_port: 0,
            external_port: 0,
            project_id: app_id.clone(),
            status: "Starting".to_string(),
            created_at: Utc::now(),
            // Userapp service_url: 传 app_deployment_name(不含 -svc),由 build_k8s_service_fqdn
            // 追加单层 -svc。【不要】传 app_service_name(已含 -svc) → -svc-svc 双后缀。
            service_url: format!(
                "http://{}",
                shared_types::build_k8s_service_fqdn(
                    &self.app_deployment_name(&app_id),
                    &self.namespace,
                    &self.config.cluster_domain,
                ),
            ),
        })
    }

    async fn capture_app_mutation_target(
        &self,
        context: &shared_types::UserAppExecutionContext,
        expected_resource_version: Option<&str>,
    ) -> ContainerRuntimeResult<shared_types::UserAppMutationTarget> {
        self.capture_stop_target(context, expected_resource_version)
            .await
    }

    async fn restart_app_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        self.restart_captured_target(target).await
    }

    async fn start_app_management_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        self.start_captured_management_target(target).await
    }

    async fn start_app_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        self.start_captured_target(target).await
    }

    async fn app_compute_absent(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<bool> {
        use k8s_openapi::api::{apps::v1::ReplicaSet, core::v1::Pod};
        use kube::{Api, api::ListParams};
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let deployments = self.deployments_api();
        let name = self.app_deployment_name(&context.app_id);
        if deployments
            .get_opt(&name)
            .await
            .map_err(|error| {
                ContainerRuntimeError::K8sError(format!("Observe absent deployment: {error}"))
            })?
            .is_some()
        {
            return Ok(false);
        }
        let selector = format!(
            "app.kubernetes.io/instance={},app.kubernetes.io/managed-by=rcoder-app-manager",
            context.app_id
        );
        let params = ListParams::default().labels(&selector);
        let replicas: Api<ReplicaSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        // A removed Deployment may leave a ReplicaSet or terminating Pods.
        // Neither condition is a completed stop, even if the endpoint is gone.
        if !replicas
            .list(&params)
            .await
            .map_err(|error| {
                ContainerRuntimeError::K8sError(format!("Observe remaining replica sets: {error}"))
            })?
            .items
            .is_empty()
            || !pods
                .list(&params)
                .await
                .map_err(|error| {
                    ContainerRuntimeError::K8sError(format!(
                        "Observe remaining compute pods: {error}"
                    ))
                })?
                .items
                .is_empty()
        {
            return Ok(false);
        }
        Ok(deployments
            .list(&params)
            .await
            .map_err(|error| {
                ContainerRuntimeError::K8sError(format!("Recheck absent deployment: {error}"))
            })?
            .items
            .is_empty())
    }

    async fn prepare_app_compute_start(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<shared_types::UserAppComputeStartTarget> {
        self.prepare_captured_compute_start(target).await
    }

    async fn prepare_app_compute_start_retry(
        &self,
        target: &shared_types::UserAppComputeStartTarget,
    ) -> ContainerRuntimeResult<Option<shared_types::UserAppComputeStartTarget>> {
        self.prepare_captured_compute_retry(target).await
    }

    async fn fence_app_compute_start(
        &self,
        target: &shared_types::UserAppComputeStartTarget,
    ) -> ContainerRuntimeResult<bool> {
        self.fence_captured_compute_write(target).await
    }

    async fn fence_app_compute_stop(
        &self,
        target: &shared_types::UserAppComputeStartTarget,
    ) -> ContainerRuntimeResult<bool> {
        self.fence_captured_compute_write(target).await
    }

    async fn start_app_compute(
        &self,
        target: &shared_types::UserAppComputeStartTarget,
    ) -> ContainerRuntimeResult<()> {
        self.start_captured_compute(target).await
    }

    async fn reconcile_app_compute_start(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<bool> {
        self.reconcile_captured_compute_start(target).await
    }

    async fn reconcile_app_compute_stop(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<bool> {
        self.reconcile_captured_compute_stop(target).await
    }

    async fn confirm_app_compute_stopped(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        self.confirm_captured_compute_stopped(target).await
    }

    async fn stop_app_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
        wake_on_traffic: bool,
    ) -> ContainerRuntimeResult<()> {
        self.stop_captured_target(target, wake_on_traffic).await
    }

    async fn scale_deployment(&self, app_id: &str, replicas: i32) -> ContainerRuntimeResult<()> {
        self.scale_app(app_id, replicas).await
    }

    async fn patch_recycle_policy(
        &self,
        app_id: &str,
        recycle_enabled: Option<bool>,
        idle_timeout_seconds: Option<u64>,
    ) -> ContainerRuntimeResult<()> {
        self.patch_app_recycle_policy(app_id, recycle_enabled, idle_timeout_seconds)
            .await
    }

    async fn patch_app_policy_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
        policy: &shared_types::UserAppRuntimePolicy,
    ) -> ContainerRuntimeResult<()> {
        self.patch_captured_policy(target, policy).await
    }

    async fn patch_wake_on_traffic(
        &self,
        app_id: &str,
        enabled: bool,
    ) -> ContainerRuntimeResult<()> {
        self.patch_app_wake_on_traffic(app_id, enabled).await
    }

    async fn restart_deployment(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        self.restart_app(app_id).await
    }

    async fn delete_deployment(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        let snapshot = self.capture_deletion(app_id, None).await?;
        self.delete_captured(&snapshot, false).await
    }

    async fn get_deployment_status(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Option<DeploymentStatus>> {
        self.get_app_status(app_id).await
    }

    async fn list_deployments(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
        self.list_app_status().await
    }

    async fn get_app_container_spec(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<ContainerSpecSnapshot> {
        // 委派到 k8s_deployment 的 inherent 实现（Userapp trait 方法统一走「委派→inherent」模式，
        // 与 get_deployment_status→get_app_status 一致）。不委派则会命中 trait 默认实现（空快照）→
        // update 回退失效 → command/env 仍被清空。
        self.read_app_container_spec(app_id).await
    }

    async fn get_app_logs(
        &self,
        app_id: &str,
        tail: u32,
        timestamps: bool,
    ) -> ContainerRuntimeResult<Vec<ContainerLogEntry>> {
        self.app_logs(app_id, tail, timestamps).await
    }

    async fn stream_app_logs(
        &self,
        app_id: &str,
        tail: u32,
    ) -> ContainerRuntimeResult<container_runtime_api::mpsc::Receiver<ContainerLogEntry>> {
        self.stream_app_logs_inner(app_id, tail).await
    }

    async fn get_app_events(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::AppEventInfo>> {
        self.app_events(app_id).await
    }

    async fn get_app_resource_usage(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<container_runtime_api::ResourceUsage> {
        self.app_resource_usage(app_id).await
    }

    async fn get_app_resource_usage_for(
        &self,
        app_id: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<container_runtime_api::ResourceUsage> {
        self.app_resource_usage_for(app_id, service_type).await
    }

    async fn exec(
        &self,
        app_id: &str,
        command: Vec<String>,
    ) -> ContainerRuntimeResult<container_runtime_api::ExecResult> {
        self.app_exec(app_id, command).await
    }

    async fn exec_app_configuration_target(
        &self,
        context: &shared_types::UserAppExecutionContext,
        target: &shared_types::RuntimeConfigurationTarget,
        command: Vec<String>,
    ) -> ContainerRuntimeResult<container_runtime_api::ExecResult> {
        self.app_configuration_exec(context, target, command).await
    }

    async fn capture_app_configuration_target(
        &self,
        context: &shared_types::UserAppExecutionContext,
        generation: &str,
    ) -> ContainerRuntimeResult<shared_types::RuntimeConfigurationTarget> {
        self.capture_configuration_pod(context, generation).await
    }

    async fn validate_app_prerequisites(&self) -> ContainerRuntimeResult<()> {
        // RBAC 探测：list deployments（limit 1）。403 = ClusterRole 缺 apps/deployments 权限。
        // 明确报错指向部署侧 RBAC，避免创建 app 时静默 403。
        use k8s_openapi::api::apps::v1::Deployment;
        use kube::api::{Api, ListParams};
        let deploy_api: Api<Deployment> = Api::namespaced(self.client.clone(), &self.namespace);
        match deploy_api.list(&ListParams::default().limit(1)).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(ae)) if ae.code == 403 => Err(ContainerRuntimeError::ConfigurationError(
                "RBAC 403：rcoder ServiceAccount 缺 apps/deployments 权限，app 管理将无法创建 Deployment。\
                 请在 ClusterRole 补 deployments/httproutes/configmaps/secrets 权限"
                    .to_string(),
            )),
            Err(e) => {
                tracing::warn!(
                    "[K8S-APP] 前置校验 list deployments 失败（非 403，可能 API Server 暂时不可达，跳过）: {}",
                    e
                );
                Ok(())
            }
        }
    }
}
