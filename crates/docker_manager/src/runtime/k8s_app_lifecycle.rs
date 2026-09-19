//! Userapp Deployment 生命周期(从 k8s_deployment.rs 拆出)。
//!
//! Scale/restart and removal of obsolete port resources. Identity-bound deletion
//! lives in k8s_app_deletion.

#[cfg(feature = "kubernetes")]
use container_runtime_api::{
    ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult, ExposeType,
};
#[cfg(feature = "kubernetes")]
use kube::api::{DeleteParams, Patch, PatchParams};
#[cfg(feature = "kubernetes")]
use tracing::info;

#[cfg(feature = "kubernetes")]
use super::k8s_app_helpers::{
    IDLE_TIMEOUT_ANNOTATION, RECYCLE_ENABLED_ANNOTATION, WAKE_ON_TRAFFIC_ANNOTATION,
};

use super::kubernetes_runtime::KubernetesRuntime;

impl KubernetesRuntime {
    pub(super) async fn patch_captured_policy(
        &self,
        target: &shared_types::UserAppMutationTarget,
        policy: &shared_types::UserAppRuntimePolicy,
    ) -> ContainerRuntimeResult<()> {
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if target.resource.kind != shared_types::AppResourceKind::Deployment
            || target.resource.name != self.app_deployment_name(&target.context.app_id)
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Invalid captured policy target".into(),
            ));
        }
        let mut annotations = serde_json::Map::new();
        if let Some(value) = policy.recycle_enabled {
            annotations.insert(RECYCLE_ENABLED_ANNOTATION.into(), value.to_string().into());
        }
        if let Some(value) = policy.idle_timeout_seconds {
            annotations.insert(IDLE_TIMEOUT_ANNOTATION.into(), value.to_string().into());
        }
        if let Some(value) = policy.wake_on_traffic {
            annotations.insert(WAKE_ON_TRAFFIC_ANNOTATION.into(), value.to_string().into());
        }
        if annotations.is_empty() {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Policy patch is empty".into(),
            ));
        }
        self.patch_captured_app(
            &target.resource,
            serde_json::json!({"metadata":{"annotations":annotations}}),
        )
        .await
    }

    pub(super) async fn capture_stop_target(
        &self,
        context: &shared_types::UserAppExecutionContext,
        expected_version: Option<&str>,
    ) -> ContainerRuntimeResult<shared_types::UserAppMutationTarget> {
        let resource = self
            .capture_owned_app_identity(context, expected_version)
            .await?;
        Ok(shared_types::UserAppMutationTarget {
            context: context.clone(),
            resource,
        })
    }

    /// Capture lifecycle ownership once, before staging any replacement config.
    pub(super) async fn capture_owned_app_identity(
        &self,
        context: &shared_types::UserAppExecutionContext,
        expected_version: Option<&str>,
    ) -> ContainerRuntimeResult<shared_types::AppResourceIdentity> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let name = self.app_deployment_name(&context.app_id);
        let deployment = self.deployments_api().get(&name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Read application mutation target: {error}"))
        })?;
        let expected_labels = self.build_app_labels(&context.app_id, None, None);
        if !deployment.metadata.labels.as_ref().is_some_and(|labels| {
            expected_labels
                .iter()
                .all(|(key, value)| labels.get(key) == Some(value))
        }) {
            return Err(ContainerRuntimeError::Conflict(
                "Application mutation target ownership changed".into(),
            ));
        }
        let annotations = deployment.metadata.annotations.as_ref().ok_or_else(|| {
            ContainerRuntimeError::Conflict(
                "Application mutation target requires lifecycle adoption".into(),
            )
        })?;
        context
            .validate_application_metadata(annotations)
            .map_err(ContainerRuntimeError::Conflict)?;
        let resource = app_mutation_identity(name, &deployment.metadata)?;
        if expected_version
            .is_some_and(|version| resource.resource_version.as_deref() != Some(version))
        {
            return Err(ContainerRuntimeError::Conflict(
                "Application mutation target version changed".into(),
            ));
        }
        Ok(resource)
    }

    pub(super) async fn restart_captured_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if target.resource.kind != shared_types::AppResourceKind::Deployment
            || target.resource.name != self.app_deployment_name(&target.context.app_id)
            || target.resource.uid.is_empty()
            || target
                .resource
                .resource_version
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Invalid captured application restart target".into(),
            ));
        }
        self.claim_app_storage_with_context(&target.context.app_id, Some(&target.context))
            .await?;
        self.patch_captured_app(&target.resource, serde_json::json!({
            "metadata":{"annotations":{(WAKE_ON_TRAFFIC_ANNOTATION):"true"}},
            "spec":{"replicas":1,"template":{"metadata":{"annotations":{"rcoder.io/restart-operation":target.context.operation_id}}}}
        })).await
    }

    pub(super) async fn start_captured_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        self.start_captured_with_policy(target, true).await
    }

    pub(super) async fn start_captured_management_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        self.start_captured_with_policy(target, false).await
    }

    async fn start_captured_with_policy(
        &self,
        target: &shared_types::UserAppMutationTarget,
        enable_traffic_wake: bool,
    ) -> ContainerRuntimeResult<()> {
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if target.resource.kind != shared_types::AppResourceKind::Deployment
            || target.resource.name != self.app_deployment_name(&target.context.app_id)
            || target.resource.uid.is_empty()
            || target
                .resource
                .resource_version
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Invalid captured application start target".into(),
            ));
        }
        self.claim_app_storage_with_context(&target.context.app_id, Some(&target.context))
            .await?;
        let patch = if enable_traffic_wake {
            serde_json::json!({
                "metadata":{"annotations":{(WAKE_ON_TRAFFIC_ANNOTATION):"true"}},
                "spec":{"replicas":1}
            })
        } else {
            serde_json::json!({"spec":{"replicas":1}})
        };
        self.patch_captured_app(&target.resource, patch).await
    }

    pub(super) async fn stop_captured_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
        wake_on_traffic: bool,
    ) -> ContainerRuntimeResult<()> {
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if target.resource.name != self.app_deployment_name(&target.context.app_id) {
            return Err(ContainerRuntimeError::Conflict(
                "Application stop target name changed".into(),
            ));
        }
        self.patch_captured_app(&target.resource, serde_json::json!({
            "metadata":{"annotations":{(WAKE_ON_TRAFFIC_ANNOTATION):wake_on_traffic.to_string()}},
            "spec":{"replicas":0}
        })).await
    }

    /// scale Deployment replicas
    pub async fn scale_app(&self, app_id: &str, replicas: i32) -> ContainerRuntimeResult<()> {
        if replicas < 0 {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Replica count must be nonnegative".into(),
            ));
        }
        let identity = self.capture_app_mutation_identity(app_id).await?;
        if replicas > 0 {
            self.claim_app_storage(app_id).await?;
        }
        let name = &identity.name;
        let patch = serde_json::json!({ "spec": { "replicas": replicas } });
        self.patch_captured_app(&identity, patch).await?;
        info!("[K8S-APP] Deployment {name} scaled to {replicas}");
        Ok(())
    }

    /// patch Deployment 的闲置回收策略注解(strategic merge:只改指定注解键,不碰 pod template → 不触发 rollout)。
    /// 字段 None=不改该键;两者皆 None 由上层 service 校验拒绝(此处防御性早返 Ok)。
    pub async fn patch_app_recycle_policy(
        &self,
        app_id: &str,
        recycle_enabled: Option<bool>,
        idle_timeout_seconds: Option<u64>,
    ) -> ContainerRuntimeResult<()> {
        let name = self.app_deployment_name(app_id);
        let mut ann = serde_json::Map::new();
        if let Some(b) = recycle_enabled {
            ann.insert(
                RECYCLE_ENABLED_ANNOTATION.to_string(),
                serde_json::Value::from(b.to_string()),
            );
        }
        if let Some(s) = idle_timeout_seconds {
            ann.insert(
                IDLE_TIMEOUT_ANNOTATION.to_string(),
                serde_json::Value::from(s.to_string()),
            );
        }
        if ann.is_empty() {
            return Ok(()); // 防御:两字段皆 None(service 层已校验)
        }
        let identity = self.capture_app_mutation_identity(app_id).await?;
        let patch = serde_json::json!({ "metadata": { "annotations": ann } });
        self.patch_captured_app(&identity, patch).await?;
        info!(
            "[K8S-APP] Deployment {name} recycle policy patched (enabled={:?}, idle_timeout={:?})",
            recycle_enabled, idle_timeout_seconds
        );
        Ok(())
    }

    pub async fn patch_app_wake_on_traffic(
        &self,
        app_id: &str,
        enabled: bool,
    ) -> ContainerRuntimeResult<()> {
        let identity = self.capture_app_mutation_identity(app_id).await?;
        let annotations =
            std::collections::BTreeMap::from([(WAKE_ON_TRAFFIC_ANNOTATION, enabled.to_string())]);
        let patch = serde_json::json!({ "metadata": { "annotations": annotations } });
        self.patch_captured_app(&identity, patch).await?;
        Ok(())
    }

    /// 触发滚动重启（rollout annotation）
    pub async fn restart_app(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        let identity = self.capture_app_mutation_identity(app_id).await?;
        self.claim_app_storage(app_id).await?;
        let name = &identity.name;
        let now = chrono::Utc::now().to_rfc3339();
        let patch = serde_json::json!({
            "spec": { "template": { "metadata": { "annotations": {
                "kubectl.kubernetes.io/restartedAt": now
            } } } }
        });
        self.patch_captured_app(&identity, patch).await?;
        info!("[K8S-APP] Deployment {name} restarted");
        Ok(())
    }

    /// Capture the physical target before any storage claim or workload mutation.
    async fn capture_app_mutation_identity(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<shared_types::AppResourceIdentity> {
        let name = self.app_deployment_name(app_id);
        let deployment = self.deployments_api().get(&name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Read application mutation target: {error}"))
        })?;
        let expected = self.build_app_labels(app_id, None, None);
        if !deployment.metadata.labels.as_ref().is_some_and(|labels| {
            expected
                .iter()
                .all(|(key, value)| labels.get(key) == Some(value))
        }) {
            return Err(ContainerRuntimeError::Conflict(
                "Application mutation target ownership changed".into(),
            ));
        }
        app_mutation_identity(name, &deployment.metadata)
    }

    async fn patch_captured_app(
        &self,
        identity: &shared_types::AppResourceIdentity,
        patch: serde_json::Value,
    ) -> ContainerRuntimeResult<()> {
        let patch = condition_app_patch(identity, patch)?;
        self.deployments_api()
            .patch(
                &identity.name,
                &PatchParams::default(),
                &Patch::Merge(patch),
            )
            .await
            .map_err(|error| match &error {
                kube::Error::Api(response) if response.code == 409 => {
                    ContainerRuntimeError::Conflict(format!(
                        "Application mutation precondition failed: {error}"
                    ))
                }
                _ => super::builder_completion::k8s_error(
                    format!("Patch captured application: {error}"),
                    error,
                ),
            })?;
        Ok(())
    }

    /// Remove obsolete port resources after a successful update.
    pub async fn cleanup_orphan_port_resources(
        &self,
        app_id: &str,
        params: &ContainerCreateParams,
    ) -> ContainerRuntimeResult<()> {
        let has_http = params
            .ports
            .as_ref()
            .is_some_and(|ps| ps.iter().any(|p| p.expose_type == ExposeType::Http));
        let has_tcp = params
            .ports
            .as_ref()
            .is_some_and(|ps| ps.iter().any(|p| p.expose_type == ExposeType::Tcp));
        let has_env = params.env.as_ref().is_some_and(|e| !e.is_empty());
        let has_secrets = params.secrets.as_ref().is_some_and(|s| !s.is_empty());
        let dp = DeleteParams::default();
        if !has_http {
            let routes = self.httproute_api();
            self.ignore_404(routes.delete(&self.app_http_route_name(app_id), &dp).await)
                .await?;
        }
        if !has_tcp {
            self.ignore_404(
                self.services_api()
                    .delete(&self.app_nodeport_name(app_id), &dp)
                    .await,
            )
            .await?;
        }
        if !has_env {
            self.ignore_404(
                self.configmaps_api()
                    .delete(&self.app_config_name(app_id), &dp)
                    .await,
            )
            .await?;
        }
        if !has_secrets {
            self.ignore_404(
                self.secrets_api()
                    .delete(&self.app_secret_name(app_id), &dp)
                    .await,
            )
            .await?;
        }
        Ok(())
    }

    /// 等 app Pod 容器全部退出（按 rcoder.io/app-id label 轮询 Pod phase），best-effort：
    /// 容器退出（phase != Running）或 Pod 消失即返回；超时/API 错误仅 warn 不阻塞删除
    /// （app 复用共享 PVC 子目录，残留写入影响可控）。
    /// 仅容忍 404（视为已删除/幂等），其余 K8s 错误透传
    async fn ignore_404<T>(&self, r: Result<T, kube::Error>) -> ContainerRuntimeResult<()> {
        match r {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
            Err(e) => Err(ContainerRuntimeError::K8sError(format!("delete: {e}"))),
        }
    }
}

fn app_mutation_identity(
    name: String,
    metadata: &k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
) -> ContainerRuntimeResult<shared_types::AppResourceIdentity> {
    if metadata.deletion_timestamp.is_some() || metadata.name.as_ref() != Some(&name) {
        return Err(ContainerRuntimeError::Conflict(
            "Application mutation target is deleting or changed".into(),
        ));
    }
    let uid = metadata
        .uid
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "Application mutation target has no UID".into(),
            )
        })?;
    let version = metadata
        .resource_version
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "Application mutation target has no resource version".into(),
            )
        })?;
    Ok(shared_types::AppResourceIdentity {
        kind: shared_types::AppResourceKind::Deployment,
        name,
        uid: uid.into(),
        resource_version: Some(version.into()),
    })
}

fn condition_app_patch(
    identity: &shared_types::AppResourceIdentity,
    mut patch: serde_json::Value,
) -> ContainerRuntimeResult<serde_json::Value> {
    let version = identity
        .resource_version
        .as_deref()
        .filter(|value| !value.is_empty());
    if identity.kind != shared_types::AppResourceKind::Deployment
        || identity.uid.is_empty()
        || identity.name.is_empty()
        || version.is_none()
    {
        return Err(ContainerRuntimeError::ConfigurationError(
            "Incomplete application mutation identity".into(),
        ));
    }
    let object = patch.as_object_mut().ok_or_else(|| {
        ContainerRuntimeError::ConfigurationError("Application patch must be an object".into())
    })?;
    let metadata = object
        .entry("metadata")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "Application patch metadata must be an object".into(),
            )
        })?;
    metadata.insert("uid".into(), identity.uid.clone().into());
    metadata.insert("resourceVersion".into(), serde_json::json!(version));
    Ok(patch)
}

#[cfg(test)]
mod mutation_identity_tests {
    use super::*;
    #[test]
    fn conditional_workload_patch_retains_requested_changes_and_captured_identity() {
        let metadata = k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some("rcoder-app-one".into()),
            uid: Some("original-uid".into()),
            resource_version: Some("17".into()),
            ..Default::default()
        };
        let identity = app_mutation_identity("rcoder-app-one".into(), &metadata).expect("identity");
        for change in [
            serde_json::json!({"spec":{"replicas":0}}),
            serde_json::json!({"metadata":{"annotations":{"rcoder.io/wake-on-traffic":"false"}}}),
            serde_json::json!({"spec":{"template":{"metadata":{"annotations":{"restart":"now"}}}}}),
        ] {
            let patch = condition_app_patch(&identity, change.clone()).expect("patch");
            assert_eq!(patch["metadata"]["uid"], "original-uid");
            assert_eq!(patch["metadata"]["resourceVersion"], "17");
            if let Some(spec) = change.get("spec") {
                assert_eq!(&patch["spec"], spec);
            }
            if let Some(annotations) = change.pointer("/metadata/annotations") {
                assert_eq!(&patch["metadata"]["annotations"], annotations);
            }
        }
        let mut missing = metadata.clone();
        missing.uid = None;
        assert!(app_mutation_identity("rcoder-app-one".into(), &missing).is_err());
        let mut missing = metadata;
        missing.resource_version = None;
        assert!(app_mutation_identity("rcoder-app-one".into(), &missing).is_err());
        assert!(app_mutation_identity("replacement-name".into(), &missing).is_err());
    }
}
