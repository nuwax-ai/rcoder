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
    /// scale Deployment replicas
    pub async fn scale_app(&self, app_id: &str, replicas: i32) -> ContainerRuntimeResult<()> {
        if replicas > 0 {
            self.claim_app_storage(app_id).await?;
        }
        let name = self.app_deployment_name(app_id);
        let patch = serde_json::json!({ "spec": { "replicas": replicas } });
        self.deployments_api()
            .patch(&name, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("scale deployment: {e}")))?;
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
        let patch = serde_json::json!({ "metadata": { "annotations": ann } });
        self.deployments_api()
            .patch(&name, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("patch recycle policy: {e}")))?;
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
        let name = self.app_deployment_name(app_id);
        let annotations =
            std::collections::BTreeMap::from([(WAKE_ON_TRAFFIC_ANNOTATION, enabled.to_string())]);
        let patch = serde_json::json!({ "metadata": { "annotations": annotations } });
        self.deployments_api()
            .patch(&name, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .map_err(|error| {
                ContainerRuntimeError::K8sError(format!("patch wake-on-traffic: {error}"))
            })?;
        Ok(())
    }

    /// 触发滚动重启（rollout annotation）
    pub async fn restart_app(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        self.claim_app_storage(app_id).await?;
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
