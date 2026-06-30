//! Envoy Gateway Backend CRD 生命周期管理
//!
//! 为每个 agent_runner Pod 创建对应的 Envoy Gateway Backend CRD，
//! 指向 K8s Service 的 FQDN，使 Envoy Gateway 能自动发现和路由到 agent_runner。
//! 使用 kube-rs DynamicObject 操作自定义资源，避免编译时依赖 CRD 定义。

#[cfg(feature = "kubernetes")]
use super::k8s_service::K8sServiceOps;
#[cfg(feature = "kubernetes")]
use super::kubernetes_runtime::KubernetesRuntime;
#[cfg(feature = "kubernetes")]
use async_trait::async_trait;
#[cfg(feature = "kubernetes")]
use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult};
#[cfg(feature = "kubernetes")]
use kube::api::{Api, DeleteParams, DynamicObject, PostParams};
#[cfg(feature = "kubernetes")]
use kube::discovery::ApiResource;
#[cfg(feature = "kubernetes")]
use shared_types::{HTTP_DEFAULT_PORT, ServiceType};
#[cfg(feature = "kubernetes")]
use std::collections::BTreeMap;
#[cfg(feature = "kubernetes")]
use tracing::{debug, info, warn};

/// Backend CRD 的 ApiResource 定义
#[cfg(feature = "kubernetes")]
pub(crate) fn backend_api_resource() -> ApiResource {
    ApiResource {
        group: "gateway.envoyproxy.io".to_string(),
        version: "v1alpha1".to_string(),
        api_version: "gateway.envoyproxy.io/v1alpha1".to_string(),
        kind: "Backend".to_string(),
        plural: "backends".to_string(),
    }
}

/// Backend CRD 生命周期管理 trait extension
#[cfg(feature = "kubernetes")]
#[async_trait]
pub(crate) trait K8sBackendCRDOps {
    /// 生成 Backend CRD 名称
    ///
    /// 格式：`backend-{sanitized_identifier}`
    fn backend_crd_name(&self, identifier: &str) -> String;

    /// 创建 Backend CRD，指向 K8s Service FQDN
    async fn create_backend_crd(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()>;

    /// 删除 Backend CRD
    async fn delete_backend_crd(&self, identifier: &str) -> ContainerRuntimeResult<()>;
}

#[cfg(feature = "kubernetes")]
fn backend_api(client: kube::Client, namespace: &str) -> Api<DynamicObject> {
    Api::namespaced_with(client, namespace, &backend_api_resource())
}

#[cfg(feature = "kubernetes")]
#[async_trait]
impl K8sBackendCRDOps for KubernetesRuntime {
    fn backend_crd_name(&self, identifier: &str) -> String {
        let sanitized = KubernetesRuntime::sanitize_k8s_name_part(identifier);
        format!("backend-{}", sanitized)
    }

    async fn create_backend_crd(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        let crd_name = self.backend_crd_name(identifier);
        let svc_name = self.agent_service_name(identifier, service_type)?;
        let backends = backend_api(self.client.clone(), &self.namespace);

        // 检查是否已存在
        match backends.get(&crd_name).await {
            Ok(_) => {
                debug!("[K8S] Backend CRD {} already exists", crd_name);
                return Ok(());
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {}
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "Failed to check Backend CRD '{}': {}",
                    crd_name, e
                )));
            }
        }

        // 构建 Backend CRD
        let fqdn = format!("{}.{}.svc.cluster.local", svc_name, self.namespace);

        let mut backend = DynamicObject::new(&crd_name, &backend_api_resource());
        backend.metadata.namespace = Some(self.namespace.clone());
        backend.metadata.labels = Some({
            let mut m = BTreeMap::new();
            m.insert("app".to_string(), "rcoder".to_string());
            m.insert("managed-by".to_string(), "rcoder-runtime".to_string());
            m
        });
        backend.data = serde_json::json!({
            "spec": {
                "endpoints": [
                    {
                        "fqdn": {
                            "hostname": fqdn,
                            "port": HTTP_DEFAULT_PORT
                        }
                    }
                ]
            }
        });

        backends
            .create(&PostParams::default(), &backend)
            .await
            .map_err(|e| {
                ContainerRuntimeError::ContainerCreationError(format!(
                    "Failed to create Backend CRD '{}': {}",
                    crd_name, e
                ))
            })?;

        info!("[K8S] Backend CRD {} created → {}", crd_name, fqdn);
        Ok(())
    }

    async fn delete_backend_crd(&self, identifier: &str) -> ContainerRuntimeResult<()> {
        let crd_name = self.backend_crd_name(identifier);
        let backends = backend_api(self.client.clone(), &self.namespace);

        match backends.delete(&crd_name, &DeleteParams::default()).await {
            Ok(_) => {
                info!("[K8S] Backend CRD {} deleted", crd_name);
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                debug!("[K8S] Backend CRD {} not found, already deleted", crd_name);
            }
            Err(e) => {
                warn!("[K8S] Failed to delete Backend CRD '{}': {}", crd_name, e);
            }
        }
        Ok(())
    }
}
