//! UserApp K8s 资源 **共享 helper**(命名/label/API accessor/const)。
//! 从原 k8s_deployment.rs(1415行)拆分后,本文件仅保留跨 create/lifecycle/query 共用的:
//! - 模块级 const:`APP_MANAGED_BY`/`APP_LABEL_PREFIX`/`RCODER_LABEL_PREFIX`/`APP_CONTAINER_NAME`
//! - 命名:`app_deployment_name`/`app_config_name`/.../`app_workspace_pvc_name`
//! - label 构建:`build_app_labels`
//! - API accessor:`pods_api`/`deployments_api`/`services_api`/`configmaps_api`/`secrets_api`
//! - SSA 参数:`ssa_patch_params`
//!
//! 实际生命周期方法见 `k8s_app_create`(创建)/`k8s_app_lifecycle`(stop/restart/cleanup)/
//! `k8s_app_query`(状态查询)/`k8s_app_helpers`(编解码/probe)。
//! UserApp 存储走 per-app CephFS subvolume PVC(`ensure_workspace_pvc(app_id, UserApp, ...)`)。

#[cfg(feature = "kubernetes")]
use container_runtime_api::ContainerRuntimeResult;
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::apps::v1::Deployment;
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::{
    ConfigMap, Service,
};
#[cfg(feature = "kubernetes")]
use kube::api::{Api, PatchParams};
#[cfg(feature = "kubernetes")]
use std::collections::BTreeMap;

#[cfg(feature = "kubernetes")]
use shared_types::ServiceType;

#[cfg(feature = "kubernetes")]
use super::k8s_pvc::K8sPvcOps;
use super::kubernetes_runtime::KubernetesRuntime;

#[cfg(feature = "kubernetes")]
pub(crate) const APP_MANAGED_BY: &str = "rcoder-app-manager";
#[cfg(feature = "kubernetes")]
pub(crate) const APP_LABEL_PREFIX: &str = "app.kubernetes.io";
#[cfg(feature = "kubernetes")]
pub(crate) const RCODER_LABEL_PREFIX: &str = "rcoder.io";
/// UserApp Pod 主容器名：build_app_deployment 创建、deployment_to_status 按此名定位状态
#[cfg(feature = "kubernetes")]
pub(crate) const APP_CONTAINER_NAME: &str = "app";

/// UserApp K8s 资源命名 + 创建/伸缩/重启/删除/查询（pub(crate)，由 ContainerRuntime
/// trait 的 Deployment 方法转调，rcoder 通过 trait 调用）。
#[cfg(feature = "kubernetes")]
impl KubernetesRuntime {

    /// app_id → Deployment 名（其余 app 资源名基于此）
    ///
    /// 前缀取自 `ServiceType::UserApp::container_prefix()`（单一来源），与 Docker 侧
    /// `docker_runtime::app_deployment_name` 对称，避免硬编码散落；改前缀只需改一处。
    pub fn app_deployment_name(&self, app_id: &str) -> String {
        format!("{}-{app_id}", ServiceType::UserApp.container_prefix())
    }


    pub(crate) fn app_config_name(&self, app_id: &str) -> String {
        format!("{}-config", self.app_deployment_name(app_id))
    }


    pub(crate) fn app_secret_name(&self, app_id: &str) -> String {
        format!("{}-secret", self.app_deployment_name(app_id))
    }


    /// app ClusterIP Service 名（供 HTTPRoute backendRef / 集群内访问）
    pub fn app_service_name(&self, app_id: &str) -> String {
        format!("{}-svc", self.app_deployment_name(app_id))
    }


    pub(crate) fn app_http_route_name(&self, app_id: &str) -> String {
        format!("{}-route", self.app_deployment_name(app_id))
    }


    pub(crate) fn app_nodeport_name(&self, app_id: &str) -> String {
        format!("{}-nodeport", self.app_deployment_name(app_id))
    }


    /// app workspace PVC 名（阶段2 per-app PVC, 复用 K8sPvcOps::workspace_pvc_name 单一事实源,
    /// 与 agent 路径 create_container + resolve_subvolume_path 同名, 根除漂移）
    pub(crate) fn app_workspace_pvc_name(&self, app_id: &str) -> ContainerRuntimeResult<String> {
        self.workspace_pvc_name(app_id, &ServiceType::UserApp)
    }


    /// 构建 app 专用 label（与 agent 物理隔离）。
    ///
    /// `tenant_id`/`space_id` 作为可选标签加入（供 label 过滤/对账）。
    /// **注意**：Deployment/Service 的 `.spec.selector` 必须用稳定 core（不含 tenant/space），
    /// 因为 selector 创建后不可变；tenant/space 若变更会导致 SSA apply 冲突。故 selector 一律
    /// 调 `build_app_labels(app_id, None, None)` 取 core，metadata/template 才用 full。
    pub(crate) fn build_app_labels(
        &self,
        app_id: &str,
        tenant_id: Option<&str>,
        space_id: Option<&str>,
    ) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::new();
        labels.insert(format!("{}/name", APP_LABEL_PREFIX), "user-app".to_string());
        labels.insert(format!("{}/instance", APP_LABEL_PREFIX), app_id.to_string());
        labels.insert(
            format!("{}/managed-by", APP_LABEL_PREFIX),
            APP_MANAGED_BY.to_string(),
        );
        labels.insert(
            format!("{}/part-of", APP_LABEL_PREFIX),
            "rcoder".to_string(),
        );
        labels.insert(
            format!("{}/app-id", RCODER_LABEL_PREFIX),
            app_id.to_string(),
        );
        if let Some(t) = tenant_id {
            labels.insert(format!("{}/tenant", RCODER_LABEL_PREFIX), t.to_string());
        }
        if let Some(s) = space_id {
            labels.insert(format!("{}/space", RCODER_LABEL_PREFIX), s.to_string());
        }
        labels
    }


    /// Server-Side Apply 参数：`field_manager=rcoder-app-manager` 标识字段 owner，
    /// `force=true` 允许从其他 manager 接管字段（controller 应总是 force，见 operator-rs）。
    /// 让 create-or-update 自然合一：不存在则创建，存在则按字段级三方合并收敛。
    pub(crate) fn ssa_patch_params() -> PatchParams {
        PatchParams {
            field_manager: Some(APP_MANAGED_BY.to_string()),
            force: true,
            ..Default::default()
        }
    }


    pub(crate) fn pods_api(&self) -> Api<k8s_openapi::api::core::v1::Pod> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }


    pub(crate) fn deployments_api(&self) -> Api<Deployment> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }


    pub(crate) fn services_api(&self) -> Api<Service> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }


    pub(crate) fn configmaps_api(&self) -> Api<ConfigMap> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }


    pub(crate) fn secrets_api(&self) -> Api<k8s_openapi::api::core::v1::Secret> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }
}
