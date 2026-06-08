//! Kubernetes Service 生命周期管理
//!
//! 为每个 agent_runner Pod 创建对应的 K8s ClusterIP Service，
//! 提供稳定的 DNS 名，用于 Envoy Gateway 路由发现。
//! 使用 trait extension 模式为 `KubernetesRuntime` 添加 Service 操作方法。

#[cfg(feature = "kubernetes")]
use async_trait::async_trait;
#[cfg(feature = "kubernetes")]
use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec};
#[cfg(feature = "kubernetes")]
use kube::api::{Api, DeleteParams, ObjectMeta, PostParams};
#[cfg(feature = "kubernetes")]
use shared_types::{HTTP_DEFAULT_PORT, ServiceType};
#[cfg(feature = "kubernetes")]
use std::collections::BTreeMap;
#[cfg(feature = "kubernetes")]
use tracing::{debug, info, warn};

#[cfg(feature = "kubernetes")]
use super::kubernetes_runtime::KubernetesRuntime;
#[cfg(feature = "kubernetes")]
use super::k8s_pod::K8sPodOps;

/// Agent Runner HTTP 端口（使用 shared_types 共享常量）
const AGENT_HTTP_PORT: u32 = shared_types::HTTP_DEFAULT_PORT as u32;

/// K8s Service 生命周期管理 trait extension
///
/// 为 `KubernetesRuntime` 添加 per-pod K8s Service 管理方法：
/// - Service 命名 (`agent_service_name`)
/// - Service 创建 (`create_agent_service`)
/// - Service 删除 (`delete_agent_service`)
#[cfg(feature = "kubernetes")]
#[async_trait]
pub(crate) trait K8sServiceOps {
    /// 生成 agent Service 名称
    ///
    /// 格式：`{pod_name}-svc`（如 `computer-agent-user-123-svc`）
    fn agent_service_name(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String>;

    /// 创建 K8s ClusterIP Service，selector 匹配 agent_runner Pod
    ///
    /// Service 暴露 HTTP 8086 端口，selector 使用与 Pod 相同的 labels
    /// （`managed-by=rcoder-runtime` + identifier label）。
    /// 创建前先检查是否已存在，已存在则跳过。
    async fn create_agent_service(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()>;

    /// 删除 agent Service
    ///
    /// 在 Pod 终止后调用。404 视为已删除，不报错。
    async fn delete_agent_service(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()>;
}

#[cfg(feature = "kubernetes")]
#[async_trait]
impl K8sServiceOps for KubernetesRuntime {
    fn agent_service_name(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        let pod_name = self.pod_name(identifier, service_type)?;
        Ok(format!("{}-svc", pod_name))
    }

    async fn create_agent_service(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        let svc_name = self.agent_service_name(identifier, service_type)?;
        let services: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);

        // 检查是否已存在
        match services.get(&svc_name).await {
            Ok(_) => {
                debug!("[K8S] Service {} already exists", svc_name);
                return Ok(());
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {}
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "Failed to check Service '{}': {}",
                    svc_name, e
                )));
            }
        }

        // 构建 selector labels（与 Pod labels 一致）
        let identifier_label_key = match service_type {
            ServiceType::ComputerAgentRunner => "user_id",
            ServiceType::RCoder => "project_id",
        };

        let mut selector = BTreeMap::new();
        selector.insert("managed-by".to_string(), "rcoder-runtime".to_string());
        // identifier 直接作为 label value（K8s labels 允许下划线）
        // 必须与 Pod labels 中的值一致（kubernetes_runtime.rs:374-378 使用原始 identifier）
        selector.insert(identifier_label_key.to_string(), identifier.to_string());

        let service = Service {
            metadata: ObjectMeta {
                name: Some(svc_name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some({
                    let mut m = BTreeMap::new();
                    m.insert("app".to_string(), "rcoder".to_string());
                    m.insert("managed-by".to_string(), "rcoder-runtime".to_string());
                    m.insert("service_type".to_string(), service_type.to_string());
                    m
                }),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                type_: Some("ClusterIP".to_string()),
                selector: Some(selector),
                ports: Some(vec![ServicePort {
                    name: Some("http".to_string()),
                    port: AGENT_HTTP_PORT as i32,
                    target_port: Some(k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(
                        AGENT_HTTP_PORT as i32,
                    )),
                    protocol: Some("TCP".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            status: None,
        };

        services
            .create(&PostParams::default(), &service)
            .await
            .map_err(|e| {
                ContainerRuntimeError::ContainerCreationError(format!(
                    "Failed to create Service '{}': {}",
                    svc_name, e
                ))
            })?;

        info!(
            "[K8S] Service {} created for {} ({})",
            svc_name, identifier, service_type
        );
        Ok(())
    }

    async fn delete_agent_service(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        let svc_name = self.agent_service_name(identifier, service_type)?;
        let services: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);

        match services.delete(&svc_name, &DeleteParams::default()).await {
            Ok(_) => {
                info!("[K8S] Service {} deleted", svc_name);
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                debug!("[K8S] Service {} not found, already deleted", svc_name);
            }
            Err(e) => {
                warn!("[K8S] Failed to delete Service '{}': {}", svc_name, e);
            }
        }
        Ok(())
    }
}
