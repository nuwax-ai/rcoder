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
    // ===== Deployment 生命周期（UserApp 专用，转调 k8s_deployment.rs 的 inherent 方法）=====

    /// 热部署收敛：仅更新 ConfigMap data，**保留原 labels/metadata、不触碰
    /// Deployment**（config-hash 注解不动 → 无 Recreate → 热部署效果保持）。
    async fn update_env_configmap(
        &self,
        app_id: &str,
        env: &std::collections::HashMap<String, String>,
    ) -> ContainerRuntimeResult<()> {
        let name = self.app_config_name(app_id);
        let api = self.configmaps_api();
        // 先读现有对象保 labels（tenant/space 归属标记随 apply 字段所有权走，
        // 重建不带上会被 SSA 抹掉）
        let existing = api
            .get(&name)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("get configmap {name}: {e}")))?;
        let mut cm = existing;
        cm.data = Some(env.clone().into_iter().collect());
        let body = serde_json::to_value(&cm)
            .map_err(|e| ContainerRuntimeError::K8sError(format!("serialize configmap: {e}")))?;
        api.patch(&name, &Self::ssa_patch_params(), &Patch::Apply(body))
            .await
            .map_err(|e| {
                ContainerRuntimeError::K8sError(format!("apply configmap (env-only): {e}"))
            })?;
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
            // UserApp service_url: 传 app_deployment_name(不含 -svc),由 build_k8s_service_fqdn
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
        let app_id = params.project_id.clone().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "patch_deployment requires project_id (app_id)".to_string(),
            )
        })?;
        let (gateway_name, gateway_namespace, http_expose) = read_app_expose_env();
        // SSA re-apply 全部资源（幂等 create-or-update，收敛到新 desired state）
        self.create_app_resources(
            &app_id,
            &params,
            gateway_name.as_deref(),
            gateway_namespace.as_deref(),
            http_expose,
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
            // UserApp service_url: 传 app_deployment_name(不含 -svc),由 build_k8s_service_fqdn
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
        self.delete_app_resources(app_id).await
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
        // 委派到 k8s_deployment 的 inherent 实现（UserApp trait 方法统一走「委派→inherent」模式，
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
