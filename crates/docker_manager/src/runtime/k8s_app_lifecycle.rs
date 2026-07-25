//! UserApp Deployment 生命周期(从 k8s_deployment.rs 拆出)。
//!
//! scale/restart + delete_app_resources + cleanup_labeled_orphans/cleanup_orphan_port_resources +
//! wait_for_app_pod_stopped/ignore_404。

#[cfg(feature = "kubernetes")]
use container_runtime_api::{
    ContainerCreateParams, ContainerRuntimeError,
    ContainerRuntimeResult, ExposeType,
};
#[cfg(feature = "kubernetes")]
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams};
#[cfg(feature = "kubernetes")]
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
#[cfg(feature = "kubernetes")]
use tracing::{info, warn};


use super::kubernetes_runtime::KubernetesRuntime;
use super::k8s_deployment::{APP_LABEL_PREFIX, APP_MANAGED_BY, RCODER_LABEL_PREFIX};


impl KubernetesRuntime {


    /// scale Deployment replicas
    pub async fn scale_app(&self, app_id: &str, replicas: i32) -> ContainerRuntimeResult<()> {
        let name = self.app_deployment_name(app_id);
        let patch = serde_json::json!({ "spec": { "replicas": replicas } });
        self.deployments_api()
            .patch(&name, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("scale deployment: {e}")))?;
        info!("[K8S-APP] Deployment {name} scaled to {replicas}");
        Ok(())
    }


    /// 触发滚动重启（rollout annotation）
    pub async fn restart_app(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        let name = self.app_deployment_name(app_id);
        let now = chrono::Utc::now().to_rfc3339();
        let patch = serde_json::json!({
            "spec": { "template": { "metadata": { "annotations": {
                "kubectl.kubernetes.io/restartedAt": now
            } } } }
        });
        self.deployments_api()
            .patch(&name, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("restart deployment: {e}")))?;
        info!("[K8S-APP] Deployment {name} restarted");
        Ok(())
    }


    /// 删除 UserApp 的全部 K8s 资源（Deployment/Service/NodePort/HTTPRoute/ConfigMap/Secret）
    /// 不删 PVC（app 复用 rcoder-workspace 共享 PVC，由 app_manager 在子目录层清理）。
    ///
    /// 任一资源删除返回非 404 错误（如 API Server 不可达、权限拒绝）立即透传，调用方
    /// （app_manager）据此决定是否继续清理工作空间目录，避免"集群资源还在但数据已删"
    /// 的不一致。404 视为已删除（幂等），忽略。
    pub async fn delete_app_resources(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        let dp = DeleteParams::default();
        self.ignore_404(
            self.deployments_api()
                .delete(&self.app_deployment_name(app_id), &dp)
                .await,
        )
        .await?;
        self.ignore_404(
            self.services_api()
                .delete(&self.app_service_name(app_id), &dp)
                .await,
        )
        .await?;
        self.ignore_404(
            self.services_api()
                .delete(&self.app_nodeport_name(app_id), &dp)
                .await,
        )
        .await?;
        self.ignore_404(
            self.configmaps_api()
                .delete(&self.app_config_name(app_id), &dp)
                .await,
        )
        .await?;
        self.ignore_404(
            self.secrets_api()
                .delete(&self.app_secret_name(app_id), &dp)
                .await,
        )
        .await?;
        // HTTPRoute（动态资源）
        let gvk = GroupVersionKind::gvk("gateway.networking.k8s.io", "v1", "HTTPRoute");
        let api_resource = ApiResource::from_gvk(&gvk);
        let routes: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), &self.namespace, &api_resource);
        self.ignore_404(routes.delete(&self.app_http_route_name(app_id), &dp).await)
            .await?;
        // orphan 扫描兜底：删除所有带本 app label 的残留计算资源（防前面按名删除中途
        // 失败留孤儿）。best-effort，list/delete 错误仅 warn，不阻塞删除主流程。不扫 PVC。
        self.cleanup_labeled_orphans(app_id).await;
        // 等 app Pod 容器退出（best-effort，超时不阻塞）。Deployment 已删 → Pod 收到 SIGTERM，
        // 容器秒级退出（不再写文件）。Pod 对象完全消失需等 graceful period（默认 30s），但容器
        // 退出即代表不再写，故等 phase != Running 而非 Pod gone，兼顾安全与速度。
        self.wait_for_app_pod_stopped(app_id, std::time::Duration::from_secs(15))
            .await;
        info!("[K8S-APP] K8s resources deleted for app: {app_id}");
        Ok(())
    }


    /// label 扫描兜底（operator-rs delete_orphaned_resources 思路）：
    /// list 所有带 `instance={app_id}, managed-by=rcoder-app-manager` 的计算资源并删除残留。
    ///
    /// 供 delete_app_resources 末尾调用，保证不留孤儿（哪怕前面按名删除部分失败）。
    /// **不扫 PVC**（共享 PVC 不带 app 标签，且不可删）。best-effort：错误仅 warn。
    async fn cleanup_labeled_orphans(&self, app_id: &str) {
        let selector = format!(
            "{}/instance={app_id},{}/managed-by={APP_MANAGED_BY}",
            APP_LABEL_PREFIX, APP_LABEL_PREFIX
        );
        let lp = ListParams::default().labels(&selector);
        let dp = DeleteParams::default();

        // Deployment
        if let Ok(list) = self.deployments_api().list(&lp).await {
            for it in list.items {
                if let Some(name) = it.metadata.name.as_ref() {
                    let _ = self.deployments_api().delete(name, &dp).await;
                }
            }
        } else {
            warn!("[K8S-APP] orphan 扫描 list deployments 失败 (ignored): {app_id}");
        }
        // Service（含 ClusterIP + NodePort，按 label 一并清）
        if let Ok(list) = self.services_api().list(&lp).await {
            for it in list.items {
                if let Some(name) = it.metadata.name.as_ref() {
                    let _ = self.services_api().delete(name, &dp).await;
                }
            }
        } else {
            warn!("[K8S-APP] orphan 扫描 list services 失败 (ignored): {app_id}");
        }
        // ConfigMap
        if let Ok(list) = self.configmaps_api().list(&lp).await {
            for it in list.items {
                if let Some(name) = it.metadata.name.as_ref() {
                    let _ = self.configmaps_api().delete(name, &dp).await;
                }
            }
        } else {
            warn!("[K8S-APP] orphan 扫描 list configmaps 失败 (ignored): {app_id}");
        }
        // Secret
        if let Ok(list) = self.secrets_api().list(&lp).await {
            for it in list.items {
                if let Some(name) = it.metadata.name.as_ref() {
                    let _ = self.secrets_api().delete(name, &dp).await;
                }
            }
        } else {
            warn!("[K8S-APP] orphan 扫描 list secrets 失败 (ignored): {app_id}");
        }
        // HTTPRoute（动态资源）
        let gvk = GroupVersionKind::gvk("gateway.networking.k8s.io", "v1", "HTTPRoute");
        let api_resource = ApiResource::from_gvk(&gvk);
        let routes: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), &self.namespace, &api_resource);
        if let Ok(list) = routes.list(&lp).await {
            for it in list.items {
                if let Some(name) = it.metadata.name.as_ref() {
                    let _ = routes.delete(name, &dp).await;
                }
            }
        } else {
            warn!("[K8S-APP] orphan 扫描 list httproutes 失败 (ignored): {app_id}");
        }
    }


    /// 清理 update 后不再需要的端口/配置资源（orphan）。
    ///
    /// SSA re-apply 只创建当前 desired 的资源；若 HTTP/TCP 端口被移除、或 env/secrets 被清空，
    /// 旧的 HTTPRoute / NodePort Service / ConfigMap / Secret 会残留。本方法按 desired 状态
    /// 清理这些 orphan（404 视为已删，幂等）。供 patch_deployment 调用。
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
            let gvk = GroupVersionKind::gvk("gateway.networking.k8s.io", "v1", "HTTPRoute");
            let api_resource = ApiResource::from_gvk(&gvk);
            let routes: Api<DynamicObject> =
                Api::namespaced_with(self.client.clone(), &self.namespace, &api_resource);
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
    async fn wait_for_app_pod_stopped(&self, app_id: &str, timeout: std::time::Duration) {
        let lp = ListParams::default().labels(&format!("{}/app-id={app_id}", RCODER_LABEL_PREFIX));
        let start = std::time::Instant::now();
        loop {
            match self.pods_api().list(&lp).await {
                Ok(pods) => {
                    // 仍有 Running 容器（未退出）→ 继续等；否则（全退出/无 Pod）→ 安全清理
                    let still_running = pods.items.iter().any(|p| {
                        p.status
                            .as_ref()
                            .and_then(|s| s.phase.as_deref())
                            .is_some_and(|ph| ph == "Running")
                    });
                    if !still_running {
                        return;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "[K8S-APP] wait_for_app_pod_stopped list 失败，跳过等待: {}",
                        e
                    );
                    return;
                }
            }
            if start.elapsed() >= timeout {
                tracing::warn!(
                    "[K8S-APP] wait_for_app_pod_stopped 超时 {}s，容器可能仍在退出（继续清理）",
                    timeout.as_secs()
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }


    /// 仅容忍 404（视为已删除/幂等），其余 K8s 错误透传
    async fn ignore_404<T>(&self, r: Result<T, kube::Error>) -> ContainerRuntimeResult<()> {
        match r {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
            Err(e) => Err(ContainerRuntimeError::K8sError(format!("delete: {e}"))),
        }
    }
}
